//! The node editor widget: a canvas, the operators' driver, the selection overlay and an
//! optional statistics overlay ([`NodeEditor::with_hud`]).
//!
//! ## The driver
//!
//! With [`NodeEditor::with_ops`] the widget runs operators from three seats, because no
//! single one does the whole job:
//!
//! * [`Layer::capture_pointer_event`] — every pointer event, before the tree and outside this widget too; it can keep
//!   an event from the tree while an operator owns the gesture.
//! * [`Widget::on_pointer_event`] — the bubbled route: widgets in nodes refuse first, and pointer capture can be taken
//!   here (Masonry allows it only during a press).
//! * [`Widget::on_text_event`] — keys, as the focus fallback (`RenderRoot::set_focus_fallback`).
//!
//! Operators change the model; the driver carries the changes into this canvas and into
//! the other views of the same graph.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::mem;
use std::rc::Rc;

use blazy_canvas::{CanvasHit, CanvasLayer, CanvasStats, PortSide, wheel_pixels};
use blazy_ops::event::{Device, OpEvent, Sample};
use blazy_ops::keymap::{Props, Scope};
use blazy_ops::runtime::{Dispatch, OpRuntime, Seat};
use blazy_ops::{OpCounters, OpResult};
use masonry::accesskit::{Node as AccessNode, Role};
use masonry::core::keyboard::{KeyState, Modifiers};
use masonry::core::{
    AccessCtx, BrushIndex, ChildrenIds, CursorIcon, EventCtx, Handled, Layer, LayerType, LayoutCtx, MeasureCtx,
    NewWidget, NoAction, PaintCtx, PointerEvent, PropertiesMut, PropertiesRef, QueryCtx, RegisterCtx, StyleProperty,
    TextEvent, Update, UpdateCtx, Widget, WidgetId, WidgetMut, WidgetPod, render_text,
};
use masonry::dpi::PhysicalPosition;
use masonry::imaging::Painter;
use masonry::kurbo::{Affine, Axis, Circle, Point, Rect, Size, Stroke, Vec2};
use masonry::layout::{LenReq, Length, SizeDef};
use masonry::parley::Layout;
use masonry::peniko::Color;
use masonry::ui_events::pointer::{PointerScrollEvent, PointerType, PointerUpdate};
use masonry::{TextAlign, TextAlignOptions};

use crate::edge_pan::{EdgePan, EdgePanCounters, EdgePanState};
use crate::menu::MenuLayer;
use crate::ops::CANVAS_SCOPE;
use crate::{Change, EDGE_PAN, Edit, EditorWorld, MoveRecorder, NodeGraph, SharedGraph};

/// What an area keeps about its view of a graph: the view, the operators, the sidebar.
///
/// Kept beside the widget rather than in it, so that an area rebuilt elsewhere (detached
/// into another window, read from a workspace) keeps it.
pub struct EditorSession<G: NodeGraph> {
    /// Everything the operators may touch, selection included.
    pub(crate) world: EditorWorld<G>,
    /// The operators, their keymap, the modal stack and the undo history.
    pub(crate) runtime: OpRuntime<EditorWorld<G>>,
    /// Where the canvas is looking, mirrored from it on every layout.
    pub(crate) view: Affine,
    /// The editor widget showing this session, once it is in a tree; set by the editor.
    pub(crate) editor: Option<WidgetId>,
    /// How the view scrolls at the area's edge during a gesture whose binding asks for it
    /// ([`EDGE_PAN`](crate::EDGE_PAN)); `None` (the default) for not at all.
    pub(crate) edge_pan: Option<EdgePan>,
    /// The area's sidebar: shown or not, and how wide. Shared with the area's widget.
    pub(crate) sidebar: blazy_areas::SidebarState,
}

impl<G: NodeGraph> EditorSession<G> {
    /// The world the area's operators act on: the graph, the pointer, the selection.
    pub fn world(&self) -> &EditorWorld<G> {
        &self.world
    }

    /// The world, for a driver to give the operators the screen's snapshot and the clipboard,
    /// and to take their requests.
    pub fn world_mut(&mut self) -> &mut EditorWorld<G> {
        &mut self.world
    }

    /// The graph the area shows.
    pub fn graph(&self) -> &SharedGraph<G> {
        &self.world.graph
    }

    /// The area's operators, keymap and history, to read.
    pub fn runtime(&self) -> &OpRuntime<EditorWorld<G>> {
        &self.runtime
    }

    /// The area's view: pan and zoom.
    pub fn view(&self) -> Affine {
        self.view
    }

    /// The editor widget showing the session, once it is in a tree.
    pub fn editor(&self) -> Option<WidgetId> {
        self.editor
    }

    /// Edge pan for gestures held at the area's border, if the application asked for it.
    pub fn edge_pan(&self) -> Option<EdgePan> {
        self.edge_pan
    }

    /// Asks for edge pan, or turns it off.
    pub fn set_edge_pan(&mut self, edge_pan: Option<EdgePan>) {
        self.edge_pan = edge_pan;
    }

    /// The area's sidebar.
    pub fn sidebar(&self) -> &blazy_areas::SidebarState {
        &self.sidebar
    }

    /// Hands `event` to the runtime, for a driver of its own.
    pub fn dispatch(&mut self, event: &OpEvent, scope: Scope<'_>, seat: Seat) -> Dispatch {
        self.runtime.dispatch(&mut self.world, event, scope, seat)
    }

    /// Ends whatever gesture is running, cleaning up after it.
    pub fn cancel_all(&mut self) {
        self.runtime.cancel_all(&mut self.world);
    }

    /// Records an edit of node `index`'s content made past the operators (in a widget) as
    /// one undo step. The edit is already in the model; call this once, when it ends.
    pub fn record_content(&mut self, index: usize, before: G::Content, after: G::Content) {
        self.runtime
            .history_mut()
            .push(Box::new(crate::world::ContentStep { index, before, after }));
    }

    /// A session over `graph`, with [`ops::runtime`](crate::ops::runtime)'s operators.
    pub fn new(graph: &SharedGraph<G>) -> Self {
        Self::with_runtime(graph, crate::ops::runtime())
    }

    /// The same, with a runtime of the caller's own.
    ///
    /// Every session over one graph shares the graph's history, as Blender keeps one per
    /// file: separate histories could redo a node into a name another area has taken.
    pub fn with_runtime(graph: &SharedGraph<G>, mut runtime: OpRuntime<EditorWorld<G>>) -> Self {
        let views = graph.borrow().views().clone();
        match views.history::<EditorWorld<G>>() {
            Some(history) => runtime.share_history(history),
            None => views.set_history(runtime.shared_history()),
        }
        Self {
            world: EditorWorld::new(graph),
            runtime,
            view: Affine::IDENTITY,
            editor: None,
            edge_pan: None,
            sidebar: Rc::default(),
        }
    }

    /// The same session, with `edge_pan`.
    #[must_use]
    pub fn with_edge_pan(mut self, edge_pan: Option<EdgePan>) -> Self {
        self.edge_pan = edge_pan;
        self
    }

    /// The edge pan the running operator scrolls by, if its binding asked for it.
    fn edge_pan_in_force(&self) -> Option<EdgePan> {
        let asked = self
            .runtime
            .modal_props()
            .is_some_and(|props| props.bool(EDGE_PAN, false));
        self.edge_pan.filter(|_| asked)
    }

    /// Wraps the session in the handle an editor takes.
    pub fn share(self) -> SessionHandle<G> {
        Rc::new(RefCell::new(self))
    }
}

/// A session as it is held: by the area that owns it and by the widget that shows it.
pub type SessionHandle<G> = Rc<RefCell<EditorSession<G>>>;

/// The colours the editor draws with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayStyle {
    /// Behind the canvas, where no node is.
    pub background: Color,
    /// The outline of a selected node, and of the rubber band.
    pub selection: Color,
    /// The inside of the rubber band.
    pub band_fill: Color,
    /// A lit port: under the pointer, the one a link is drawn from, or the one it would
    /// connect to.
    pub port_hover: Color,
    /// A link dragged onto a port that refuses it.
    pub refused: Color,
    /// Where the editor outlines selected nodes itself.
    pub outline: SelectionOutline,
}

/// Where the editor outlines selected nodes itself. Every selected node also wears the
/// [`SELECTED`](crate::SELECTED) class, for an application to style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(clippy::exhaustive_enums, reason = "configuration, matched by the caller")]
pub enum SelectionOutline {
    /// Round every selected node on screen. The default.
    #[default]
    Always,
    /// Only where nodes have no widgets (the far field), which no class reaches.
    FarField,
}

impl Default for OverlayStyle {
    fn default() -> Self {
        Self {
            background: Color::from_rgb8(0x1c, 0x1c, 0x20),
            selection: Color::from_rgb8(0xff, 0xa5, 0x2c),
            outline: SelectionOutline::Always,
            band_fill: Color::from_rgba8(0xff, 0xa5, 0x2c, 0x20),
            refused: Color::from_rgb8(0xe0, 0x40, 0x40),
            port_hover: Color::from_rgb8(0xd8, 0xd8, 0xe4),
        }
    }
}

/// Renders a canvas, drives the operators, and draws the selection over it.
pub struct NodeEditor<G: NodeGraph> {
    canvas: WidgetPod<CanvasLayer>,
    /// The operator layer, or `None` for an editor with the canvas's own gestures.
    session: Option<SessionHandle<G>>,
    /// How the overlays are drawn.
    style: OverlayStyle,
    /// The last line of the statistics overlay, or `None` for no overlay at all.
    hud_caption: Option<String>,
    /// The canvas's view transform as of the last layout, for `post_paint`, which cannot
    /// reach into a child.
    view: Affine,
    /// Stats cached during layout, so `post_paint` draws numbers from this frame.
    stats: CanvasStats,
    /// The HUD text currently on screen.
    hud: String,
    /// Scratch buffer the next HUD text is formatted into, reused between frames.
    hud_next: String,
    /// The HUD text, shaped.
    /// Reshaped only when [`Self::hud`] changes: shaping costs more than the rest of a frame.
    hud_layout: Option<Layout<BrushIndex>>,
    /// The nodes this editor's canvas has been told are selected, so a change of selection
    /// touches only the nodes that changed.
    shown: BTreeSet<usize>,
    /// The graph's generation this editor last saw; a newer one means the graph was replaced.
    generation: u64,
    /// The menu this editor opened and has not seen closed, by its layer's root.
    menu: Option<WidgetId>,
    /// What the pointer was over when the press now held was made.
    press_hover: Option<CanvasHit>,
    /// The port under the pointer a drag would start a link from, lit over the canvas.
    port_hint: Option<(usize, PortSide, u16)>,
    /// The node edge under the pointer a drag would resize the node by.
    edge_hint: Option<(usize, crate::ops::NodeEdge)>,
    /// The modifiers held as of the last event (`Ctrl` changes what a port drag does).
    mods: Modifiers,
    /// The canvas's hover predates a gesture it did not see; trusted again from the next move.
    hover_stale: bool,
    /// The running gesture's edge pan, and what edge pan has done here.
    edge: EdgePanState,
}

impl<G: NodeGraph> NodeEditor<G> {
    /// The canvas this editor drives; its id is what the graph's views are named by.
    pub fn canvas_id(&self) -> WidgetId {
        self.canvas.id()
    }

    /// Statistics from the canvas, as of the last layout pass.
    pub fn stats(&self) -> CanvasStats {
        self.stats
    }

    /// Runs a callback with a `WidgetMut` for the inner canvas.
    pub fn with_canvas<R>(this: &mut WidgetMut<'_, Self>, f: impl FnOnce(WidgetMut<'_, CanvasLayer>) -> R) -> R {
        let canvas = this.ctx.get_mut(&mut this.widget.canvas);
        f(canvas)
    }

    /// Wraps a canvas in an editor with no operator layer and no statistics overlay.
    pub fn new(canvas: CanvasLayer) -> Self {
        Self {
            canvas: WidgetPod::new(canvas),
            session: None,
            style: OverlayStyle::default(),
            hud_caption: None,
            view: Affine::IDENTITY,
            stats: CanvasStats::default(),
            hud: String::new(),
            hud_next: String::new(),
            hud_layout: None,
            shown: BTreeSet::new(),
            generation: 0,
            menu: None,
            press_hover: None,
            port_hint: None,
            edge_hint: None,
            mods: Modifiers::empty(),
            hover_stale: false,
            edge: EdgePanState::default(),
        }
    }

    /// The same editor, with operators driving the pointer and the keyboard.
    ///
    /// Turns the canvas's own gestures off ([`CanvasLayer::with_builtin_gestures`]), or it
    /// would answer a press before the keymap heard of it; the canvas still picks on every
    /// pointer event, which is what the operators poll on.
    pub fn with_ops(canvas: CanvasLayer, graph: &SharedGraph<G>) -> Self {
        Self::with_session(canvas, EditorSession::new(graph).share())
    }

    /// As [`with_ops`](Self::with_ops), with a runtime of the caller's own.
    pub fn with_runtime(canvas: CanvasLayer, graph: &SharedGraph<G>, runtime: OpRuntime<EditorWorld<G>>) -> Self {
        Self::with_session(canvas, EditorSession::with_runtime(graph, runtime).share())
    }

    /// An editor over a session that already exists (an area rebuilt elsewhere); the canvas
    /// opens at the session's view.
    pub fn with_session(canvas: CanvasLayer, session: SessionHandle<G>) -> Self {
        // The wheel too: `view.zoom` is bound on it, and the canvas below would answer
        // first and mark it handled.
        let canvas = canvas
            .with_builtin_gestures(false)
            .with_wheel_zoom(false)
            .with_view(session.borrow().view);
        // Where the ports are, for a link drag whose fixed end the pointer is not on.
        session.borrow_mut().world.ports = canvas.ports();
        Self {
            session: Some(session),
            ..Self::new(canvas)
        }
    }

    /// The session this editor shows, if it has one.
    pub fn session(&self) -> Option<&SessionHandle<G>> {
        self.session.as_ref()
    }

    /// The same editor, drawing a statistics overlay whose last line is `caption`.
    #[must_use]
    pub fn with_hud(mut self, caption: impl Into<String>) -> Self {
        self.hud_caption = Some(caption.into());
        self
    }

    /// The same editor, drawn in other colours.
    #[must_use]
    pub fn with_style(mut self, style: OverlayStyle) -> Self {
        self.style = style;
        self
    }

    /// The selected nodes. Empty for an editor with no operator layer.
    pub fn selection(&self) -> BTreeSet<usize> {
        self.session
            .as_ref()
            .map(|session| session.borrow().world.selection())
            .unwrap_or_default()
    }

    /// The operator counters, all zero when there is no operator layer.
    pub fn op_counters(&self) -> OpCounters {
        self.session
            .as_ref()
            .map(|session| session.borrow().runtime.counters())
            .unwrap_or_default()
    }

    /// How many operators are running; zero between gestures.
    pub fn modal_depth(&self) -> usize {
        self.session
            .as_ref()
            .map_or(0, |session| session.borrow().runtime.modal_depth())
    }

    /// The status line as it was last painted. Empty without [`with_hud`](Self::with_hud).
    pub fn hud(&self) -> &str {
        &self.hud
    }

    /// Whether a press is held while the runtime waits to see what it becomes.
    pub fn is_holding(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.borrow().runtime.is_holding())
    }

    /// Steps in the undo history.
    pub fn history_depth(&self) -> usize {
        self.session
            .as_ref()
            .map_or(0, |session| session.borrow().runtime.history().depth())
    }

    /// Bytes the undo history is holding, by its steps' own reckoning.
    pub fn history_bytes(&self) -> usize {
        self.session
            .as_ref()
            .map_or(0, |session| session.borrow().runtime.history().bytes())
    }

    /// What edge pan has done in this editor. See [`EdgePanCounters`].
    pub fn edge_pan_counters(&self) -> EdgePanCounters {
        self.edge.counters
    }

    /// Runs an operator by name (a menu, a script, a test), through the same poll and history
    /// as a key, and carries its changes into the views.
    pub fn exec(this: &mut WidgetMut<'_, Self>, name: &str, props: &Props) -> OpResult {
        let Some(session) = this.widget.session.clone() else {
            return OpResult::PassThrough;
        };
        // The borrow ends before the flush, which takes its own.
        let result = {
            let session = &mut *session.borrow_mut();
            session.runtime.exec(&mut session.world, name, props)
        };
        Self::flush_mut(this);
        result
    }

    /// Hands a pointer event to the operator running here, from the host's seat in front of
    /// the tree, wherever the pointer is.
    ///
    /// For an operator started by a key, which has no pointer capture (Masonry grants it
    /// only during a press). Positions are in window coordinates. Returns whether the
    /// operator consumed the event.
    pub fn host_pointer_event(this: &mut WidgetMut<'_, Self>, event: &PointerEvent) -> bool {
        if this.widget.session.is_none() {
            return false;
        }
        let scale = this.ctx.scale_factor();
        let size = this.ctx.content_box().size();
        let converted = {
            let ctx = &this.ctx;
            to_op_event_in(
                |p| {
                    let p = p.to_logical::<f64>(scale);
                    ctx.to_local(Point::new(p.x, p.y))
                },
                scale,
                size,
                event,
            )
        };
        let Some((op_event, sample)) = converted else {
            return false;
        };
        let tracking = this
            .widget
            .session
            .as_ref()
            .is_some_and(|session| session.borrow().world.track_hover);
        let (view, hover) = {
            let mut canvas = this.ctx.get_mut(&mut this.widget.canvas);
            let hover = match op_event.pos() {
                Some(pos) if tracking => CanvasLayer::hit_test(&mut canvas, pos),
                _ => canvas.widget.stats().hovered,
            };
            (canvas.widget.view(), hover)
        };
        let consumed = this.widget.feed(&op_event, sample, Seat::Host, view, hover);
        if this.widget.watch_edge_pan(Some(&op_event), size) {
            this.ctx.request_anim_frame();
        }
        Self::flush_mut(this);
        consumed
    }

    /// Follows the running gesture's pointer for edge pan, and says whether to ask for an
    /// animation frame. A gesture cancelled after scrolling puts the view back, as Blender's
    /// does.
    fn watch_edge_pan(&mut self, event: Option<&OpEvent>, size: Size) -> bool {
        let Some(session) = self.session.clone() else {
            return false;
        };
        let (config, cancels) = {
            let session = session.borrow();
            (session.edge_pan_in_force(), session.runtime.counters().modal_cancels)
        };
        let Some(config) = config else {
            let edge = &mut self.edge;
            if edge.gesture {
                edge.gesture = false;
                if cancels > edge.cancels && edge.total != Vec2::ZERO {
                    session.borrow_mut().world.pan -= edge.total;
                    edge.counters.restores += 1;
                }
                edge.total = Vec2::ZERO;
            }
            return false;
        };
        if !self.edge.gesture {
            self.edge.begin(cancels);
        }
        let pos = event.and_then(OpEvent::pos);
        if let (Some(event), Some(_)) = (event, pos) {
            self.edge.mods = event.mods();
        }
        if !self.edge.track(&config, pos, size) || self.edge.frame_pending {
            return false;
        }
        self.edge.frame_pending = true;
        self.edge.counters.frames += 1;
        true
    }

    /// One animation frame of edge pan: moves the view, and hands the operator a move at
    /// the same screen position.
    fn edge_pan_frame(this: &mut WidgetMut<'_, Self>, interval_ns: u64) {
        let Some(session) = this.widget.session.clone() else {
            return;
        };
        let config = session.borrow().edge_pan_in_force();
        // The gesture ended between the request and the frame.
        let (Some(config), Some(pointer)) = (config, this.widget.edge.pointer) else {
            return;
        };
        if !this.widget.edge.gesture {
            return;
        }
        let size = this.ctx.content_box().size();
        let tracking = session.borrow().world.track_hover;
        let (delta, view, hover) = {
            let mut canvas = this.ctx.get_mut(&mut this.widget.canvas);
            let delta = this.widget.edge.step(&config, size, canvas.widget.zoom(), interval_ns);
            if delta != Vec2::ZERO {
                CanvasLayer::pan(&mut canvas, delta);
            }
            let hover = if tracking {
                CanvasLayer::hit_test(&mut canvas, pointer)
            } else {
                canvas.widget.stats().hovered
            };
            (delta, canvas.widget.view(), hover)
        };
        if delta != Vec2::ZERO {
            this.widget.edge.total += delta;
            this.widget.edge.counters.pans += 1;
            let event = OpEvent::Move {
                pos: pointer,
                mods: this.widget.edge.mods,
            };
            // `Tree`: nothing below was offered this move, because there was no move.
            this.widget.feed(&event, Sample::at(pointer), Seat::Tree, view, hover);
            Self::flush_mut(this);
        }
        if this.widget.edge.in_band(&config, size) {
            this.widget.edge.frame_pending = true;
            this.widget.edge.counters.frames += 1;
            this.ctx.request_anim_frame();
        }
    }

    /// The menu this editor has open, by its layer's root, if any.
    pub fn open_menu(&self) -> Option<WidgetId> {
        self.menu
    }

    /// Hears that its menu closed. Called by the menu.
    pub(crate) fn menu_closed(this: &mut WidgetMut<'_, Self>) {
        this.widget.menu = None;
    }

    /// Sets how a finished move becomes an undo step.
    pub fn set_move_recorder(this: &mut WidgetMut<'_, Self>, recorder: MoveRecorder<G>) {
        if let Some(session) = this.widget.session.as_ref() {
            session.borrow_mut().world.record_move = recorder;
        }
    }

    /// Brings the [`SELECTED`](crate::SELECTED) class on this canvas's nodes in line with the
    /// graph's selection, touching only the nodes whose selection changed. Every editor over
    /// the graph does this for its own canvas.
    fn sync_selection(this: &mut WidgetMut<'_, Self>) {
        let Some(session) = this.widget.session.clone() else {
            return;
        };
        let selection = session.borrow().world.selection();
        if selection == this.widget.shown {
            return;
        }
        let gone: Vec<usize> = this.widget.shown.difference(&selection).copied().collect();
        let came: Vec<usize> = selection.difference(&this.widget.shown).copied().collect();
        {
            let mut canvas = this.ctx.get_mut(&mut this.widget.canvas);
            for index in gone {
                CanvasLayer::set_node_class(&mut canvas, index, crate::SELECTED, false);
            }
            // Raising is the registry's, once for every view.
            for index in came {
                CanvasLayer::set_node_class(&mut canvas, index, crate::SELECTED, true);
            }
        }
        this.widget.shown = selection;
        // The outline is this editor's own paint.
        this.ctx.request_post_paint();
    }

    /// The menu an operator asked for, as a layer. Each entry is polled, so it shows as
    /// available exactly what would run, and labelled with its key binding if it has one.
    fn take_menu(
        &self,
        target: WidgetId,
        to_window: impl FnOnce(Point) -> Point,
    ) -> Option<(NewWidget<MenuLayer<G>>, Point)> {
        let session = self.session.as_ref()?;
        let session = &mut *session.borrow_mut();
        let menu = session.world.menu.take()?;
        let entries = menu
            .items
            .iter()
            .map(|item| crate::menu::Entry {
                label: item.label.clone(),
                op: item.op.clone(),
                props: item.props.clone(),
                enabled: session
                    .runtime
                    .poll(&mut session.world, &item.op, &item.props)
                    .unwrap_or(false),
                shortcut: session
                    .runtime
                    .keymap()
                    .binding_for(&item.op, &item.props)
                    .map(|binding| binding.pattern.label()),
            })
            .collect();
        let at = to_window(session.world.pointer_screen);
        Some((NewWidget::new(MenuLayer::new(&menu.title, entries, target, at)), at))
    }

    /// Whether the session's selection is one this editor's nodes have not been told about.
    fn selection_stale(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.borrow().world.selection() != self.shown)
    }

    /// Applies what the operators changed, from a `WidgetMut`; the twin of
    /// [`flush`](Self::flush).
    fn flush_mut(this: &mut WidgetMut<'_, Self>) {
        let Some(session) = this.widget.session.clone() else {
            return;
        };
        let changes = session.borrow_mut().take_changes();
        if changes.dirty {
            this.ctx.request_post_paint();
        }
        let editor = this.ctx.widget_id();
        let origin = this.ctx.to_window(Point::ORIGIN);
        if let Some((menu, at)) = this.widget.take_menu(editor, |p| p + origin.to_vec2()) {
            this.widget.menu = Some(menu.id());
            this.ctx.create_layer(LayerType::Other, menu, at);
        }
        if !changes.is_empty() {
            let mut canvas = this.ctx.get_mut(&mut this.widget.canvas);
            apply_edits(&mut canvas, &changes.edits);
            for &(index, pos) in &changes.positions {
                CanvasLayer::move_child(&mut canvas, index, pos);
            }
            if changes.pan != Vec2::ZERO {
                CanvasLayer::pan(&mut canvas, changes.pan);
            }
            for &(origin, factor) in &changes.zoom {
                CanvasLayer::zoom_around(&mut canvas, origin, factor);
            }
        }
        // After the edits: a node added and selected in one step must exist before its class.
        Self::sync_selection(this);
        if !changes.concerns_the_graph() {
            return;
        }
        let (peers, announced) = fan_out(&session.borrow(), this.widget.canvas.id(), &changes);
        if !announced.is_empty() {
            let mut canvas = this.ctx.get_mut(&mut this.widget.canvas);
            for &change in &announced {
                crate::views::apply(&mut canvas, change);
            }
        }
        for peer in peers {
            let (positions, edits, announced) = (changes.positions.clone(), changes.edits.clone(), announced.clone());
            this.ctx
                .mutate_later(peer, move |widget| apply_to_peer(widget, positions, edits, announced));
        }
        // Panels in this window; another window's refresh on its own pull.
        for (watcher, refresh) in session.borrow().world.graph.borrow().views().owed_watchers() {
            this.ctx.mutate_later(watcher, move |mut widget| refresh(&mut widget));
        }
    }
}

/// What the operators changed and the driver has not carried into the tree yet.
struct Changes {
    positions: Vec<(usize, Point)>,
    /// Structure changes, in order: a link to a node added in the same step comes after it.
    edits: Vec<Edit>,
    pan: Vec2,
    zoom: Vec<(Point, f64)>,
    dirty: bool,
    /// The nodes that came into the selection, when it changed at all.
    selected: Option<Vec<usize>>,
}

impl Changes {
    /// Whether anything has to reach the canvas. A repaint is not "something": it is
    /// asked for before this is consulted.
    fn is_empty(&self) -> bool {
        self.positions.is_empty()
            && self.edits.is_empty()
            && self.pan == Vec2::ZERO
            && self.zoom.is_empty()
            && self.selected.is_none()
    }

    /// Whether the other views of the graph have anything to hear. A pan or a zoom is this
    /// view's business alone: the view is not model state, so the other views of the graph
    /// are not following it.
    fn concerns_the_graph(&self) -> bool {
        !self.positions.is_empty() || !self.edits.is_empty() || self.selected.is_some()
    }
}

impl<G: NodeGraph> EditorSession<G> {
    /// Takes what the operators changed, leaving the world clean.
    fn take_changes(&mut self) -> Changes {
        let moved = mem::take(&mut self.world.moved);
        let selected = self.world.selection_before.take().and_then(|before| {
            let now = self.world.selection();
            (now != before).then(|| now.difference(&before).copied().collect())
        });
        Changes {
            selected,
            positions: positions_of(&self.world, moved),
            edits: mem::take(&mut self.world.edits),
            pan: mem::replace(&mut self.world.pan, Vec2::ZERO),
            zoom: mem::take(&mut self.world.zoom),
            dirty: mem::take(&mut self.world.dirty),
        }
    }
}

/// Applies structural edits to one canvas.
///
/// Node names are the model's, so every view files the node under the same name and the
/// two stay in step. Link names are not: a canvas hands out its own, so a link is named
/// here by its ends and looked up in each canvas — which is what `CanvasHit::Link`
/// carries both of for.
fn apply_edits(canvas: &mut WidgetMut<'_, CanvasLayer>, edits: &[Edit]) {
    for &edit in edits {
        apply_edit(canvas, edit);
    }
}

/// Applies one structural edit, so that applying it twice changes nothing.
///
/// Applying an edit twice is normal (the push and then the pull deliver it), so a link is
/// looked up before it is inserted: the canvas would file it twice.
pub(crate) fn apply_edit(canvas: &mut WidgetMut<'_, CanvasLayer>, edit: Edit) {
    match edit {
        Edit::NodeAdded { index, rect } => CanvasLayer::insert_node(canvas, index, rect.origin(), rect.size()),
        Edit::NodeRemoved { index } => {
            CanvasLayer::remove_node(canvas, index);
        },
        Edit::NodeResized { index, rect } => CanvasLayer::set_child_rect(canvas, index, rect),
        Edit::ContentChanged { index } => {
            CanvasLayer::refresh_node(canvas, index);
        },
        Edit::LinkAdded(link) => {
            if CanvasLayer::link_name(canvas, link).is_none() {
                CanvasLayer::insert_link(canvas, link);
            }
        },
        Edit::LinkRemoved(link) => {
            if let Some(name) = CanvasLayer::link_name(canvas, link) {
                CanvasLayer::remove_link(canvas, name);
            }
        },
    }
}

/// Moves nodes in another view of the same graph.
fn apply_moves(mut widget: WidgetMut<'_, dyn Widget>, positions: Vec<(usize, Point)>, edits: Vec<Edit>) {
    let mut canvas = widget.downcast::<CanvasLayer>();
    apply_edits(&mut canvas, &edits);
    for (index, pos) in positions {
        CanvasLayer::move_child(&mut canvas, index, pos);
    }
}

/// What a view of the graph in this window is pushed: the moves and edits, then the
/// selection changes.
fn apply_to_peer(
    mut widget: WidgetMut<'_, dyn Widget>,
    positions: Vec<(usize, Point)>,
    edits: Vec<Edit>,
    announced: Vec<Change>,
) {
    let mut canvas = widget.downcast::<CanvasLayer>();
    apply_edits(&mut canvas, &edits);
    for (index, pos) in positions {
        CanvasLayer::move_child(&mut canvas, index, pos);
    }
    for change in announced {
        crate::views::apply(&mut canvas, change);
    }
}

/// The current position of every node in `moved`, deduplicated.
fn positions_of<G: NodeGraph>(world: &EditorWorld<G>, mut moved: Vec<usize>) -> Vec<(usize, Point)> {
    if moved.is_empty() {
        return Vec::new();
    }
    moved.sort_unstable();
    moved.dedup();
    let graph = world.graph.borrow();
    moved
        .into_iter()
        .map(|index| (index, graph.node_rect(index).origin()))
        .collect()
}

/// Records what the operators changed in the graph's [`Views`](crate::Views) for every
/// other view, and returns the views to push to in this frame.
fn fan_out<G: NodeGraph>(
    session: &EditorSession<G>,
    own_canvas: WidgetId,
    changes: &Changes,
) -> (Vec<WidgetId>, Vec<Change>) {
    let graph = session.world.graph.borrow();
    let views = graph.views();
    for &edit in &changes.edits {
        views.note_except(own_canvas, Change::Structure(edit));
    }
    for &(index, pos) in &changes.positions {
        views.note_except(own_canvas, Change::Moved { index, pos });
    }
    // After the edits: a node added and selected in one gesture must exist to be raised.
    let announced = changes
        .selected
        .as_deref()
        .map(|came| views.announce_selection(came))
        .unwrap_or_default();
    let mut peers = Vec::new();
    views.others(own_canvas, &mut peers);
    (peers, announced)
}

/// Formats the HUD into `out`, reusing its allocation.
///
/// Only what a human watches: the first line changes rarely, so the shaped text stays
/// cached. The second line counts recognised gestures, which shows in a real window
/// whether the platform's timestamps make double clicks work.
fn format_hud(stats: &CanvasStats, ops: &OpCounters, caption: &str, out: &mut String) {
    let detail = match stats.detail {
        Some(detail) => detail.as_str(),
        None => "-",
    };
    out.clear();
    write!(
        out,
        "nodes {visible}/{total} materialised   zoom {zoom:.2}x   lod {detail}   built {builds}\n\
         clicks {clicks} (double {doubles})   drags {drags}   held from the tree {withheld}\n\
         {caption}",
        visible = stats.materialised,
        total = stats.total,
        zoom = stats.zoom,
        builds = stats.counters.builds,
        clicks = ops.clicks,
        doubles = ops.double_clicks,
        drags = ops.drags,
        withheld = ops.withheld,
    )
    .ok();
}

/// The pre-tree seat: every pointer event, before the target is computed and outside this
/// widget's rectangle too. Returns whether the event is kept from the tree.
impl<G: NodeGraph> Layer for NodeEditor<G> {
    fn capture_pointer_event(
        &mut self,
        ctx: &mut EventCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        event: &PointerEvent,
    ) -> Handled {
        let Some((op_event, sample)) = to_op_event(ctx, event) else {
            return Handled::No;
        };
        if self.session.is_none() {
            return Handled::No;
        }
        // Somebody below holds the pointer: the event goes to the tree anyway, and
        // dispatching here too would deliver it twice.
        if ctx.pointer_capture_target_id().is_some_and(|id| id != ctx.widget_id()) {
            if let Some(session) = self.session.as_ref() {
                session.borrow_mut().runtime.observe(&op_event);
            }
            return Handled::No;
        }
        // The runtime acts from this seat only when it owns the gesture (a modal operator,
        // or a held press); otherwise the tree has the first refusal.
        if self.dispatch(ctx, &op_event, sample, Seat::Layer) {
            Handled::Yes
        } else {
            Handled::No
        }
    }
}

impl<G: NodeGraph> NodeEditor<G> {
    /// Offers one event to the runtime, carries out what it changed, and returns whether it
    /// was consumed.
    fn dispatch(&mut self, ctx: &mut EventCtx<'_>, event: &OpEvent, sample: Sample, seat: Seat) -> bool {
        let (view, mut hover) = {
            let (canvas, _) = ctx.get_raw(&mut self.canvas);
            (canvas.view(), canvas.stats().hovered)
        };
        // An operator holding the pointer gets a pick of its own when it asks (a link drag):
        // the canvas sees no events meanwhile.
        let tracking = self
            .session
            .as_ref()
            .is_some_and(|session| session.borrow().world.track_hover);
        if tracking && let Some(pos) = event.pos() {
            let window = ctx.window_transform();
            let (canvas, mut raw) = ctx.get_raw_mut(&mut self.canvas);
            hover = canvas.hit_test_raw(pos, window, &mut raw);
        }
        let is_press = matches!(event, OpEvent::Press { .. });
        // After a held gesture the canvas's hover is from the press; distrust it until the
        // next move (not re-picked: criteria hold a gesture to one pick).
        if matches!(event, OpEvent::Release { .. }) && self.modal_depth() > 0 {
            self.hover_stale = true;
        } else if matches!(event, OpEvent::Move { .. }) {
            self.hover_stale = false;
        }
        let consumed = self.feed(event, sample, seat, view, hover);
        self.mods = held_after(event);
        self.show_port_hint(ctx, self.hover_stale);
        if self.watch_edge_pan(Some(event), ctx.content_box().size()) {
            ctx.request_anim_frame();
        }

        // Capture keeps the tree off a gesture this driver owns. Masonry grants it only
        // during a press, so a held press (which may become a drag) takes it too.
        if seat != Seat::Layer {
            let running = self.modal_depth() > 0 || self.is_holding();
            let holds = ctx.pointer_capture_target_id() == Some(ctx.widget_id());
            if running && is_press && !holds {
                ctx.capture_pointer();
            } else if !running && holds && !is_press {
                ctx.release_pointer();
            }
        }

        self.flush(ctx);
        consumed
    }

    /// Lights the port under the pointer a drag would start a link from; a repaint only when
    /// it changes. `stale` puts the light out until the next move.
    fn show_port_hint(&mut self, ctx: &mut EventCtx<'_>, stale: bool) {
        let (hint, edge) = self
            .session
            .as_ref()
            .filter(|_| !stale)
            .map(|session| {
                let session = session.borrow();
                let world = &session.world;
                let idle = session.runtime.modal_depth() == 0;
                (
                    crate::ops::port_hint(&session.runtime, world, self.mods),
                    crate::ops::edge_under(world, world.pointer).filter(|_| idle),
                )
            })
            .unwrap_or_default();
        if hint != self.port_hint || edge != self.edge_hint {
            self.port_hint = hint;
            self.edge_hint = edge;
            ctx.request_post_paint();
        }
    }

    /// The node edge under the pointer a drag would resize the node by, if any.
    pub fn edge_hint(&self) -> Option<(usize, crate::ops::NodeEdge)> {
        self.edge_hint
    }

    /// The port lit up because a press and drag from it would start a link drag, if any.
    pub fn port_hint(&self) -> Option<(usize, PortSide, u16)> {
        self.port_hint
    }

    /// Hands one event to the runtime in canvas coordinates and says whether it was
    /// consumed; the caller carries out the changes.
    fn feed(
        &mut self,
        event: &OpEvent,
        sample: Sample,
        seat: Seat,
        view: Affine,
        mut hover: Option<CanvasHit>,
    ) -> bool {
        self.view = view;

        // While a press is held, what the pointer is over is what it was over at the press:
        // a fast drag leaves its target before the threshold.
        if matches!(event, OpEvent::Press { .. }) {
            self.press_hover = hover;
        } else if self
            .session
            .as_ref()
            .is_some_and(|session| session.borrow().runtime.is_holding())
        {
            hover = self.press_hover;
        }

        // Operators work in canvas coordinates; screen ones travel in the sample, for view
        // operators.
        let event = match event.pos() {
            Some(pos) => event.clone().with_pos(view.inverse() * pos),
            None => event.clone(),
        };

        let result = {
            let session = self.session.clone().expect("checked by the caller");
            let session = &mut *session.borrow_mut();
            session.world.hover = hover;
            if let Some(pos) = event.pos() {
                session.world.pointer = pos;
                session.world.pointer_screen = sample.screen;
                // The view is a uniform scale and a translation, so one coefficient says it.
                let [a, b, ..] = view.as_coeffs();
                session.world.scale = a.hypot(b);
            }
            session
                .runtime
                .feed(&mut session.world, &event, sample, Scope(&CANVAS_SCOPE), seat)
        };
        result.is_consumed()
    }

    /// Applies what the operators changed to this canvas and to the graph's other views.
    fn flush(&mut self, ctx: &mut EventCtx<'_>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let changes = session.borrow_mut().take_changes();
        if changes.dirty {
            ctx.request_post_paint();
        }
        // A menu is a layer above the window, at the pointer; the host puts it there.
        let origin = ctx.to_window(Point::ORIGIN);
        if let Some((menu, at)) = self.take_menu(ctx.widget_id(), |p| p + origin.to_vec2()) {
            self.menu = Some(menu.id());
            ctx.create_layer(LayerType::Other, menu, at);
        }
        // Structure goes through the mutate pass, the only one that may add or drop children.
        if !changes.edits.is_empty() {
            let edits = changes.edits.clone();
            ctx.mutate_later(self.canvas.id(), move |widget| apply_moves(widget, Vec::new(), edits));
        }
        // Classes too, queued after the edits so a node added and selected exists first.
        if self.selection_stale() {
            ctx.mutate_self_later(|mut widget| Self::sync_selection(&mut widget.downcast::<Self>()));
        }
        if changes.is_empty() {
            return;
        }
        {
            let (canvas, mut raw) = ctx.get_raw_mut(&mut self.canvas);
            for &(index, pos) in &changes.positions {
                canvas.move_child_raw(index, pos, &mut raw);
            }
            if changes.pan != Vec2::ZERO {
                canvas.pan_raw(changes.pan, &mut raw);
            }
            for &(origin, factor) in &changes.zoom {
                canvas.zoom_raw(origin, factor, &mut raw);
            }
        }
        if !changes.concerns_the_graph() {
            return;
        }
        let (peers, announced) = fan_out(&session.borrow(), self.canvas.id(), &changes);
        if !announced.is_empty() {
            // After the edits: a node selected as it is added must exist to be raised.
            let announced = announced.clone();
            ctx.mutate_later(self.canvas.id(), move |widget| {
                apply_to_peer(widget, Vec::new(), Vec::new(), announced);
            });
        }
        for peer in peers {
            let (positions, edits, announced) = (changes.positions.clone(), changes.edits.clone(), announced.clone());
            ctx.mutate_later(peer, move |widget| apply_to_peer(widget, positions, edits, announced));
        }
        // The panels showing the graph, as in `flush_mut`.
        for (watcher, refresh) in session.borrow().world.graph.borrow().views().owed_watchers() {
            ctx.mutate_later(watcher, move |mut widget| refresh(&mut widget));
        }
    }

    /// Draws the selection and the rubber band over the canvas, inside this area: a cached
    /// area layer must own its rectangle.
    /// The status line and the panel under it.
    fn paint_hud(&mut self, ctx: &mut PaintCtx<'_>, painter: &mut Painter<'_>) {
        let Some(caption) = self.hud_caption.as_deref() else {
            return;
        };
        let content_box = ctx.content_box();

        // Formatted here (counters change without a layout) into a scratch buffer, swapped
        // only on a real difference so the shaped text stays valid.
        let counters = self.op_counters();
        format_hud(&self.stats, &counters, caption, &mut self.hud_next);
        if self.hud_next != self.hud {
            mem::swap(&mut self.hud, &mut self.hud_next);
            self.hud_layout = None;
        }

        if self.hud_layout.is_none() {
            let text = &self.hud;
            let (fcx, lcx) = ctx.text_contexts();
            let mut builder = lcx.ranged_builder(fcx, text, 1.0, true);
            builder.push_default(StyleProperty::FontSize(12.0));
            let mut layout = builder.build(text);
            layout.break_all_lines(None);
            layout.align(None, TextAlign::Start, TextAlignOptions::default());
            self.hud_layout = Some(layout);
        }
        let Some(layout) = self.hud_layout.as_ref() else {
            return;
        };

        // As tall as the text.
        let height = f64::from(layout.height()) + 16.0;
        let panel = Rect::new(content_box.x0, content_box.y1 - height, content_box.x1, content_box.y1);
        painter.fill(panel, Color::from_rgba8(0x10, 0x10, 0x14, 0xd0)).draw();

        render_text(
            painter,
            Affine::translate((panel.x0 + 10.0, panel.y0 + 8.0)),
            layout,
            &[Color::from_rgb8(0xd0, 0xd0, 0xd8).into()],
            true,
        );
    }

    fn paint_overlay(&self, ctx: &PaintCtx<'_>, painter: &mut Painter<'_>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let session = session.borrow();
        let viewport = ctx.content_box();
        let graph = session.world.graph.borrow();
        // The links being dragged, in the selection's colour or the refusal's.
        if let Some(preview) = &session.world.link_preview {
            let colour = if preview.refused {
                self.style.refused
            } else {
                self.style.selection
            };
            for &(start, end) in &preview.curves {
                let curve = self.view * blazy_canvas::link_curve(start, end);
                painter.stroke(curve, &Stroke::new(2.0), colour).draw();
            }
        }
        // Lit ports, drawn again over the node's own dots.
        let preview = session.world.link_preview.as_ref();
        let source = preview.and_then(|preview| preview.source);
        let target = preview.and_then(|preview| {
            let colour = if preview.refused {
                self.style.refused
            } else {
                self.style.port_hover
            };
            preview.target.map(|port| (port, colour))
        });
        let lit = self
            .port_hint
            .into_iter()
            .chain(source)
            .map(|port| (port, self.style.port_hover))
            .chain(target);
        // The edge a drag would resize a node by: a line down it, in the same colour.
        if let Some((index, edge)) = self.edge_hint
            && index < graph.node_count()
        {
            let rect = self.view.transform_rect_bbox(graph.node_rect(index));
            let x = match edge {
                crate::ops::NodeEdge::Left => rect.x0 + 1.0,
                crate::ops::NodeEdge::Right => rect.x1 - 1.0,
            };
            let line = masonry::kurbo::Line::new((x, rect.y0), (x, rect.y1));
            painter.stroke(line, &Stroke::new(2.0), self.style.port_hover).draw();
        }
        let ports = session.world.ports;
        let radius = ports.dot_radius * self.view.as_coeffs()[0];
        for ((index, side, port), colour) in lit.filter(|_| radius > 0.0) {
            if index >= graph.node_count() {
                continue;
            }
            let at = self.view * ports.position(graph.node_rect(index), side, port);
            painter.fill(Circle::new(at, radius), colour).draw();
        }
        // Nodes with widgets wear the class; far-field nodes have nothing to wear it on.
        let outline = match self.style.outline {
            SelectionOutline::Always => true,
            SelectionOutline::FarField => self.stats.detail == Some(blazy_canvas::Detail::Box),
        };
        for &index in session.world.selection().iter().filter(|_| outline) {
            if index >= graph.node_count() {
                continue;
            }
            let rect = self.view.transform_rect_bbox(graph.node_rect(index));
            // Only what is on screen.
            if rect.intersect(viewport).is_zero_area() {
                continue;
            }
            painter.stroke(rect, &Stroke::new(2.0), self.style.selection).draw();
        }
        if let Some(band) = session.world.band {
            let rect = self.view.transform_rect_bbox(band);
            painter.fill(rect, self.style.band_fill).draw();
            painter.stroke(rect, &Stroke::new(1.0), self.style.selection).draw();
        }
    }
}

/// The modifiers held once `event` has happened: whether a modifier key's own event
/// includes itself depends on the platform.
fn held_after(event: &OpEvent) -> Modifiers {
    let mut mods = event.mods();
    if let OpEvent::Key {
        key: masonry::core::keyboard::Key::Named(named),
        down,
        ..
    } = event
    {
        use masonry::core::keyboard::NamedKey;
        let modifier = match named {
            NamedKey::Control => Modifiers::CONTROL,
            NamedKey::Shift => Modifiers::SHIFT,
            NamedKey::Alt => Modifiers::ALT,
            NamedKey::Meta => Modifiers::META,
            _ => Modifiers::empty(),
        };
        mods.set(modifier, *down);
    }
    mods
}

/// Converts a Masonry pointer event into the one the keymap matches; `None` for enter,
/// leave and gestures.
fn to_op_event(ctx: &EventCtx<'_>, event: &PointerEvent) -> Option<(OpEvent, Sample)> {
    to_op_event_in(
        |p| ctx.local_position(p),
        ctx.scale_factor(),
        ctx.content_box().size(),
        event,
    )
}

/// [`to_op_event`] for a caller with no `EventCtx`: `local` takes a window position into
/// this widget's space, `scale` is the display's and `size` the widget's.
fn to_op_event_in(
    local: impl Fn(PhysicalPosition<f64>) -> Point,
    scale: f64,
    size: Size,
    event: &PointerEvent,
) -> Option<(OpEvent, Sample)> {
    let (op_event, state, info) = match event {
        PointerEvent::Down(e) => (
            OpEvent::Press {
                button: e.button?,
                pos: local(e.state.position),
                mods: e.state.modifiers,
            },
            &e.state,
            &e.pointer,
        ),
        PointerEvent::Up(e) => (
            OpEvent::Release {
                button: e.button?,
                pos: local(e.state.position),
                mods: e.state.modifiers,
            },
            &e.state,
            &e.pointer,
        ),
        PointerEvent::Move(PointerUpdate { current, pointer, .. }) => (
            OpEvent::Move {
                pos: local(current.position),
                mods: current.modifiers,
            },
            current,
            pointer,
        ),
        PointerEvent::Scroll(PointerScrollEvent {
            delta, state, pointer, ..
        }) => (
            OpEvent::Scroll {
                pos: local(state.position),
                dy: wheel_pixels(*delta, scale, size),
                mods: state.modifiers,
            },
            state,
            pointer,
        ),
        PointerEvent::Enter(_) | PointerEvent::Leave(_) | PointerEvent::Cancel(_) | PointerEvent::Gesture(_) => {
            return None;
        },
    };
    // The drag threshold is in screen pixels and the timestamp is the platform's. `screen`
    // is this widget's logical space, not the event's physical position.
    let sample = Sample {
        time_ns: state.time,
        device: match info.pointer_type {
            PointerType::Mouse => Device::Mouse,
            PointerType::Pen => Device::Pen,
            PointerType::Touch => Device::Touch,
            _ => Device::Other,
        },
        screen: local(state.position),
    };
    Some((op_event, sample))
}

impl<G: NodeGraph> Widget for NodeEditor<G> {
    type Action = NoAction;

    fn on_pointer_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &PointerEvent) {
        // Pointer activity may change what the HUD shows; its numbers refresh in `layout`.
        if matches!(
            event,
            PointerEvent::Move(_) | PointerEvent::Scroll(_) | PointerEvent::Down(_) | PointerEvent::Up(_)
        ) {
            ctx.request_post_paint();
        }

        if self.session.is_none() {
            return;
        }
        // The window says the gesture is over.
        if let PointerEvent::Cancel(_) = event {
            if let Some(session) = self.session.clone() {
                let session = &mut *session.borrow_mut();
                session.runtime.cancel_all(&mut session.world);
            }
            self.watch_edge_pan(None, ctx.content_box().size());
            self.flush(ctx);
            return;
        }
        // Something below took it (a slider, the canvas panning); a control may capture
        // without marking the event handled.
        if ctx.is_handled() || ctx.pointer_capture_target_id().is_some_and(|id| id != ctx.widget_id()) {
            return;
        }
        let Some((op_event, sample)) = to_op_event(ctx, event) else {
            return;
        };
        // Bubbled: a descendant was offered it first (counted as a leak while an operator runs).
        let seat = if ctx.target() == ctx.widget_id() {
            Seat::Tree
        } else {
            Seat::Bubbled
        };
        if self.dispatch(ctx, &op_event, sample, seat) {
            ctx.set_handled();
        }
    }

    /// Keys: bubbled from whatever has focus, or sent here as the focus fallback. A text
    /// field handles its own keys.
    fn on_text_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &TextEvent) {
        let Some(session) = self.session.clone() else {
            return;
        };
        match event {
            TextEvent::WindowFocusChange(false) => {
                {
                    let session = &mut *session.borrow_mut();
                    session.runtime.cancel_all(&mut session.world);
                }
                self.watch_edge_pan(None, ctx.content_box().size());
                self.flush(ctx);
            },
            // While the menu is open, `Escape` closes it and no key reaches the keymap.
            TextEvent::Keyboard(key) if !ctx.is_handled() && self.menu.is_some() => {
                if key.state == KeyState::Down
                    && key.key == masonry::core::keyboard::Key::Named(masonry::core::keyboard::NamedKey::Escape)
                    && let Some(menu) = self.menu.take()
                {
                    ctx.remove_layer(menu);
                }
                ctx.set_handled();
            },
            TextEvent::Keyboard(key) if !ctx.is_handled() => {
                let op_event = OpEvent::from_keyboard(key);
                let seat = if ctx.target() == ctx.widget_id() {
                    Seat::Tree
                } else {
                    Seat::Bubbled
                };
                // A key has no position: no sample.
                if self.dispatch(ctx, &op_event, Sample::default(), seat) {
                    ctx.set_handled();
                }
            },
            _ => {},
        }
    }

    /// Edge pan's frame, asked for only while a gesture's pointer is in the band.
    fn on_anim_frame(&mut self, ctx: &mut UpdateCtx<'_>, _props: &mut PropertiesMut<'_>, interval: u64) {
        if !mem::take(&mut self.edge.frame_pending) {
            return;
        }
        ctx.mutate_self_later(move |mut widget| Self::edge_pan_frame(&mut widget.downcast::<Self>(), interval));
    }

    /// Tells the session which widget shows it, so a driver can send this one keys.
    fn update(&mut self, ctx: &mut UpdateCtx<'_>, _props: &mut PropertiesMut<'_>, event: &Update) {
        if let (Update::WidgetAdded, Some(session)) = (event, self.session.as_ref()) {
            session.borrow_mut().editor = Some(ctx.widget_id());
        }
        // The pointer left (`PointerEvent::Leave` never reaches `on_pointer_event`).
        if matches!(event, Update::ChildHoveredChanged(false))
            && (self.port_hint.take().is_some() | self.edge_hint.take().is_some())
        {
            ctx.request_post_paint();
        }
    }

    /// The resize cursor while a node is resized (Masonry asks the capture target). Before
    /// the press the node's own widget chooses, so the edge is lined instead.
    fn get_cursor(&self, _ctx: &QueryCtx<'_>, _pos: Point) -> CursorIcon {
        let resizing = self
            .session
            .as_ref()
            .is_some_and(|session| session.borrow().runtime.modal_name() == Some("node.resize"));
        if resizing || self.edge_hint.is_some() {
            CursorIcon::EwResize
        } else {
            CursorIcon::Default
        }
    }

    /// Returns `Some(self)`, which is what puts this widget in the pre-tree seat.
    fn as_layer(&mut self) -> Option<&mut dyn Layer> {
        Some(self)
    }

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        let fallback = match axis {
            Axis::Horizontal => 1100.0,
            Axis::Vertical => 750.0,
        };
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(fallback),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        // The graph was replaced: its history names nodes that are gone, and the classes went
        // with the node widgets.
        if let Some(session) = self.session.clone() {
            let generation = session.borrow().world.graph.borrow().views().generation();
            if generation != self.generation {
                self.generation = generation;
                let session = &mut *session.borrow_mut();
                session.runtime.cancel_all(&mut session.world);
                session.runtime.history_mut().clear();
                self.shown.clear();
            }
        }
        let canvas_size = ctx.compute_size(&mut self.canvas, SizeDef::fixed(size), size.into());
        ctx.run_layout(&mut self.canvas, canvas_size);
        ctx.place_child(&mut self.canvas, Point::ORIGIN);

        // Read the canvas counters back after its layout has run.
        let (canvas, _) = ctx.get_raw(&mut self.canvas);
        let stats = canvas.stats();
        self.view = canvas.view();
        // Mirror the view into the session, which outlives this widget.
        if let Some(session) = self.session.as_ref() {
            session.borrow_mut().view = self.view;
        }
        self.stats = stats;
        // A widget built over an existing selection tells its nodes once.
        if self.selection_stale() {
            ctx.mutate_self_later(|mut widget| Self::sync_selection(&mut widget.downcast::<Self>()));
        }
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        painter.fill(ctx.content_box(), self.style.background).draw();
    }

    fn post_paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        // Clipped to this editor's box: neither outline nor status line is a child, and an
        // unclipped one widens the area's layer rectangle into its neighbour's.
        painter.push_fill_clip(ctx.content_box());
        // Under the HUD panel, so the numbers stay readable over a selected node.
        self.paint_overlay(ctx, painter);
        self.paint_hud(ctx, painter);
        painter.pop_clip();
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.canvas);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.canvas.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut AccessNode) {}
}
