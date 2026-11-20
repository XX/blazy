//! [`CanvasLayer`]: the viewport — fixed size, clip path, and the view it owns.

use std::cell::Cell;

use blazy_shape::scale_of;
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AllowRawMut, ChildrenIds, EventCtx, LayoutCtx, MeasureCtx, NoAction, PaintCtx, PointerEvent,
    PropertiesMut, PropertiesRef, RawCtx, RegisterCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Affine, Axis, Point, Rect, Size, Vec2};
use masonry::layout::{AsUnit, LenReq, Length, SizeDef};
use masonry::ui_events::pointer::{PointerButton, PointerScrollEvent, PointerUpdate};

use super::*;
use crate::detail::{DetailBudget, DetailThresholds};
use crate::links::{Link, LinkLayer, LinkStyle};
use crate::source::NodeSource;
use crate::stats::{CanvasCounters, CanvasHit, CanvasStats};

/// So a parent driving gestures from above can reach the canvas inside its own event,
/// without a `mutate_later` per node.
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

/// A canvas of freely positioned children with a pan/zoom view: the viewport, which clips
/// its content and pushes the view down to its [`CanvasContent`] during layout.
pub struct CanvasLayer {
    content: WidgetPod<CanvasContent>,
    /// Canvas-space to viewport-space transform (pan and zoom).
    view: Affine,
    /// Whether `view` still needs pushing down to the content widget.
    view_dirty: bool,
    /// Viewport size in widget coordinates, set during layout.
    viewport: Size,
    /// How far past the viewport to keep nodes alive, as a fraction of the viewport.
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
    /// Whether the wheel zooms the view here, or is left to an operator layer above.
    wheel_zoom: bool,
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
    /// Where each node is, by name — kept from [`new`](Self::new) for a
    /// [`reset`](Self::reset), which reads the whole graph again.
    geometry: Box<dyn FnMut(usize) -> Option<(Point, Size)>>,
    link_style: LinkStyle,
    ports: PortLayout,
}

impl CanvasLayer {
    /// Creates a canvas over `count` names.
    ///
    /// `geometry` gives each node's position and size (`None` for a name nothing holds), and
    /// `source` builds its widget when it scrolls into view. The canvas keeps `geometry` for
    /// [`reset`](Self::reset).
    pub fn new(
        count: usize,
        geometry: impl FnMut(usize) -> Option<(Point, Size)> + 'static,
        source: impl NodeSource,
    ) -> Self {
        let mut geometry: Box<dyn FnMut(usize) -> Option<(Point, Size)>> = Box::new(geometry);
        let slots = slots_of(count, &mut geometry);
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
            wheel_zoom: true,
            thresholds: DetailThresholds::default(),
            budget: DetailBudget::default(),
            attached: false,
            peers: Vec::new(),
            zoom_limits: (0.02, 8.0),
            pending_links: None,
            link_style: LinkStyle::default(),
            ports: PortLayout::default(),
            geometry,
        }
    }

    /// Starts over from the graph, for a graph replaced as a whole: `count` names read again
    /// from the geometry callback, and `links`.
    ///
    /// Every node widget, class and stacking stamp goes; the view stays.
    pub fn reset(this: &mut WidgetMut<'_, Self>, count: usize, links: Vec<Link>) {
        let slots = slots_of(count, &mut this.widget.geometry);
        {
            let mut content = this.ctx.get_mut(&mut this.widget.content);
            CanvasContent::reset(&mut content, slots, links);
        }
        this.ctx.request_layout();
    }

    /// Adds edges between nodes; an edge naming a missing node is skipped when drawn.
    pub fn with_links(mut self, links: Vec<Link>) -> Self {
        self.pending_links = Some(links);
        self
    }

    /// Sets how far past the viewport the far field and the link set are recorded, as a
    /// fraction of the viewport: re-recordings against path segments per frame.
    pub fn with_far_overscan(mut self, fraction: f64) -> Self {
        self.far_overscan = fraction.max(0.0);
        self
    }

    /// The default of [`Self::with_far_overscan`]: a quarter of the viewport on each side.
    pub const DEFAULT_FAR_OVERSCAN: f64 = FAR_OVERSCAN;

    /// Where the ports are on a node, and whether they are drawn.
    #[must_use]
    pub fn with_ports(mut self, ports: PortLayout) -> Self {
        self.ports = ports;
        self
    }

    /// The port layout this canvas uses.
    pub fn ports(&self) -> PortLayout {
        self.ports
    }

    /// Restyles the links.
    pub fn with_link_style(mut self, style: LinkStyle) -> Self {
        self.link_style = style;
        self
    }

    /// Builds interactive controls only for the node under the pointer; the others get
    /// their `Simplified` form. Off by default.
    ///
    /// Several times cheaper per frame, but the stand-in is swapped for real widgets as the
    /// pointer arrives, so it has to look like them.
    pub fn with_controls_on_hover(mut self, enabled: bool) -> Self {
        self.controls_on_hover = enabled;
        self
    }

    /// Whether the canvas acts on the primary button itself (drag a node, or pan). On by
    /// default; an operator layer above turns it off. The middle button, the wheel and the
    /// pick stay.
    pub fn with_builtin_gestures(mut self, enabled: bool) -> Self {
        self.builtin_gestures = enabled;
        self
    }

    /// Whether the canvas zooms on the wheel itself. On by default; off, the scroll bubbles
    /// up to an operator layer.
    #[must_use]
    pub fn with_wheel_zoom(mut self, enabled: bool) -> Self {
        self.wheel_zoom = enabled;
        self
    }

    /// Sets where the detail levels switch over for readability.
    pub fn with_thresholds(mut self, thresholds: DetailThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Sets the ceiling on widgets this canvas may keep in the tree. Several canvases in one
    /// window should share one budget ([`DetailBudget::split`]).
    pub fn with_budget(mut self, budget: DetailBudget) -> Self {
        self.budget = budget;
        self
    }

    /// The cost ceiling in force.
    pub fn budget(&self) -> DetailBudget {
        self.budget
    }

    /// The same canvas, showing what `view` says.
    #[must_use]
    pub fn with_view(mut self, view: Affine) -> Self {
        self.view = view;
        self.view_dirty = true;
        self
    }

    /// The canvas-space to viewport-space transform.
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

    /// Sets the view transform. Lays out the canvas (culling depends on the view) but not
    /// the content, whose children are in canvas coordinates.
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

    /// The view that results from zooming about `origin`, or `None` at the zoom limit.
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

    /// Zooms about `origin`, in viewport coordinates: the canvas point under it stays.
    pub fn zoom_around(this: &mut WidgetMut<'_, Self>, origin: Point, factor: f64) {
        if let Some(view) = this.widget.zoomed_view(origin, factor) {
            Self::set_view(this, view);
        }
    }

    /// Moves a child to a new canvas-space position.
    pub fn move_child(this: &mut WidgetMut<'_, Self>, index: usize, pos: Point) {
        let mut content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.store_child_pos(index, pos).apply(&mut content.ctx);
    }

    /// Gives a node a new rectangle.
    pub fn set_child_rect(this: &mut WidgetMut<'_, Self>, index: usize, rect: Rect) {
        {
            let mut content = this.ctx.get_mut(&mut this.widget.content);
            content.widget.store_child_rect(index, rect).apply(&mut content.ctx);
        }
        this.ctx.request_layout();
    }

    /// Moves a child from the parent's raw context, without telling the model or other
    /// views: for an operator layer that has written the model already.
    pub fn move_child_raw(&mut self, index: usize, pos: Point, ctx: &mut RawCtx<'_>) {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        content.store_child_pos(index, pos).apply(&mut raw);
    }

    /// Zooms the view about `origin` from the parent's raw context.
    pub fn zoom_raw(&mut self, origin: Point, factor: f64, ctx: &mut RawCtx<'_>) {
        if let Some(view) = self.zoomed_view(origin, factor)
            && self.store_view(view)
        {
            ctx.request_layout();
        }
    }

    /// Pans the view from the parent's raw context, for an operator layer that owns panning.
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

    /// Puts `class` on node `index`'s widget or takes it off — now, and on every widget it is
    /// built with later — for a [`Selector`](masonry::core::Selector) to style. Returns
    /// whether anything changed.
    pub fn set_node_class(this: &mut WidgetMut<'_, Self>, index: usize, class: &'static str, on: bool) -> bool {
        let mut content = this.ctx.get_mut(&mut this.widget.content);
        let changed = CanvasContent::set_node_class(&mut content, index, class, on);
        // Published now: a class change causes no layout.
        let mut stats = this.widget.stats.get();
        stats.counters.class_changes = content.widget.class_changes;
        this.widget.stats.set(stats);
        changed
    }

    /// Puts node `index` above every other node of this canvas, and says whether its place
    /// changed. For a canvas that is the only view of its graph; several views share stamps
    /// through [`stack_node`](Self::stack_node).
    pub fn raise_node(this: &mut WidgetMut<'_, Self>, index: usize) -> bool {
        Self::restack_with(this, index, |content| content.raise_node(index))
    }

    /// Gives node `index` the stacking stamp `stamp` (nodes stack by `(stamp, name)`; zero
    /// for never raised), and says whether its place changed.
    pub fn stack_node(this: &mut WidgetMut<'_, Self>, index: usize, stamp: u64) -> bool {
        Self::restack_with(this, index, |content| content.stack_node(index, stamp))
    }

    fn restack_with(
        this: &mut WidgetMut<'_, Self>,
        index: usize,
        change: impl FnOnce(&mut CanvasContent) -> bool,
    ) -> bool {
        let mut content = this.ctx.get_mut(&mut this.widget.content);
        if !change(content.widget) {
            return false;
        }
        // A repaint of the node is the cheapest way to get a frame for the new order.
        let fresh = content.widget.fresh.contains(&index);
        if let Some(pod) = content.widget.slots[index].pod.as_mut() {
            if fresh {
                let id = pod.id();
                content
                    .ctx
                    .mutate_later(id, |mut widget| widget.ctx.request_paint_only());
            } else {
                content.ctx.get_mut(pod).ctx.request_paint_only();
            }
        }
        let mut stats = this.widget.stats.get();
        stats.counters.raises = content.widget.raises;
        this.widget.stats.set(stats);
        true
    }

    /// Node `index`'s stacking stamp, as this canvas remembers it: nodes stack by
    /// `(stamp, name)`, and zero is a node no one raised.
    pub fn node_stacking(this: &mut WidgetMut<'_, Self>, index: usize) -> u64 {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content
            .widget
            .slots
            .get(index)
            .filter(|slot| slot.alive)
            .map_or(0, |slot| slot.raised)
    }

    /// Whether node `index` wears `class`, as the canvas remembers it.
    pub fn node_has_class(this: &mut WidgetMut<'_, Self>, index: usize, class: &str) -> bool {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content
            .widget
            .slots
            .get(index)
            .is_some_and(|slot| slot.alive && slot.classes.contains(&class))
    }

    /// Tells the canvas that what node `index` holds has changed: its widget is rebuilt from
    /// the [`NodeSource`] (or the far field re-recorded). Returns whether anything will be.
    pub fn refresh_node(this: &mut WidgetMut<'_, Self>, index: usize) -> bool {
        let refreshed = {
            let mut content = this.ctx.get_mut(&mut this.widget.content);
            let invalidate = content.widget.refresh_node(index);
            let refreshed = !matches!(invalidate, Invalidate::Nothing);
            invalidate.apply(&mut content.ctx);
            refreshed
        };
        if refreshed {
            // The cull, in this widget's layout, turns the mark into a rebuild.
            this.ctx.request_layout();
        }
        refreshed
    }

    /// The nodes that currently have a widget, as `(index, widget id)` pairs. Ids change as
    /// nodes scroll in and out.
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

    /// Puts a node into the canvas under the name `index` (the model's), at `pos` with size
    /// `size`. Inserting a live name again only updates its geometry.
    pub fn insert_node(this: &mut WidgetMut<'_, Self>, index: usize, pos: Point, size: Size) {
        {
            let mut content = this.ctx.get_mut(&mut this.widget.content);
            let invalidate = content.widget.insert_node(index, pos, size);
            invalidate.apply(&mut content.ctx);
            // A node landing in view is built now, not by the next layout's cull, which saves
            // a rewrite pass (a menu entry in a two-area window needs it).
            let rect = Rect::from_origin_size(pos, size);
            if content.widget.readable.is_some() && content.widget.live_rect.overlaps(rect) {
                content.widget.cull();
                if content.widget.pending.is_some() {
                    CanvasContent::apply_pending(&mut content);
                }
            }
        }
        this.ctx.request_layout();
    }

    /// Takes the node named `index` out with its links, returned as `(name, link)` for undo.
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
    pub fn link_name(this: &mut WidgetMut<'_, Self>, link: Link) -> Option<usize> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.links.name_of(link).map(|name| name as usize)
    }

    /// What is under a point given in this widget's coordinates, from the model.
    pub fn hit_test(this: &mut WidgetMut<'_, Self>, pos: Point) -> Option<CanvasHit> {
        let canvas_pos = this.widget.view.inverse() * pos;
        let scale = this.widget.hit_scale(this.ctx.window_transform());
        let content = this.ctx.get_mut(&mut this.widget.content);
        let hit = content.widget.hit(canvas_pos, scale);
        publish_hit_stats(&this.widget.stats, content.widget);
        hit
    }

    /// What is under a point, from the parent's raw context: for an operator holding the
    /// pointer, while the canvas sees no events.
    pub fn hit_test_raw(&mut self, pos: Point, window: Affine, ctx: &mut RawCtx<'_>) -> Option<CanvasHit> {
        let canvas_pos = self.view.inverse() * pos;
        let scale = self.hit_scale(window);
        let (content, _) = ctx.get_raw_mut(&mut self.content);
        let hit = content.hit(canvas_pos, scale);
        publish_hit_stats(&self.stats, content);
        hit
    }

    /// The canvas-space position of a node, or `None` for a name nothing holds.
    pub fn child_pos(this: &mut WidgetMut<'_, Self>, index: usize) -> Option<Point> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.live_slot(index).map(|slot| slot.pos)
    }

    // --- MARK: INTERNAL

    /// Records a new view transform. Returns `true` if a layout pass is needed.
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

    /// Moves a child from the canvas's own drag: unlike [`move_child`](Self::move_child), the
    /// model and the other canvases hear about it.
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
            // Another canvas is reached through the mutate pass, still within this frame.
            ctx.mutate_later(peer, move |mut widget| {
                let mut canvas = widget.downcast::<Self>();
                Self::move_child(&mut canvas, index, pos);
            });
        }
        self.peers = peers;
    }

    /// How many screen pixels one canvas unit covers: the zoom times whatever scales the
    /// canvas (a region's `ui_scale`, the device).
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

    /// Records what the pointer is over: a repaint for the highlight, a layout only when the
    /// node with controls changes.
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

    /// Handles pan, zoom and node dragging, after the widget under the pointer had its turn.
    fn on_pointer_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &PointerEvent) {
        match event {
            PointerEvent::Down(e) if !ctx.is_handled() => {
                // A control may capture without marking the event handled (`Checkbox`,
                // `Slider`): defer to the capture target.
                if ctx.pointer_capture_target_id().is_some_and(|id| id != ctx.widget_id()) {
                    return;
                }
                let pos = ctx.local_position(e.state.position);
                let canvas_pos = self.view.inverse() * pos;
                // A press picks, whether or not the canvas acts: an operator layer reads it.
                let hit = self.hover(pos, ctx);
                // Where the node a port belongs to is, for the drag below to grab it by.
                let port_node = match hit {
                    Some(CanvasHit::Port { index, .. }) => {
                        let (content, _) = ctx.get_raw(&mut self.content);
                        content.live_rect_of(index).map(|rect| (index, rect.origin()))
                    },
                    _ => None,
                };
                self.drag = match e.button {
                    // Left button drags a node, or pans, unless operators took the button.
                    Some(PointerButton::Primary) if self.builtin_gestures => match hit {
                        Some(CanvasHit::Node { index, pos: child_pos }) => Drag::Node {
                            index,
                            grab: canvas_pos - child_pos,
                        },
                        // Without operators a port is part of its node.
                        Some(CanvasHit::Port { .. }) => match port_node {
                            Some((index, origin)) => Drag::Node {
                                index,
                                grab: canvas_pos - origin,
                            },
                            None => Drag::Pan { last: pos },
                        },
                        // A press on a link pans: rewiring is an operator's.
                        Some(CanvasHit::Link { .. }) | None => Drag::Pan { last: pos },
                    },
                    // The middle button always pans, as in Blender.
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
            PointerEvent::Scroll(PointerScrollEvent { delta, state, .. }) if self.wheel_zoom && !ctx.is_handled() => {
                // Notches converted as `Portal` does.
                let y = wheel_pixels(*delta, ctx.scale_factor(), self.viewport);
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

        // The first moment this widget knows its id, to hand to the source.
        if !self.attached {
            self.attached = true;
            let id = ctx.widget_id();
            let (content, _) = ctx.get_raw_mut(&mut self.content);
            content.source.attached(id);
        }

        // Clip to the viewport, for painting and hit testing.
        ctx.set_clip_path(Rect::from_origin_size(Point::ORIGIN, size));

        let live_rect = self.live_canvas_rect();
        let zoom = self.zoom();
        // The readability half of the decision; the cull takes the cost half.
        let readable = self.thresholds.for_scale(zoom);
        let view = self.view;
        let view_dirty = std::mem::take(&mut self.view_dirty);

        // Push the view down (`set_transform` marks compose, which runs after layout).
        // Culling is here rather than in the content's layout: Masonry repaints whatever is
        // laid out, and a pan would re-record the far field every frame.
        let needs_mutate = {
            let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
            if let Some(links) = self.pending_links.take() {
                let count = content.slots.len();
                content.links = LinkLayer::new(links, count);
                content.links.invalidate();
            }
            content.link_style = self.link_style;
            content.ports = self.ports;
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
            // Children change in the mutate pass, which runs before the next layout.
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
                class_changes: content.class_changes,
                raises: content.raises,
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
                hit_port_tests: content.hit_port_tests,
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

/// A slot per name, from where the geometry callback says each node is.
fn slots_of(count: usize, geometry: &mut dyn FnMut(usize) -> Option<(Point, Size)>) -> Vec<Slot> {
    (0..count)
        .map(|i| match geometry(i) {
            Some((pos, size)) => Slot {
                alive: true,
                pos,
                size,
                pod: None,
                built: None,
                classes: Vec::new(),
                raised: 0,
            },
            None => EMPTY_SLOT,
        })
        .collect()
}
