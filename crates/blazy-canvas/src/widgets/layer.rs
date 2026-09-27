//! [`CanvasLayer`]: the viewport — fixed size, clip path, and the view it owns.

use std::cell::Cell;

use blazy_shape::scale_of;
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AllowRawMut, ChildrenIds, EventCtx, LayoutCtx, MeasureCtx, NoAction, PaintCtx, PointerEvent,
    PropertiesMut, PropertiesRef, RawCtx, RegisterCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::dpi::{LogicalPosition, PhysicalPosition};
use masonry::imaging::Painter;
use masonry::kurbo::{Affine, Axis, Point, Rect, Size, Vec2};
use masonry::layout::{AsUnit, LenReq, Length, SizeDef};
use masonry::ui_events::pointer::{PointerButton, PointerScrollEvent, PointerUpdate};

use super::*;
use crate::detail::{DetailBudget, DetailThresholds};
use crate::links::{Link, LinkLayer, LinkStyle};
use crate::source::NodeSource;
use crate::stats::{CanvasCounters, CanvasHit, CanvasStats};

/// So a parent driving gestures from above can reach the canvas inside its own event.
///
/// The alternative is `mutate_later`, which defers to the mutate pass and boxes a
/// closure per node per event; a driver moving a selection is the case that makes the
/// difference (§38.3).
impl AllowRawMut for CanvasLayer {}

/// What the pointer is currently doing on the canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    /// Nothing.
    None,
    /// Panning the view. Holds the last pointer position in viewport space.
    Pan { last: Point },
    /// Dragging a node. Holds its index and the grab offset in canvas space.
    Node { index: usize, grab: Vec2 },
}

// --- MARK: LAYER

/// A canvas of freely positioned children with a pan/zoom view.
///
/// This is the viewport: fixed size, clips its content, and owns the view transform
/// which it pushes down to its [`CanvasContent`] child during layout.
pub struct CanvasLayer {
    content: WidgetPod<CanvasContent>,
    /// Canvas-space to viewport-space transform (pan and zoom).
    view: Affine,
    /// Whether `view` still needs pushing down to the content widget.
    view_dirty: bool,
    /// Viewport size in widget coordinates, set during layout.
    viewport: Size,
    /// How far past the viewport to keep nodes alive, as a fraction of the viewport.
    ///
    /// Culling exactly at the viewport edge makes nodes pop in mid-drag. Expressed as
    /// a fraction rather than in canvas units on purpose: a fixed canvas-space margin
    /// means a huge screen margin when zoomed in and a sliver when zoomed out, which
    /// is backwards.
    overscan: f64,
    /// How far past the viewport the far-field scene and the link set are recorded.
    far_overscan: f64,
    /// Mirror of the content's counters, refreshed at the end of each layout.
    stats: Cell<CanvasStats>,
    /// Current pointer gesture.
    drag: Drag,
    /// Whether only the node under the pointer gets interactive controls.
    controls_on_hover: bool,
    /// Whether the canvas acts on the primary button itself.
    builtin_gestures: bool,
    /// Where the detail levels switch over for readability.
    thresholds: DetailThresholds,
    /// What the tree may cost, in widgets.
    budget: DetailBudget,
    /// Smallest and largest permitted zoom.
    zoom_limits: (f64, f64),
    /// Whether the source has been told this canvas's id yet.
    attached: bool,
    /// Reused buffer for the peers a move has to be broadcast to.
    peers: Vec<WidgetId>,
    /// Links handed to [`CanvasLayer::with_links`] before the canvas was in a tree.
    pending_links: Option<Vec<Link>>,
    link_style: LinkStyle,
}

impl CanvasLayer {
    /// Creates a canvas over `count` nodes.
    ///
    /// `geometry` supplies each node's position and size, and `source` builds its
    /// widget when it scrolls into view. Only the geometry is stored up front: a
    /// graph of a million nodes costs a million `(Point, Size)` pairs, not a million
    /// widgets.
    ///
    /// `count` is how many *names* the graph uses, not how many nodes it holds, and
    /// `geometry` answers `None` for a name nothing holds. A graph that has been edited
    /// has holes in its names — a removal frees a name and nothing renumbers what stayed
    /// (§43) — and a view built over it afterwards has to agree about them, or the
    /// names in a saved selection would mean different nodes in different views.
    pub fn new(
        count: usize,
        mut geometry: impl FnMut(usize) -> Option<(Point, Size)>,
        source: impl NodeSource,
    ) -> Self {
        let slots = (0..count)
            .map(|i| match geometry(i) {
                Some((pos, size)) => Slot {
                    alive: true,
                    pos,
                    size,
                    pod: None,
                    built: None,
                },
                None => EMPTY_SLOT,
            })
            .collect();
        Self {
            content: WidgetPod::new(CanvasContent::new(slots, Box::new(source))),
            view: Affine::IDENTITY,
            view_dirty: true,
            viewport: Size::ZERO,
            overscan: DEFAULT_OVERSCAN,
            far_overscan: FAR_OVERSCAN,
            stats: Cell::new(CanvasStats {
                zoom: 1.0,
                ..CanvasStats::default()
            }),
            drag: Drag::None,
            controls_on_hover: false,
            builtin_gestures: true,
            thresholds: DetailThresholds::default(),
            budget: DetailBudget::default(),
            attached: false,
            peers: Vec::new(),
            zoom_limits: (0.02, 8.0),
            pending_links: None,
            link_style: LinkStyle::default(),
        }
    }

    /// Adds edges between nodes.
    ///
    /// Indices into the node array given to [`new`](Self::new); an edge naming a node
    /// that does not exist is skipped when drawn rather than rejected here, because
    /// the graph is the application's to validate.
    ///
    /// Held here and handed down at the first layout: a `WidgetPod` gives its widget
    /// to the arena on insertion, so the canvas cannot reach its own content between
    /// construction and being in a tree.
    pub fn with_links(mut self, links: Vec<Link>) -> Self {
        self.pending_links = Some(links);
        self
    }

    /// Sets how far past the viewport the far field and the link set are recorded, as
    /// a fraction of the viewport.
    ///
    /// The margin that turns "re-record every frame" into "re-record every few hundred"
    /// (§20.6a). It is bought with a bigger recorded scene, and the scene is what the
    /// rasteriser is charged for every frame (§32.3), so the two sides of the trade are
    /// re-recordings and path segments. [`DEFAULT_FAR_OVERSCAN`](Self::DEFAULT_FAR_OVERSCAN)
    /// is what §35.2 measured the trade at.
    pub fn with_far_overscan(mut self, fraction: f64) -> Self {
        self.far_overscan = fraction.max(0.0);
        self
    }

    /// The default of [`Self::with_far_overscan`]: a quarter of the viewport on each
    /// side, measured down from a half in §35.2.
    pub const DEFAULT_FAR_OVERSCAN: f64 = FAR_OVERSCAN;

    /// Restyles the links.
    pub fn with_link_style(mut self, style: LinkStyle) -> Self {
        self.link_style = style;
        self
    }

    /// Materialises interactive controls only for the node under the pointer.
    ///
    /// Off by default. When on, every other node gets whatever its `Simplified` form
    /// paints instead of real control widgets, which at 140 visible nodes is roughly
    /// five times cheaper per frame — a control nobody is touching is still three or
    /// four widgets that every pass has to walk.
    ///
    /// The catch is visual: the painted stand-in is swapped for real widgets as the
    /// pointer arrives, so unless it matches them closely the interface appears to
    /// change under the cursor. Matching Masonry's themed controls by hand is also
    /// fragile — a theme change silently breaks the resemblance. Turn this on only
    /// where the node body is drawn by the application anyway, or where nodes are
    /// small enough that the difference does not read.
    pub fn with_controls_on_hover(mut self, enabled: bool) -> Self {
        self.controls_on_hover = enabled;
        self
    }

    /// Whether the canvas acts on the primary button itself. On by default.
    ///
    /// The seam an operator layer needs (§11, §38). The canvas's own primary-button
    /// gestures — drag the node under the pointer, pan when there is none — are a
    /// default, not the mechanism: an application whose keymap binds that button
    /// cannot have the canvas answering it first, because the canvas sits *below* the
    /// application's driver and Masonry routes to the deepest widget before it
    /// bubbles.
    ///
    /// Turning them off leaves the rest alone: the middle button still pans, the wheel
    /// still zooms, and the pointer still picks on every press and move — the pick is
    /// what an operator's context is made of, and it is measured not to cost a layout
    /// (§25.4).
    pub fn with_builtin_gestures(mut self, enabled: bool) -> Self {
        self.builtin_gestures = enabled;
        self
    }

    /// Sets where the detail levels switch over for readability.
    ///
    /// Policy, and the application's: how small a control may get before it stops
    /// being usable depends on how the node is drawn (§20.7).
    pub fn with_thresholds(mut self, thresholds: DetailThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Sets the ceiling on widgets this canvas may keep in the tree.
    ///
    /// The other half of the level decision, and the one that does not follow the
    /// zoom: see [`DetailBudget`]. An application tiling several canvases in one
    /// window should hand each of them a share of one window budget
    /// ([`DetailBudget::split`]), because the frame walks the window's tree and not
    /// any single canvas's.
    pub fn with_budget(mut self, budget: DetailBudget) -> Self {
        self.budget = budget;
        self
    }

    /// The cost ceiling in force.
    pub fn budget(&self) -> DetailBudget {
        self.budget
    }

    /// The canvas-space to viewport-space transform.
    ///
    /// The same canvas, showing what `view` says.
    ///
    /// For a canvas built to take over from another one — the area rebuilt in a second
    /// window — because the view is a view's own state and nothing else remembers it:
    /// §22 keeps it out of layout, and §30 kept it out of the model.
    #[must_use]
    pub fn with_view(mut self, view: Affine) -> Self {
        self.view = view;
        self.view_dirty = true;
        self
    }

    /// Public because anything drawing over the canvas — an overlay, a rubber band,
    /// a tooltip anchored to a node — has to agree with it about where things are,
    /// and rederiving it from the zoom and the pan is how two answers start to
    /// differ.
    pub fn view(&self) -> Affine {
        self.view
    }

    /// The current zoom factor, derived from the view transform.
    pub fn zoom(&self) -> f64 {
        let c = self.view.as_coeffs();
        (c[0] * c[0] + c[1] * c[1]).sqrt()
    }

    /// Statistics as of the last layout pass.
    pub fn stats(&self) -> CanvasStats {
        self.stats.get()
    }

    /// The region of canvas space nodes are kept live in: what the viewport covers,
    /// plus the overscan margin on each side.
    fn live_canvas_rect(&self) -> Rect {
        let viewport = Rect::from_origin_size(Point::ORIGIN, self.viewport);
        let rect = self.view.inverse().transform_rect_bbox(viewport);
        rect.inflate(rect.width() * self.overscan, rect.height() * self.overscan)
    }

    // --- MARK: WIDGETMUT

    /// Sets the view transform.
    ///
    /// Requests a layout pass on the canvas itself, because culling depends on the
    /// view. It does *not* dirty the content widget: child positions are in canvas
    /// coordinates, so a view change moves nobody and the transform does all the
    /// work. Keeping the content clean is what stops Masonry from marking it for
    /// repaint — see the note in [`CanvasLayer::layout`].
    pub fn set_view(this: &mut WidgetMut<'_, Self>, view: Affine) {
        if this.widget.store_view(view) {
            this.ctx.request_layout();
        }
    }

    /// Pans the view by a delta in viewport coordinates.
    pub fn pan(this: &mut WidgetMut<'_, Self>, delta: Vec2) {
        let view = Affine::translate(delta) * this.widget.view;
        Self::set_view(this, view);
    }

    /// The view that results from zooming about `origin`, or `None` if the zoom is
    /// already at its limit.
    ///
    /// Pure, so the `WidgetMut` entry point and the wheel handler share one copy of
    /// the arithmetic instead of two that can drift apart.
    fn zoomed_view(&self, origin: Point, factor: f64) -> Option<Affine> {
        let current = self.zoom();
        let clamped = (current * factor).clamp(self.zoom_limits.0, self.zoom_limits.1);
        let factor = clamped / current;
        if (factor - 1.0).abs() < ZOOM_EPSILON {
            return None;
        }
        // Zoom about the cursor: the canvas point under `origin` stays under it.
        Some(
            Affine::translate(origin.to_vec2())
                * Affine::scale(factor)
                * Affine::translate(-origin.to_vec2())
                * self.view,
        )
    }

    /// Zooms around a fixed point given in viewport coordinates.
    ///
    /// The canvas point under `origin` stays under `origin`, which is what makes
    /// wheel-zoom feel anchored to the cursor.
    pub fn zoom_around(this: &mut WidgetMut<'_, Self>, origin: Point, factor: f64) {
        if let Some(view) = this.widget.zoomed_view(origin, factor) {
            Self::set_view(this, view);
        }
    }

    /// Moves a child to a new canvas-space position.
    ///
    /// Only the moved child is affected: Masonry re-places it in the next layout,
    /// and no other child's scene is re-encoded.
    pub fn move_child(this: &mut WidgetMut<'_, Self>, index: usize, pos: Point) {
        let mut content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.store_child_pos(index, pos).apply(&mut content.ctx);
    }

    /// Moves a child from the parent's raw context, without telling the model.
    ///
    /// The seam an operator layer moves nodes through (§38.3). By then the operator
    /// has already written the position to the model — that is where the truth lives
    /// (§30) — and what is left is this view's own copy of the geometry. Three ways
    /// in, and the difference is who has already been told:
    ///
    /// * [`move_child`](Self::move_child) — a `WidgetMut`, from outside any pass;
    /// * this one — the parent widget, holding an `EventCtx`, in the same event;
    /// * the canvas's own drag, which also calls [`NodeSource::moved`] because there the canvas is the one that heard
    ///   the user.
    ///
    /// Nothing is broadcast to the other views of the same model: the caller wrote the
    /// model and knows who else is looking at it.
    pub fn move_child_raw(&mut self, index: usize, pos: Point, ctx: &mut RawCtx<'_>) {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        content.store_child_pos(index, pos).apply(&mut raw);
    }

    /// Pans the view from the parent's raw context.
    ///
    /// The view twin of [`move_child_raw`](Self::move_child_raw), and it exists for the
    /// same caller: an operator layer that has taken the primary button owns panning
    /// too, and the driver holding an `EventCtx` is the one that has to carry it in.
    /// Only the canvas is dirtied — child positions are in canvas coordinates, so a
    /// view change moves nobody (§22).
    pub fn pan_raw(&mut self, delta: Vec2, ctx: &mut RawCtx<'_>) {
        let view = Affine::translate(delta) * self.view;
        if self.store_view(view) {
            ctx.request_layout();
        }
    }

    /// Reaches node `index`'s widget, if it currently has one.
    ///
    /// The way a change in the model reaches a view that is already on screen. A node
    /// widget is built from the model and then keeps its own copy — it has to, because
    /// it is painted far more often than it is built — so a model change that happens
    /// while the node is materialised has to be pushed into it.
    ///
    /// Returns `false` when the node has no widget: it is off screen, or the canvas is
    /// in the far field. That is not a failure and needs no repair — a node without a
    /// widget reads the model when it is next built. What a far-field canvas *draws*
    /// comes from [`NodeSource::paint_far`], and if a change affects that drawing the
    /// caller invalidates it by moving the node, not by this.
    pub fn update_child(
        this: &mut WidgetMut<'_, Self>,
        index: usize,
        f: impl FnOnce(WidgetMut<'_, dyn Widget>),
    ) -> bool {
        let mut content = this.ctx.get_mut(&mut this.widget.content);
        let Some(pod) = content
            .widget
            .slots
            .get_mut(index)
            .filter(|slot| slot.alive)
            .and_then(|slot| slot.pod.as_mut())
        else {
            return false;
        };
        f(content.ctx.get_mut(pod));
        true
    }

    /// The nodes that currently have a widget, as `(index, widget id)` pairs.
    ///
    /// Useful for tests and for apps that need to reach into a live node. The list
    /// changes as nodes scroll in and out, so ids must not be cached across frames.
    pub fn live_children(this: &mut WidgetMut<'_, Self>) -> Vec<(usize, WidgetId)> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content
            .widget
            .live
            .iter()
            .filter_map(|&i| content.widget.slots[i].pod.as_ref().map(|pod| (i, pod.id())))
            .collect()
    }

    // --- MARK: STRUCTURE

    /// Puts a node into the canvas under the name `index`, at `pos` with size `size`.
    ///
    /// **The name is the caller's** (§43): the model is the truth and a name is part of
    /// it, so whoever owns the model hands out names — reusing the ones its own removals
    /// freed — and this array follows. A name beyond the end leaves holes behind it, and
    /// they are the names the next insertions will use.
    ///
    /// Re-inserting a live name replaces what was there, which is what undo of a removal
    /// needs and what a second insertion under the same name has no business doing.
    pub fn insert_node(this: &mut WidgetMut<'_, Self>, index: usize, pos: Point, size: Size) {
        {
            let mut content = this.ctx.get_mut(&mut this.widget.content);
            let invalidate = content.widget.insert_node(index, pos, size);
            invalidate.apply(&mut content.ctx);
        }
        this.ctx.request_layout();
    }

    /// Takes the node named `index` out, with every link that ended on it.
    ///
    /// The links come back as `(name, link)` pairs so that undo can put them back under
    /// the names they had — the same rule as the node's own name (§41.2). Its widget, if
    /// it had one, leaves the tree in the next mutate pass.
    pub fn remove_node(this: &mut WidgetMut<'_, Self>, index: usize) -> Vec<(usize, Link)> {
        let removed = {
            let mut content = this.ctx.get_mut(&mut this.widget.content);
            let removed = content.widget.remove_node(index);
            Invalidate::LayoutAndPaint.apply(&mut content.ctx);
            removed
        };
        this.ctx.request_layout();
        removed.into_iter().map(|(name, link)| (name as usize, link)).collect()
    }

    /// Adds a link and returns the name it got.
    pub fn insert_link(this: &mut WidgetMut<'_, Self>, link: Link) -> usize {
        let name = {
            let mut content = this.ctx.get_mut(&mut this.widget.content);
            let name = content.widget.insert_link(link);
            Invalidate::LayoutAndPaint.apply(&mut content.ctx);
            name
        };
        this.ctx.request_layout();
        name as usize
    }

    /// Puts a link back under the name it had, for undo.
    pub fn restore_link(this: &mut WidgetMut<'_, Self>, name: usize, link: Link) {
        {
            let mut content = this.ctx.get_mut(&mut this.widget.content);
            content.widget.restore_link(name as u32, link);
            Invalidate::LayoutAndPaint.apply(&mut content.ctx);
        }
        this.ctx.request_layout();
    }

    /// Removes the link named `name`, and hands back what it was.
    pub fn remove_link(this: &mut WidgetMut<'_, Self>, name: usize) -> Option<Link> {
        let link = {
            let mut content = this.ctx.get_mut(&mut this.widget.content);
            let link = content.widget.remove_link(name as u32);
            Invalidate::LayoutAndPaint.apply(&mut content.ctx);
            link
        };
        this.ctx.request_layout();
        link
    }

    /// The name of a live link between two nodes, in either direction.
    ///
    /// A pick already answers with both the name and the ends ([`CanvasHit::Link`]);
    /// this is for a caller that has the ends and wants the name — undo, or an
    /// application whose model names links by their endpoints.
    pub fn link_name(this: &mut WidgetMut<'_, Self>, link: Link) -> Option<usize> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.links.name_of(link).map(|name| name as usize)
    }

    /// What is under a point given in this widget's coordinates.
    ///
    /// Nodes first, then links; `None` for empty canvas. Answered from the model,
    /// so it works below the far-field threshold where no node has a widget, and for
    /// links, which never do.
    pub fn hit_test(this: &mut WidgetMut<'_, Self>, pos: Point) -> Option<CanvasHit> {
        let canvas_pos = this.widget.view.inverse() * pos;
        let scale = this.widget.hit_scale(this.ctx.window_transform());
        let content = this.ctx.get_mut(&mut this.widget.content);
        let hit = content.widget.hit(canvas_pos, scale);
        publish_hit_stats(&this.widget.stats, content.widget);
        hit
    }

    /// The canvas-space position of a node, or `None` for a name nothing holds.
    ///
    /// A removed node answers `None` from the moment it is removed, not from the moment
    /// its widget leaves the tree: the slot outlives the node by design (§43), and a
    /// caller asking where a deleted node is has to hear that it is nowhere.
    pub fn child_pos(this: &mut WidgetMut<'_, Self>, index: usize) -> Option<Point> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.live_slot(index).map(|slot| slot.pos)
    }

    // --- MARK: INTERNAL

    /// Records a new view transform. Returns `true` if a layout pass is needed.
    ///
    /// Split out because the two entry points hold different context types — a
    /// `WidgetMut` from the public API, an `EventCtx` from the pointer handler — and
    /// only the "who do I tell" half differs between them.
    #[must_use]
    fn store_view(&mut self, view: Affine) -> bool {
        if self.view == view {
            return false;
        }
        self.view = view;
        self.view_dirty = true;
        true
    }

    /// Applies a new view transform from an event handler.
    fn apply_view(&mut self, view: Affine, ctx: &mut EventCtx<'_>) {
        if self.store_view(view) {
            ctx.request_layout();
        }
    }

    /// Moves a child from an event handler — the drag, as opposed to the programmatic
    /// [`move_child`](Self::move_child).
    ///
    /// The two differ in exactly one thing and it is the point of the split: a drag is
    /// the *user* moving a node, so the model has to hear about it and so do the other
    /// canvases showing the same model. A programmatic move is what those other
    /// canvases then receive, and it must not bounce back out again.
    fn move_child_at(&mut self, index: usize, pos: Point, ctx: &mut EventCtx<'_>) {
        let mut peers = std::mem::take(&mut self.peers);
        peers.clear();
        {
            let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
            let invalidate = content.store_child_pos(index, pos);
            if invalidate != Invalidate::Nothing {
                content.source.moved(index, pos, &mut peers);
            }
            invalidate.apply(&mut raw);
        }
        for &peer in &peers {
            // A mutate callback rather than a direct reach: another canvas is not this
            // widget's child, and the mutate pass is where a widget outside the current
            // subtree may legally be changed. It runs before the next layout, so the
            // other areas move in the same frame.
            ctx.mutate_later(peer, move |mut widget| {
                let mut canvas = widget.downcast::<Self>();
                Self::move_child(&mut canvas, index, pos);
            });
        }
        self.peers = peers;
    }

    /// How many screen pixels one canvas unit covers.
    ///
    /// The zoom is only part of it: the canvas may itself be scaled by whatever it
    /// sits inside — a region with its own `ui_scale`, a device scale factor — and a
    /// tolerance in screen pixels has to account for the whole chain. The content
    /// widget's own transform is exactly this product, but reading it back through
    /// the arena during an event would cost more than multiplying two numbers.
    fn hit_scale(&self, window: Affine) -> f64 {
        scale_of(window) * self.zoom()
    }

    /// Marks the node under the pointer, so only it gets interactive controls.
    ///
    /// Returns `true` if the active node changed.
    fn set_active(&mut self, active: Option<usize>, ctx: &mut EventCtx<'_>) -> bool {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        if content.active == active || content.far.active {
            return false;
        }
        content.active = active;
        content.detail_dirty = true;
        raw.request_layout();
        true
    }

    /// Picks what is under a point, from an event handler.
    fn hit_at(&mut self, pos: Point, ctx: &mut EventCtx<'_>) -> Option<CanvasHit> {
        let canvas_pos = self.view.inverse() * pos;
        let scale = self.hit_scale(ctx.window_transform());
        let (content, _) = ctx.get_raw_mut(&mut self.content);
        let hit = content.hit(canvas_pos, scale);
        publish_hit_stats(&self.stats, content);
        hit
    }

    /// Records what the pointer is over and asks for whatever that changes.
    ///
    /// A hover changes pixels — the link under the pointer is highlighted — and with
    /// `controls_on_hover` it also changes which node has real controls, which is a
    /// layout. Keeping the two apart is the same distinction the link layer makes
    /// between repainting a curve and re-choosing the set (§24.3): a highlight must
    /// not drag a relayout of the graph behind it.
    fn hover(&mut self, pos: Point, ctx: &mut EventCtx<'_>) -> Option<CanvasHit> {
        let hit = self.hit_at(pos, ctx);
        self.set_hovered(hit, ctx);
        if self.controls_on_hover {
            self.set_active(hit.and_then(CanvasHit::node), ctx);
        }
        hit
    }

    /// Stores what the pointer is over, repainting if the highlight changed.
    fn set_hovered(&mut self, hit: Option<CanvasHit>, ctx: &mut EventCtx<'_>) {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        if content.set_hovered(hit) {
            raw.request_paint_only();
        }
        publish_hit_stats(&self.stats, content);
    }
}

impl Widget for CanvasLayer {
    type Action = NoAction;

    /// Handles pan, zoom and node dragging.
    ///
    /// This runs *after* the event has been offered to the widget under the pointer
    /// and bubbled up, so a slider inside a node gets first refusal: if it marked
    /// the event handled, the canvas leaves it alone. That is what makes claim 3
    /// work — controls inside nodes need no cooperation from the canvas.
    fn on_pointer_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &PointerEvent) {
        match event {
            PointerEvent::Down(e) if !ctx.is_handled() => {
                // A control inside a node may have taken pointer capture without
                // marking the event handled — `Checkbox` and `Slider` both do
                // exactly that. Starting a drag here would steal the capture out
                // from under them and break every control on the canvas, so the
                // capture target is the signal to defer to, not `is_handled`.
                if ctx.pointer_capture_target_id().is_some_and(|id| id != ctx.widget_id()) {
                    return;
                }
                let pos = ctx.local_position(e.state.position);
                let canvas_pos = self.view.inverse() * pos;
                // A press picks whatever a move would have picked, whether or not the
                // canvas is going to act on it. The record is what an operator layer
                // above reads as its context (§38.3): a driver holding an `EventCtx`
                // cannot hit-test a child, and this costs the pick the drag decision
                // needed anyway.
                let hit = self.hover(pos, ctx);
                self.drag = match e.button {
                    // Left button drags a node if there is one under the pointer,
                    // and pans otherwise — unless the application has taken the
                    // primary button for its keymap.
                    Some(PointerButton::Primary) if self.builtin_gestures => match hit {
                        Some(CanvasHit::Node { index, pos: child_pos }) => Drag::Node {
                            index,
                            grab: canvas_pos - child_pos,
                        },
                        // A link is pickable but not draggable by the canvas:
                        // selection and rewiring are operators (§11). A press on a
                        // curve pans, as it did before curves could be picked at all.
                        Some(CanvasHit::Link { .. }) | None => Drag::Pan { last: pos },
                    },
                    // Middle button always pans, as in Blender. Not a gesture an
                    // operator layer competes for, so it is not switched off with the
                    // others.
                    Some(PointerButton::Auxiliary) => Drag::Pan { last: pos },
                    _ => Drag::None,
                };
                if self.drag != Drag::None {
                    ctx.capture_pointer();
                    ctx.set_handled();
                }
            },
            PointerEvent::Move(PointerUpdate { current, .. }) => {
                let pos = ctx.local_position(current.position);
                match self.drag {
                    Drag::None => {
                        self.hover(pos, ctx);
                    },
                    Drag::Pan { last } => {
                        self.drag = Drag::Pan { last: pos };
                        let view = Affine::translate(pos - last) * self.view;
                        self.apply_view(view, ctx);
                        ctx.set_handled();
                    },
                    Drag::Node { index, grab } => {
                        let canvas_pos = self.view.inverse() * pos;
                        self.move_child_at(index, canvas_pos - grab, ctx);
                        ctx.set_handled();
                    },
                }
            },
            PointerEvent::Leave(_) => {
                self.set_hovered(None, ctx);
                if self.controls_on_hover {
                    self.set_active(None, ctx);
                }
            },
            PointerEvent::Up(_) | PointerEvent::Cancel(_) => {
                if self.drag != Drag::None {
                    self.drag = Drag::None;
                    ctx.release_pointer();
                    ctx.set_handled();
                }
            },
            PointerEvent::Scroll(PointerScrollEvent { delta, state, .. }) if !ctx.is_handled() => {
                // Wheel notches are converted the same way `Portal` does it, so the
                // zoom speed matches the platform's idea of a scroll step.
                let scale_factor = ctx.scale_factor();
                let line_px = PhysicalPosition {
                    x: WHEEL_LINE_PX * scale_factor,
                    y: WHEEL_LINE_PX * scale_factor,
                };
                let viewport = self.viewport;
                let page_px = PhysicalPosition {
                    x: viewport.width * scale_factor,
                    y: viewport.height * scale_factor,
                };
                let delta_px = delta.to_pixel_delta(line_px, page_px);
                let LogicalPosition { y, .. } = delta_px.to_logical::<f64>(scale_factor);
                if y == 0.0 {
                    return;
                }

                let origin = ctx.local_position(state.position);
                let Some(view) = self.zoomed_view(origin, (-y * WHEEL_ZOOM_RATE).exp()) else {
                    return;
                };
                self.apply_view(view, ctx);
                ctx.set_handled();
            },
            _ => {},
        }
    }

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        // The viewport fills whatever space it is given.
        match len_req {
            LenReq::MinContent => Length::ZERO,
            LenReq::MaxContent => match axis {
                Axis::Horizontal => 800.0.px(),
                Axis::Vertical => 600.0.px(),
            },
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        self.viewport = size;

        // The first layout is the first moment this widget knows its own id and can
        // hand it to the source. Construction is too early: a `CanvasLayer` is built
        // before it is a widget, and the id is minted when it enters the tree.
        if !self.attached {
            self.attached = true;
            let id = ctx.widget_id();
            let (content, _) = ctx.get_raw_mut(&mut self.content);
            content.source.attached(id);
        }

        // Clip to the viewport so children panned out of view cannot paint over the
        // surrounding UI, and so Masonry excludes them from hit testing.
        ctx.set_clip_path(Rect::from_origin_size(Point::ORIGIN, size));

        let live_rect = self.live_canvas_rect();
        let zoom = self.zoom();
        // Only the readability half of the decision can be taken here: the cost half
        // needs the number of visible nodes, which the cull computes (§29.2).
        let readable = self.thresholds.for_scale(zoom);
        let view = self.view;
        let view_dirty = std::mem::take(&mut self.view_dirty);

        // Push the view down to the content widget. `set_transform` marks it as
        // needing compose, which runs after layout — so this does not violate the
        // "don't set flags for an earlier pass" rule that `get_raw_mut` warns about.
        // Culling belongs here, not in the content widget: it depends on the view and
        // the viewport, both of which live on this side. Doing it in the content's
        // own `layout` would mean asking the content to re-lay-out on every pan — and
        // Masonry marks anything that re-lays-out for repaint (`passes/layout.rs`,
        // "TODO - Not everything that has been re-laid out needs to be repainted").
        // That is what made a far-field pan re-record its scene every frame.
        //
        // Child positions are in canvas coordinates, so a view change moves nobody:
        // the transform does all the work and no layout is needed at all.
        let needs_mutate = {
            let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
            if let Some(links) = self.pending_links.take() {
                let count = content.slots.len();
                content.links = LinkLayer::new(links, count);
                content.links.invalidate();
            }
            content.link_style = self.link_style;
            content.far_overscan = self.far_overscan;
            content.links.set_slack(region_slack(self.far_overscan));
            content.controls_on_hover = self.controls_on_hover;
            content.live_rect = live_rect;
            content.scale = zoom;
            content.readable = Some(readable);
            content.budget = self.budget;
            if view_dirty {
                raw.set_transform(view);
            }

            content.cull();
            if std::mem::take(&mut content.far.dirty) | content.links.take_repaint() {
                raw.request_paint_only();
            }
            content.pending.is_some()
        };
        if needs_mutate {
            // Adding and removing children needs a `WidgetMut`, which layout does not
            // have. The mutate pass runs before the next layout pass in the same
            // rewrite loop, so a node entering the view is built and placed in the
            // same frame.
            ctx.mutate_child_later(&mut self.content, |mut content| {
                CanvasContent::apply_pending(&mut content);
            });
        }

        let content_size = ctx.compute_size(&mut self.content, SizeDef::fixed(size), size.into());
        ctx.run_layout(&mut self.content, content_size);
        ctx.place_child(&mut self.content, Point::ORIGIN);

        let (content, _) = ctx.get_raw(&mut self.content);
        self.stats.set(CanvasStats {
            total: content.node_count(),
            materialised: content.live.len(),
            visible: content.visible.len(),
            detail: content.detail,
            zoom,
            recorded_far: content.far.nodes.len(),
            recorded_links: content.links.recorded().len(),
            hidden_links: content.links.hidden(),
            hovered: content.hovered,
            counters: CanvasCounters {
                content_layouts: content.layouts,
                child_layouts: content.child_layouts,
                composes: content.composes,
                builds: content.builds,
                node_edits: content.node_edits,
                link_edits: content.link_edits,
                link_compactions: content.links.edit_counters().0,
                edit_edge_scans: content.links.edit_counters().1,
                level_switches: content.level_switches,
                far_repaints: content.far_repaints,
                far_records: content.far_records,
                slot_visits: content.visits,
                link_repaints: content.link_repaints,
                link_reselects: content.links.refreshes(),
                hit_queries: content.hit_queries,
                hit_node_tests: content.hit_node_tests,
                hit_curve_tests: content.hit_curve_tests,
                hit_curve_scans: content.hit_curve_scans,
            },
        });
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.content);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.content.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}
