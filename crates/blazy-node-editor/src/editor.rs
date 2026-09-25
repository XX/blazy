//! The node editor widget: a canvas, the operator layer's driver, and two overlays.
//!
//! The selection overlay is always drawn. The statistics overlay is optional
//! ([`NodeEditor::with_hud`]): it began because Phase 0 is a measurement, not a demo —
//! numbers that only appear in a log are numbers nobody checks while dragging a node
//! around — and it is what an application wants while it tunes its own graph.
//!
//! ## The driver
//!
//! With [`NodeEditor::with_ops`] this widget is also where §38's answer lands: the
//! seat the operator layer sits in. It is three seats at once, and it has to be,
//! because no single one of them does the whole job:
//!
//! * [`Layer::capture_pointer_event`] — every pointer event, before the tree, even outside this widget's rectangle. It
//!   cannot stop one, so it only counts them.
//! * [`Widget::on_pointer_event`] — the ordinary bubbled route. Everything below has had its refusal first, which is
//!   what keeps the sliders inside nodes working, and it is the only place pointer capture may be taken (Masonry allows
//!   it during a press and nowhere else).
//! * [`Widget::on_text_event`] — keys, once the host has made this widget the focus fallback
//!   (`RenderRoot::set_focus_fallback`), because a keymap has to hear the keys no focused widget claimed.
//!
//! Everything an operator does lands in the model. What comes back out is
//! [`EditorWorld::moved`], and carrying that into this canvas and into the *other*
//! views of the same graph is this driver's job — §30's fan-out, moved from the
//! canvas's own drag handler to here.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::mem;

use blazy_canvas::{CanvasLayer, CanvasStats};
use blazy_ops::event::{Device, OpEvent, Sample};
use blazy_ops::keymap::{Props, Scope};
use blazy_ops::runtime::{OpRuntime, Seat};
use blazy_ops::{OpCounters, OpResult};
use masonry::accesskit::{Node as AccessNode, Role};
use masonry::core::keyboard::KeyState;
use masonry::core::{
    AccessCtx, BrushIndex, ChildrenIds, EventCtx, Handled, Layer, LayoutCtx, MeasureCtx, NoAction, PaintCtx,
    PointerEvent, PropertiesMut, PropertiesRef, RegisterCtx, StyleProperty, TextEvent, Widget, WidgetId, WidgetMut,
    WidgetPod, render_text,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Affine, Axis, Point, Rect, Size, Stroke, Vec2};
use masonry::layout::{LenReq, Length, SizeDef};
use masonry::parley::Layout;
use masonry::peniko::Color;
use masonry::ui_events::pointer::{PointerScrollEvent, PointerType, PointerUpdate};
use masonry::{TextAlign, TextAlignOptions};

use crate::ops::CANVAS_SCOPE;
use crate::{Edit, EditorWorld, MoveRecorder, NodeGraph, SharedGraph};

/// The operator layer, when this editor drives one.
struct Ops<G: NodeGraph> {
    runtime: OpRuntime<EditorWorld<G>>,
    world: EditorWorld<G>,
}

/// The colours the editor draws with.
///
/// Style rather than mechanism, like the canvas's `LinkStyle`: what a selection looks
/// like is the application's business.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayStyle {
    /// Behind the canvas, where no node is.
    pub background: Color,
    /// The outline of a selected node, and of the rubber band.
    pub selection: Color,
    /// The inside of the rubber band.
    pub band_fill: Color,
}

impl Default for OverlayStyle {
    fn default() -> Self {
        Self {
            background: Color::from_rgb8(0x1c, 0x1c, 0x20),
            selection: Color::from_rgb8(0xff, 0xa5, 0x2c),
            band_fill: Color::from_rgba8(0xff, 0xa5, 0x2c, 0x20),
        }
    }
}

/// Renders a canvas, drives the operators, and draws the selection over it.
pub struct NodeEditor<G: NodeGraph> {
    canvas: WidgetPod<CanvasLayer>,
    /// The operator layer, or `None` for an editor with the canvas's own gestures.
    ///
    /// Optional so that every measurement written before §38 still measures what it
    /// measured: an editor without operators is the widget it always was, down to the
    /// counter.
    ops: Option<Ops<G>>,
    /// How the overlays are drawn.
    style: OverlayStyle,
    /// The last line of the statistics overlay, or `None` for no overlay at all.
    hud_caption: Option<String>,
    /// The canvas's view transform, as of the last layout.
    ///
    /// Cached because the overlay is drawn in `post_paint`, and a `PaintCtx` cannot
    /// reach into a child. It cannot go stale: the view only ever changes through a
    /// layout (§22), and this is refreshed in every one.
    view: Affine,
    /// Stats cached during layout, so `post_paint` draws numbers from this frame.
    stats: CanvasStats,
    /// The HUD text currently on screen.
    hud: String,
    /// Scratch buffer the next HUD text is formatted into, reused between frames.
    hud_next: String,
    /// The HUD text, shaped.
    ///
    /// Shaping three lines of text costs more than everything else this widget does,
    /// so it must not happen per frame. It is redone only when [`Self::hud`] actually
    /// changes, which during a steady pan is almost never.
    hud_layout: Option<Layout<BrushIndex>>,
}

impl<G: NodeGraph> NodeEditor<G> {
    /// Statistics from the canvas, as of the last layout pass.
    pub fn stats(&self) -> CanvasStats {
        self.stats
    }

    /// Runs a callback with a `WidgetMut` for the inner canvas.
    ///
    /// The canvas is reached through a context rather than a field because a
    /// `WidgetPod` hands its widget to the arena once inserted.
    pub fn with_canvas<R>(this: &mut WidgetMut<'_, Self>, f: impl FnOnce(WidgetMut<'_, CanvasLayer>) -> R) -> R {
        let canvas = this.ctx.get_mut(&mut this.widget.canvas);
        f(canvas)
    }

    /// Wraps a canvas in an editor with no operator layer and no statistics overlay.
    pub fn new(canvas: CanvasLayer) -> Self {
        Self {
            canvas: WidgetPod::new(canvas),
            ops: None,
            style: OverlayStyle::default(),
            hud_caption: None,
            view: Affine::IDENTITY,
            stats: CanvasStats::default(),
            hud: String::new(),
            hud_next: String::new(),
            hud_layout: None,
        }
    }

    /// The same editor, with the operator layer driving the primary button and the
    /// keyboard.
    ///
    /// Takes the canvas's own primary-button gestures away
    /// ([`CanvasLayer::with_builtin_gestures`]): the canvas is *below* this widget, so
    /// leaving them on would mean the canvas answering a press before the keymap ever
    /// heard of it. What the canvas keeps is the middle-button pan, the wheel zoom and
    /// the pick on every pointer event — the last of which is what the operators' poll
    /// reads as context.
    pub fn with_ops(canvas: CanvasLayer, graph: &SharedGraph<G>) -> Self {
        Self::with_runtime(canvas, graph, crate::ops::runtime())
    }

    /// As [`with_ops`](Self::with_ops), with a runtime of the caller's own.
    ///
    /// For a keymap read from a file, or operators of the application's own registered
    /// beside [`ops`](crate::ops)'s. The runtime is handed over whole rather than
    /// assembled here, because what goes into it is the application's decision and
    /// `blazy-ops` already has the vocabulary for it.
    pub fn with_runtime(canvas: CanvasLayer, graph: &SharedGraph<G>, runtime: OpRuntime<EditorWorld<G>>) -> Self {
        Self {
            ops: Some(Ops {
                runtime,
                world: EditorWorld::new(graph),
            }),
            ..Self::new(canvas.with_builtin_gestures(false))
        }
    }

    /// The same editor, drawing a statistics overlay whose last line is `caption`.
    ///
    /// Two lines of numbers a human watches — what is materialised, the zoom, the
    /// detail level, the widgets built, and the gestures the runtime recognised — and
    /// one line of the application's, usually what the keys do.
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
        self.ops
            .as_ref()
            .map(|ops| ops.world.selection.clone())
            .unwrap_or_default()
    }

    /// The operator counters, all zero when there is no operator layer.
    pub fn op_counters(&self) -> OpCounters {
        self.ops.as_ref().map(|ops| ops.runtime.counters()).unwrap_or_default()
    }

    /// How many operators are running. Zero between gestures, and a gesture that ends
    /// with this non-zero is one that never finished.
    pub fn modal_depth(&self) -> usize {
        self.ops.as_ref().map_or(0, |ops| ops.runtime.modal_depth())
    }

    /// The status line as it was last painted. Empty without [`with_hud`](Self::with_hud).
    ///
    /// For a test that wants to check what the window says rather than what the counters
    /// hold — the two can disagree, and when they do it is the line that is wrong.
    pub fn hud(&self) -> &str {
        &self.hud
    }

    /// Whether a press is being held while the runtime waits to see what it becomes.
    ///
    /// Between "nothing running" and "a modal operator running" there is now a third
    /// state, and a gesture that ends in it is one that never resolved (§39.3).
    pub fn is_holding(&self) -> bool {
        self.ops.as_ref().is_some_and(|ops| ops.runtime.is_holding())
    }

    /// Steps in the undo history.
    pub fn history_depth(&self) -> usize {
        self.ops.as_ref().map_or(0, |ops| ops.runtime.history().depth())
    }

    /// Bytes the undo history is holding, by its steps' own reckoning.
    pub fn history_bytes(&self) -> usize {
        self.ops.as_ref().map_or(0, |ops| ops.runtime.history().bytes())
    }

    /// Runs an operator by name, from outside the tree.
    ///
    /// The `exec` half of §11 — a script, a test, a redo — and it goes through the
    /// same runtime, the same poll and the same history as a key would. What it also
    /// does is what the interactive path does after a dispatch: carry the moved nodes
    /// into this canvas and into the other views of the graph.
    pub fn exec(this: &mut WidgetMut<'_, Self>, name: &str, props: &Props) -> OpResult {
        let Some(ops) = this.widget.ops.as_mut() else {
            return OpResult::PassThrough;
        };
        let result = ops.runtime.exec(&mut ops.world, name, props);
        Self::flush_mut(this);
        result
    }

    /// Sets how a finished move becomes an undo step. See [`EditorWorld::record_move`].
    pub fn set_move_recorder(this: &mut WidgetMut<'_, Self>, recorder: MoveRecorder<G>) {
        if let Some(ops) = this.widget.ops.as_mut() {
            ops.world.record_move = recorder;
        }
    }

    /// Applies what the operators changed, from a `WidgetMut`.
    ///
    /// The twin of [`flush`](Self::flush), and what the two share — draining the world,
    /// and what a peer does with the result — is [`Changes`] and [`apply_moves`]. What
    /// is left here is the only thing that really differs: which context reaches the
    /// canvas.
    fn flush_mut(this: &mut WidgetMut<'_, Self>) {
        let Some(ops) = this.widget.ops.as_mut() else {
            return;
        };
        let changes = ops.take_changes();
        if changes.dirty {
            this.ctx.request_post_paint();
        }
        if changes.is_empty() {
            return;
        }
        {
            let mut canvas = this.ctx.get_mut(&mut this.widget.canvas);
            apply_edits(&mut canvas, &changes.edits);
            for &(index, pos) in &changes.positions {
                CanvasLayer::move_child(&mut canvas, index, pos);
            }
            if changes.pan != Vec2::ZERO {
                CanvasLayer::pan(&mut canvas, changes.pan);
            }
        }
        if changes.positions.is_empty() && changes.edits.is_empty() {
            // A pan is this view's business alone: the view is not model state, so the
            // other views of the graph are not following it (§30).
            return;
        }
        let peers = peers_of(
            this.widget.ops.as_ref().expect("checked above"),
            this.widget.canvas.id(),
        );
        for peer in peers {
            let (positions, edits) = (changes.positions.clone(), changes.edits.clone());
            this.ctx
                .mutate_later(peer, move |widget| apply_moves(widget, positions, edits));
        }
    }
}

/// What the operators changed and the driver has not carried into the tree yet.
///
/// Drained in one place because the two flush paths used to drain it in two, in
/// slightly different orders — the `EventCtx` one asked for a post-paint before it
/// computed the positions and the `WidgetMut` one after, which is the kind of
/// difference that survives a review and then diverges.
struct Changes {
    positions: Vec<(usize, Point)>,
    /// Changes to the shape of the graph, in the order the operators made them.
    ///
    /// Order matters here where it does not for positions: a link added to a node that
    /// was added in the same step has to arrive after it.
    edits: Vec<Edit>,
    pan: Vec2,
    dirty: bool,
}

impl Changes {
    /// Whether anything has to reach the canvas. A repaint is not "something": it is
    /// asked for before this is consulted.
    fn is_empty(&self) -> bool {
        self.positions.is_empty() && self.edits.is_empty() && self.pan == Vec2::ZERO
    }
}

impl<G: NodeGraph> Ops<G> {
    /// Takes what the operators changed, leaving the world clean.
    fn take_changes(&mut self) -> Changes {
        let moved = mem::take(&mut self.world.moved);
        Changes {
            positions: positions_of(&self.world, moved),
            edits: mem::take(&mut self.world.edits),
            pan: mem::replace(&mut self.world.pan, Vec2::ZERO),
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
        match edit {
            Edit::NodeAdded { index, rect } => CanvasLayer::insert_node(canvas, index, rect.origin(), rect.size()),
            Edit::NodeRemoved { index } => {
                CanvasLayer::remove_node(canvas, index);
            },
            Edit::LinkAdded(link) => {
                CanvasLayer::insert_link(canvas, link);
            },
            Edit::LinkRemoved(link) => {
                if let Some(name) = CanvasLayer::link_name(canvas, link) {
                    CanvasLayer::remove_link(canvas, name);
                }
            },
        }
    }
}

/// Moves nodes in another view of the same graph (§30).
///
/// The body of every `mutate_later` this driver schedules, in one place: a peer is
/// reached the same way whichever path found the change.
fn apply_moves(mut widget: WidgetMut<'_, dyn Widget>, positions: Vec<(usize, Point)>, edits: Vec<Edit>) {
    let mut canvas = widget.downcast::<CanvasLayer>();
    apply_edits(&mut canvas, &edits);
    for (index, pos) in positions {
        CanvasLayer::move_child(&mut canvas, index, pos);
    }
}

/// The current position of every node in `moved`, deduplicated.
///
/// Deduplicated here rather than as it is filled: a drag pushes the same index on
/// every frame, and one sort of a handful of indices per event is cheaper than a set
/// lookup per node per frame.
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

/// The other canvases showing the same graph (§30).
fn peers_of<G: NodeGraph>(ops: &Ops<G>, own_canvas: WidgetId) -> Vec<WidgetId> {
    let mut peers = Vec::new();
    ops.world.graph.borrow().other_views(own_canvas, &mut peers);
    peers
}

/// Formats the HUD into `out`, reusing its allocation.
///
/// Deliberately shows only quantities a human watches: how much of the graph is
/// materialised, the zoom, the detail level, and how many widgets have been built.
/// The cumulative pass counters live in [`CanvasStats`] for the benchmark; putting
/// them on screen would change the text every frame and defeat the cache.
///
/// The second line is a measurement rather than a caption. A gesture is decided from
/// the events themselves — the travel since the press and the
/// platform's own timestamps (§39.2) — so the only way to know that the platform really
/// gives us what the mechanism assumes is to watch the counters move in a real window.
/// A double click that never registers because `PointerState::time` arrives as zero on
/// some backend is invisible to every test we have, and obvious here.
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

/// The pre-tree seat (§38.1).
///
/// Called for every pointer event before the target is even computed, and for events
/// outside this widget's rectangle as well. It cannot stop one: the method returns
/// nothing, its `EventCtx` is dropped, and `capture_pointer` is refused because the
/// pass does not allow capture here. So what it does is count, and the count is the
/// evidence for the one-line request upstream — whose own TODO already lists "return
/// flag to suppress event from reaching children".
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
        if self.ops.is_none() {
            return Handled::No;
        }
        // Somebody below holds the pointer, so the event is going to the tree whatever
        // this seat says — the fork lets capture outrank a layer, exactly so that a
        // widget in the middle of a gesture is told how it ends. Dispatching here as
        // well would deliver every event twice.
        if ctx.pointer_capture_target_id().is_some_and(|id| id != ctx.widget_id()) {
            if let Some(ops) = self.ops.as_mut() {
                ops.runtime.observe(&op_event);
            }
            return Handled::No;
        }
        // Everything else is the runtime's call. It acts from this seat only when it
        // already owns the gesture — a running modal operator, or a press it is holding
        // to see whether it becomes a click or a drag — and otherwise counts and lets
        // the tree have its right of first refusal (§20 claim 3, §39.3).
        if self.dispatch(ctx, &op_event, sample, Seat::Layer) {
            Handled::Yes
        } else {
            Handled::No
        }
    }
}

impl<G: NodeGraph> NodeEditor<G> {
    /// Offers one event to the runtime and carries out what it changed.
    ///
    /// Returns whether the event was consumed. Everything the operators need to know
    /// about the world is filled in first: where the pointer is, in canvas
    /// coordinates, and what the canvas last found under it.
    fn dispatch(&mut self, ctx: &mut EventCtx<'_>, event: &OpEvent, sample: Sample, seat: Seat) -> bool {
        let (view, hover) = {
            let (canvas, _) = ctx.get_raw(&mut self.canvas);
            (canvas.view(), canvas.stats().hovered)
        };
        self.view = view;
        let is_press = matches!(event, OpEvent::Press { .. });

        // Into canvas coordinates before the runtime sees it, because that is the space
        // this application's operators think in — and the space a `Drag` has to carry its
        // anchor in, or the first event of every drag moves the node by the pan (§39.7).
        // The other space travels in the sample, which is what a view operator uses.
        let event = match event.pos() {
            Some(pos) => event.clone().with_pos(view.inverse() * pos),
            None => event.clone(),
        };

        let result = {
            let ops = self.ops.as_mut().expect("checked by the caller");
            ops.world.hover = hover;
            if let Some(pos) = event.pos() {
                ops.world.pointer = pos;
                ops.world.pointer_screen = sample.screen;
            }
            ops.runtime
                .feed(&mut ops.world, &event, sample, Scope(&CANVAS_SCOPE), seat)
        };

        // Masonry's own modality. It may be taken during a press and at no other time,
        // and only where the event is addressed to this widget — the pre-tree seat is
        // given a context that refuses it (§38.1). Kept even though the layer seat can
        // now withhold events on its own: capture is what stops the *tree* from acting
        // on a gesture this driver already owns.
        if seat != Seat::Layer {
            let running = self.modal_depth() > 0;
            let holds = ctx.pointer_capture_target_id() == Some(ctx.widget_id());
            if running && is_press && !holds {
                ctx.capture_pointer();
            } else if !running && holds && !is_press {
                ctx.release_pointer();
            }
        }

        self.flush(ctx);
        result.is_consumed()
    }

    /// Applies what the operators changed to this canvas and to the graph's other
    /// views.
    ///
    /// The §30 fan-out, from here rather than from the canvas's drag: the operator
    /// wrote the model, and only the driver holds a widget context.
    fn flush(&mut self, ctx: &mut EventCtx<'_>) {
        let Some(ops) = self.ops.as_mut() else {
            return;
        };
        let changes = ops.take_changes();
        if changes.dirty {
            ctx.request_post_paint();
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
        }
        // Structure goes through the mutate pass even for this canvas, because adding a
        // node may add a child and removing one may drop a child, and that is the only
        // pass allowed to do either. A drag does not wait for it; an edit can.
        if !changes.edits.is_empty() {
            let edits = changes.edits.clone();
            ctx.mutate_later(self.canvas.id(), move |widget| apply_moves(widget, Vec::new(), edits));
        }
        if changes.positions.is_empty() && changes.edits.is_empty() {
            // A pan is this view's business alone: the view is not model state, so the
            // other views of the graph are not following it (§30).
            return;
        }
        let peers = peers_of(self.ops.as_ref().expect("checked above"), self.canvas.id());
        for peer in peers {
            let (positions, edits) = (changes.positions.clone(), changes.edits.clone());
            ctx.mutate_later(peer, move |widget| apply_moves(widget, positions, edits));
        }
    }

    /// Draws the selection and the rubber band over the canvas.
    ///
    /// In this widget rather than in the canvas, and in canvas coordinates through the
    /// view transform, because that is the answer §38.5 gives to "where does a modal
    /// operator draw": inside the area that owns the pixels. An overlay across areas
    /// would break the one condition the layer cache cannot check — that a cached
    /// layer owns its rectangle (§36.4).
    fn paint_overlay(&self, ctx: &PaintCtx<'_>, painter: &mut Painter<'_>) {
        let Some(ops) = self.ops.as_ref() else {
            return;
        };
        let viewport = ctx.content_box();
        let graph = ops.world.graph.borrow();
        for &index in &ops.world.selection {
            if index >= graph.node_count() {
                continue;
            }
            let rect = self.view.transform_rect_bbox(graph.node_rect(index));
            // Only what is on screen. A box select can hold thousands of nodes, and an
            // outline off screen costs the same as one on it.
            if rect.intersect(viewport).is_zero_area() {
                continue;
            }
            painter.stroke(rect, &Stroke::new(2.0), self.style.selection).draw();
        }
        if let Some(band) = ops.world.band {
            let rect = self.view.transform_rect_bbox(band);
            painter.fill(rect, self.style.band_fill).draw();
            painter.stroke(rect, &Stroke::new(1.0), self.style.selection).draw();
        }
    }
}

/// Converts a Masonry pointer event into the one the keymap matches.
///
/// Returns `None` for the events an operator layer has no use for — enter, leave,
/// gestures, and the scroll the canvas owns.
fn to_op_event(ctx: &EventCtx<'_>, event: &PointerEvent) -> Option<(OpEvent, Sample)> {
    let (op_event, state, info) = match event {
        PointerEvent::Down(e) => (
            OpEvent::Press {
                button: e.button?,
                pos: ctx.local_position(e.state.position),
                mods: e.state.modifiers,
            },
            &e.state,
            &e.pointer,
        ),
        PointerEvent::Up(e) => (
            OpEvent::Release {
                button: e.button?,
                pos: ctx.local_position(e.state.position),
                mods: e.state.modifiers,
            },
            &e.state,
            &e.pointer,
        ),
        PointerEvent::Move(PointerUpdate { current, pointer, .. }) => (
            OpEvent::Move {
                pos: ctx.local_position(current.position),
                mods: current.modifiers,
            },
            current,
            pointer,
        ),
        PointerEvent::Scroll(PointerScrollEvent { .. })
        | PointerEvent::Enter(_)
        | PointerEvent::Leave(_)
        | PointerEvent::Cancel(_)
        | PointerEvent::Gesture(_) => return None,
    };
    // The threshold is measured in screen pixels and the timestamp comes from the
    // platform, so a drag means the same distance at every zoom and a double click can
    // be produced by a test without a clock (§39.2).
    //
    // `screen` is this widget's own logical space — the space `EditorWorld::pointer_screen`
    // is in — and not the physical position the event arrived with. Two spaces are one
    // too many already; a third, differing by the display's scale factor, is how the
    // view came to jump by half a click on a HiDPI screen.
    let sample = Sample {
        time_ns: state.time,
        device: match info.pointer_type {
            PointerType::Mouse => Device::Mouse,
            PointerType::Pen => Device::Pen,
            PointerType::Touch => Device::Touch,
            _ => Device::Other,
        },
        screen: ctx.local_position(state.position),
    };
    Some((op_event, sample))
}

impl<G: NodeGraph> Widget for NodeEditor<G> {
    type Action = NoAction;

    fn on_pointer_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &PointerEvent) {
        // Any pointer activity may have moved, zoomed or dragged something, so the
        // HUD needs redrawing. The numbers themselves are refreshed in `layout`,
        // which runs before paint, so what gets drawn is this frame's data.
        if matches!(
            event,
            PointerEvent::Move(_) | PointerEvent::Scroll(_) | PointerEvent::Down(_) | PointerEvent::Up(_)
        ) {
            ctx.request_post_paint();
        }

        if self.ops.is_none() {
            return;
        }
        // A cancel is the window telling us the gesture is over, and there is nobody
        // else to tell the operators.
        if let PointerEvent::Cancel(_) = event {
            if let Some(ops) = self.ops.as_mut() {
                ops.runtime.cancel_all(&mut ops.world);
            }
            self.flush(ctx);
            return;
        }
        // Something below took it — a slider in a node, or the canvas panning. The
        // capture target rather than `is_handled` alone, for the reason the canvas
        // gives: a control may capture without marking the event handled, and
        // dispatching over it would steal the grip.
        if ctx.is_handled() || ctx.pointer_capture_target_id().is_some_and(|id| id != ctx.widget_id()) {
            return;
        }
        let Some((op_event, sample)) = to_op_event(ctx, event) else {
            return;
        };
        // Bubbled means a descendant was offered this event first. While an operator is
        // running that is a leak, and the runtime counts it (§38.1).
        let seat = if ctx.target() == ctx.widget_id() {
            Seat::Tree
        } else {
            Seat::Bubbled
        };
        if self.dispatch(ctx, &op_event, sample, seat) {
            ctx.set_handled();
        }
    }

    /// Keys, once the host has made this widget the focus fallback.
    ///
    /// Bubbled from whatever had focus, or targeted here directly when nothing did —
    /// which is what a keymap needs and what `RenderRoot::set_focus_fallback` is for.
    /// A text field keeps its keys: it is focused, it handles them, and this never
    /// sees them (§12).
    fn on_text_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &TextEvent) {
        let Some(ops) = self.ops.as_mut() else {
            return;
        };
        match event {
            TextEvent::WindowFocusChange(false) => {
                ops.runtime.cancel_all(&mut ops.world);
                self.flush(ctx);
            },
            TextEvent::Keyboard(key) if !ctx.is_handled() => {
                let op_event = OpEvent::Key {
                    key: key.key.clone(),
                    mods: key.modifiers,
                    down: key.state == KeyState::Down,
                };
                let seat = if ctx.target() == ctx.widget_id() {
                    Seat::Tree
                } else {
                    Seat::Bubbled
                };
                // A key has no position and no device: the sample is what a resolver
                // would measure a gesture on, and there is no gesture here.
                if self.dispatch(ctx, &op_event, Sample::default(), seat) {
                    ctx.set_handled();
                }
            },
            _ => {},
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
        let canvas_size = ctx.compute_size(&mut self.canvas, SizeDef::fixed(size), size.into());
        ctx.run_layout(&mut self.canvas, canvas_size);
        ctx.place_child(&mut self.canvas, Point::ORIGIN);

        // Read the canvas counters back after its layout has run.
        let (canvas, _) = ctx.get_raw(&mut self.canvas);
        let stats = canvas.stats();
        self.view = canvas.view();
        self.stats = stats;
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        painter.fill(ctx.content_box(), self.style.background).draw();
    }

    fn post_paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        // Under the HUD panel, so the numbers stay readable over a selected node.
        self.paint_overlay(ctx, painter);

        let Some(caption) = self.hud_caption.as_deref() else {
            return;
        };
        let content_box = ctx.content_box();

        // Formatted here rather than in `layout`, because half of what it says changes
        // without the layout changing: a click moves the counters and relayouts nothing.
        // Formatting into a scratch buffer and swapping only on a real difference is what
        // keeps the shaped text — which is the expensive half — valid between frames.
        let counters = self.ops.as_ref().map(|ops| ops.runtime.counters()).unwrap_or_default();
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

        // The panel is as tall as the text, rather than as tall as the text used to be:
        // a line added to the status line should not disappear under the edge of a
        // rectangle whose height someone typed in once.
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
