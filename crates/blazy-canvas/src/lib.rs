//! A zoomable, pannable canvas of freely positioned widgets, for Masonry.
//!
//! This crate exists to answer the Phase 0 question from `rnd/architecture.md`:
//! can a Blender-style node editor be built on top of `masonry_core` without
//! forking it?
//!
//! Three claims are under test:
//!
//! 1. **Pan and zoom cost one `Affine`.** Changing the view sets a transform on the content widget. It must not
//!    re-encode any child's cached scene, and it must not re-run any child's `layout`. This is the payoff of a retained
//!    tree on top of a vector display list: the encoded scene stores curves, not triangles, so it stays sharp at any
//!    scale.
//!
//! 2. **Culling is mandatory, not an optimisation.** Masonry's per-widget scene cache saves the `paint()` call, but the
//!    paint pass still copies every visible widget's commands into the layer scene every frame (`passes/paint.rs`,
//!    `Scene::append_transformed`). Frame cost is proportional to the volume of *visible* commands, so off-screen nodes
//!    must be stashed to be skipped.
//!
//! 3. **Ordinary widgets work inside nodes.** Masonry already inverts `window_transform` when routing pointer events,
//!    so sliders and checkboxes inside a zoomed node need no special handling from us.
//!
//! # Structure
//!
//! The canvas is two widgets, not one:
//!
//! ```text
//! CanvasLayer      viewport: fixed size, clip path, owns the view. No transform.
//!   └ CanvasContent    carries the view transform; owns the placed children.
//! ```
//!
//! They cannot be merged. A widget's transform maps its own border-box into its
//! parent's space, and the paint pass transforms the clip path by that same
//! `window_transform` (`passes/paint.rs`). A single widget holding both the clip
//! and the view would zoom its own viewport clip along with the content.
//!
//! Because a `WidgetPod` hands its widget to the arena on insertion, the canvas
//! cannot read its own children through `&self`. Everything that needs child state
//! is therefore an associated function taking a [`WidgetMut`], which is the normal
//! Masonry idiom.
//!
//! # What this is not
//!
//! Not a finished node editor. Culling is a linear scan rather than a spatial
//! index ([`CanvasContent::cull`]), and there is no link layer, selection model or
//! serialisation. Those belong to `blazy-canvas` proper, once Phase 0 has answered
//! the feasibility question.

use std::any::TypeId;
use std::cell::Cell;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AllowRawMut, ChildrenIds, ComposeCtx, EventCtx, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx,
    PointerEvent, PropertiesMut, PropertiesRef, Property, RegisterCtx, UpdateCtx, Widget, WidgetMut, WidgetPod,
};
use masonry::dpi::{LogicalPosition, PhysicalPosition};
use masonry::imaging::Painter;
use masonry::kurbo::{Affine, Axis, Point, Rect, Size, Vec2};
use masonry::layout::{AsUnit, LenReq, Length, SizeDef};
use masonry::ui_events::pointer::{PointerButton, PointerScrollEvent, PointerUpdate};

/// How much detail a canvas child should draw at the current zoom level.
///
/// Level of detail serves two purposes, and the second matters more. The obvious
/// one is fewer draw commands per node. The important one is that at
/// [`Detail::Box`] a node can stash its contents entirely — and layout, not
/// painting, is what makes a large graph expensive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Detail {
    /// Full contents: header, body and interactive controls.
    Full,
    /// Header only; controls are stashed.
    Simplified,
    /// A flat filled rectangle. Contents are stashed and not laid out.
    Box,
}

impl Detail {
    /// Chooses a detail level for an effective scale factor.
    ///
    /// Thresholds are picked so controls disappear slightly before they become too
    /// small to hit, rather than after.
    pub fn for_scale(scale: f64) -> Self {
        if scale > 0.6 {
            Self::Full
        } else if scale > 0.25 {
            Self::Simplified
        } else {
            Self::Box
        }
    }
}

/// The detail level a canvas child should render at.
///
/// The canvas sets this property on every child when the zoom crosses a threshold.
/// Children opt in by reading it in `layout`/`paint` and handling it in
/// [`Widget::property_changed`]; children that ignore it simply always draw in full.
///
/// A property rather than a trait method, so the canvas can host heterogeneous
/// children. It is also the same mechanism `rnd/architecture.md` §9 earmarks for
/// per-region `ui_scale`, which has the same shape: a value that flows down a
/// subtree and invalidates layout when it changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanvasDetail(pub Detail);

impl Property for CanvasDetail {
    fn static_default() -> &'static Self {
        static DEFAULT: CanvasDetail = CanvasDetail(Detail::Full);
        &DEFAULT
    }
}

impl Default for CanvasDetail {
    fn default() -> Self {
        *Self::static_default()
    }
}

impl CanvasDetail {
    /// Helper for [`Widget::property_changed`]: requests a relayout when the detail
    /// level changed.
    pub fn prop_changed(ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        if property_type == TypeId::of::<Self>() {
            ctx.request_layout();
        }
    }
}

/// One item to place on the canvas.
pub struct CanvasItem {
    /// The widget to place.
    pub widget: NewWidget<dyn Widget>,
    /// Position of the top-left corner, in canvas coordinates.
    pub pos: Point,
    /// Size in canvas coordinates. Fixed, so the child is never measured.
    pub size: Size,
}

impl CanvasItem {
    /// Creates an item at a fixed canvas position and size.
    pub fn new(widget: NewWidget<impl Widget + ?Sized>, pos: Point, size: Size) -> Self {
        Self {
            widget: widget.erased(),
            pos,
            size,
        }
    }
}

/// Statistics from the last layout pass, for the Phase 0 measurements.
///
/// Deliberately cheap to collect: Phase 0 exists to produce numbers, and numbers
/// nobody can see are not evidence.
#[derive(Clone, Copy, Debug, Default)]
pub struct CanvasStats {
    /// Total number of children.
    pub total: usize,
    /// Children that survived culling and were laid out and painted.
    pub visible: usize,
    /// Layout passes run on the content widget since it was created.
    pub content_layouts: u64,
    /// Children that actually needed layout, summed over all passes.
    ///
    /// This is the number that matters. `run_layout_on` early-returns for a clean
    /// widget whose size is unchanged, so panning should leave this counter flat
    /// even though a layout pass did run. If it climbs with every pan, this design
    /// is wrong and Phase 0 has done its job.
    pub child_layouts: u64,
    /// Compose passes run on the content widget since it was created.
    pub composes: u64,
    /// Detail level applied at the last layout.
    pub detail: Option<Detail>,
    /// Current zoom factor.
    pub zoom: f64,
}

struct Child {
    pod: WidgetPod<dyn Widget>,
    /// Position of the child's top-left corner, in canvas coordinates.
    pos: Point,
    /// Size in canvas coordinates. Fixed, so children are never measured.
    size: Size,
    /// Whether this child survived the last cull.
    visible: bool,
}

// --- MARK: CONTENT

/// The transformed inner half of a [`CanvasLayer`].
///
/// Holds the freely positioned children and carries the pan/zoom transform. Not
/// constructed directly; a [`CanvasLayer`] owns one.
pub struct CanvasContent {
    children: Vec<Child>,
    /// Visible region in canvas coordinates, pushed down by the parent.
    visible_rect: Rect,
    /// Detail level pushed down by the parent.
    detail: Option<Detail>,
    layouts: Cell<u64>,
    child_layouts: Cell<u64>,
    composes: Cell<u64>,
    visible_count: Cell<usize>,
}

// `CanvasLayer` owns this widget completely and reaches into it during layout to
// push down the view transform, the visible rect and the detail level. This is the
// case the escape hatch is documented for: "a parent widget completely controls
// their child, but needs it to be a separate widget for user interaction to behave
// as expected".
impl AllowRawMut for CanvasContent {}

impl CanvasContent {
    fn new(children: Vec<Child>) -> Self {
        Self {
            children,
            visible_rect: Rect::ZERO,
            detail: None,
            layouts: Cell::new(0),
            child_layouts: Cell::new(0),
            composes: Cell::new(0),
            visible_count: Cell::new(0),
        }
    }

    /// Sets the detail property on every child.
    ///
    /// Only called on a zoom threshold crossing, so an O(n) walk is acceptable —
    /// unlike the per-frame work, which is culled first.
    pub fn set_detail(this: &mut WidgetMut<'_, Self>, detail: Detail) {
        for i in 0..this.widget.children.len() {
            let pod = &mut this.widget.children[i].pod;
            let mut child = this.ctx.get_mut(pod);
            child.insert_prop(CanvasDetail(detail));
        }
    }

    /// Marks children inside the visible rect and stashes the rest.
    ///
    /// A linear scan. For Phase 0 that is the honest baseline: it makes the cost of
    /// *not* having a spatial index visible in the measurements, so the decision to
    /// add one is driven by numbers rather than assumption. An R-tree drops in here
    /// without changing anything else.
    ///
    /// Stashing from `layout` is the same thing `virtual_scroll` does. The stashed
    /// pass runs before the layout pass inside the rewrite loop, so a child that
    /// becomes visible is unstashed and laid out within the same frame.
    fn cull(&mut self, ctx: &mut LayoutCtx<'_>) {
        let mut visible = 0;
        for child in &mut self.children {
            let bounds = Rect::from_origin_size(child.pos, child.size);
            let is_visible = bounds.overlaps(self.visible_rect);
            if is_visible != child.visible {
                child.visible = is_visible;
                ctx.set_stashed(&mut child.pod, !is_visible);
            }
            if is_visible {
                visible += 1;
            }
        }
        self.visible_count.set(visible);
    }
}

impl Widget for CanvasContent {
    type Action = NoAction;

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        // Never measures its children. Asking thousands of nodes how big they want
        // to be, on every layout pass, is precisely the cost this design avoids:
        // node sizes come from the graph model, so they are known before layout.
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::ZERO,
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {
        self.layouts.set(self.layouts.get() + 1);
        self.cull(ctx);

        // Children are laid out in canvas coordinates at their natural size; the
        // zoom lives entirely in this widget's transform. That separation is the
        // point. A per-region `ui_scale` would belong here, in layout — but `view`
        // must not, or every zoom step would relayout the whole graph.
        for child in &mut self.children {
            if !child.visible {
                continue;
            }
            if ctx.child_needs_layout(&child.pod) {
                self.child_layouts.set(self.child_layouts.get() + 1);
            }
            let child_size = ctx.compute_size(&mut child.pod, SizeDef::fixed(child.size), child.size.into());
            ctx.run_layout(&mut child.pod, child_size);
            ctx.place_child(&mut child.pod, child.pos);
        }
    }

    fn compose(&mut self, _ctx: &mut ComposeCtx<'_>) {
        self.composes.set(self.composes.get() + 1);
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        for child in &mut self.children {
            ctx.register_child(&mut child.pod);
        }
    }

    fn children_ids(&self) -> ChildrenIds {
        self.children.iter().map(|c| c.pod.id()).collect()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

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
    /// Extra margin around the viewport when culling, in canvas units.
    ///
    /// Culling exactly at the viewport edge makes nodes pop in mid-drag. A margin
    /// trades a little wasted work for stability.
    cull_margin: f64,
    /// Mirror of the content's counters, refreshed at the end of each layout.
    stats: Cell<CanvasStats>,
    /// Current pointer gesture.
    drag: Drag,
    /// Detail level already pushed to the children.
    applied_detail: Option<Detail>,
}

impl CanvasLayer {
    /// Creates a canvas holding the given items.
    pub fn new(items: impl IntoIterator<Item = CanvasItem>) -> Self {
        let children = items
            .into_iter()
            .map(|item| Child {
                pod: item.widget.to_pod(),
                pos: item.pos,
                size: item.size,
                visible: true,
            })
            .collect();
        Self {
            content: WidgetPod::new(CanvasContent::new(children)),
            view: Affine::IDENTITY,
            view_dirty: true,
            viewport: Size::ZERO,
            cull_margin: 128.0,
            stats: Cell::new(CanvasStats {
                zoom: 1.0,
                ..CanvasStats::default()
            }),
            drag: Drag::None,
            applied_detail: None,
        }
    }

    /// The current view transform.
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

    /// Converts a point from viewport coordinates to canvas coordinates.
    pub fn to_canvas(&self, p: Point) -> Point {
        self.view.inverse() * p
    }

    /// The region of canvas space currently visible, plus the cull margin.
    fn visible_canvas_rect(&self) -> Rect {
        let viewport = Rect::from_origin_size(Point::ORIGIN, self.viewport);
        self.view
            .inverse()
            .transform_rect_bbox(viewport)
            .inflate(self.cull_margin, self.cull_margin)
    }

    // --- MARK: WIDGETMUT

    /// Sets the view transform.
    ///
    /// This is the operation Phase 0 measures. It requests a layout pass, because
    /// culling depends on the view and culling happens in layout. That pass is not
    /// free, but it must be *shallow*: `run_layout_on` early-returns for every clean
    /// child whose size is unchanged, so [`CanvasStats::child_layouts`] should stay
    /// flat while panning.
    pub fn set_view(this: &mut WidgetMut<'_, Self>, view: Affine) {
        if this.widget.view == view {
            return;
        }
        this.widget.view = view;
        this.widget.view_dirty = true;
        this.ctx.request_layout();
    }

    /// Pans the view by a delta in viewport coordinates.
    pub fn pan(this: &mut WidgetMut<'_, Self>, delta: Vec2) {
        let view = Affine::translate(delta) * this.widget.view;
        Self::set_view(this, view);
    }

    /// Zooms around a fixed point given in viewport coordinates.
    ///
    /// The canvas point under `origin` stays under `origin`, which is what makes
    /// wheel-zoom feel anchored to the cursor.
    pub fn zoom_around(this: &mut WidgetMut<'_, Self>, origin: Point, factor: f64) {
        let current = this.widget.zoom();
        let clamped = (current * factor).clamp(0.02, 8.0);
        let factor = clamped / current;
        if (factor - 1.0).abs() < 1e-9 {
            return;
        }
        let view = Affine::translate(origin.to_vec2())
            * Affine::scale(factor)
            * Affine::translate(-origin.to_vec2())
            * this.widget.view;
        Self::set_view(this, view);
    }

    /// Moves a child to a new canvas-space position.
    ///
    /// Only the moved child is affected: Masonry re-places it in the next layout,
    /// and no other child's scene is re-encoded.
    pub fn move_child(this: &mut WidgetMut<'_, Self>, index: usize, pos: Point) {
        let mut content = this.ctx.get_mut(&mut this.widget.content);
        if let Some(child) = content.widget.children.get_mut(index) {
            child.pos = pos;
            content.ctx.request_layout();
        }
    }

    /// Returns the index of the topmost child whose bounds contain a canvas point.
    ///
    /// A linear scan, like [`CanvasContent::cull`], and for the same reason.
    pub fn child_at(this: &mut WidgetMut<'_, Self>, canvas_pos: Point) -> Option<usize> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content
            .widget
            .children
            .iter()
            .enumerate()
            .rev()
            .find(|(_, c)| Rect::from_origin_size(c.pos, c.size).contains(canvas_pos))
            .map(|(i, _)| i)
    }

    /// The canvas-space position of a child.
    pub fn child_pos(this: &mut WidgetMut<'_, Self>, index: usize) -> Option<Point> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.children.get(index).map(|c| c.pos)
    }

    // --- MARK: INTERNAL

    /// Applies a new view transform from an event handler.
    ///
    /// The `WidgetMut` variants above cannot be used here: an event handler holds
    /// `&mut self` plus an `EventCtx`, not a `WidgetMut`. The behaviour is the same.
    fn apply_view(&mut self, view: Affine, ctx: &mut EventCtx<'_>) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.view_dirty = true;
        ctx.request_layout();
    }

    /// Moves a child from an event handler.
    fn move_child_at(&mut self, index: usize, pos: Point, ctx: &mut EventCtx<'_>) {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        if let Some(child) = content.children.get_mut(index) {
            if child.pos == pos {
                return;
            }
            child.pos = pos;
            // Only the content widget is dirtied. Sibling nodes keep their cached
            // scenes; the moved node keeps its own too, since only its position
            // changed, not its contents.
            raw.request_layout();
        }
    }

    /// Finds the topmost child under a canvas-space point, from an event handler.
    fn hit_child(&mut self, canvas_pos: Point, ctx: &mut EventCtx<'_>) -> Option<(usize, Point)> {
        let (content, _) = ctx.get_raw(&mut self.content);
        content
            .children
            .iter()
            .enumerate()
            .rev()
            .find(|(_, c)| c.visible && Rect::from_origin_size(c.pos, c.size).contains(canvas_pos))
            .map(|(i, c)| (i, c.pos))
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
                let pos = ctx.local_position(e.state.position);
                let canvas_pos = self.view.inverse() * pos;
                self.drag = match e.button {
                    // Left button drags a node if there is one under the pointer,
                    // and pans otherwise.
                    Some(PointerButton::Primary) => match self.hit_child(canvas_pos, ctx) {
                        Some((index, child_pos)) => Drag::Node {
                            index,
                            grab: canvas_pos - child_pos,
                        },
                        None => Drag::Pan { last: pos },
                    },
                    // Middle button always pans, as in Blender.
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
                    Drag::None => {},
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
                    x: 120.0 * scale_factor,
                    y: 120.0 * scale_factor,
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
                let factor = (-y * 0.0015).exp();
                let current = self.zoom();
                let clamped = (current * factor).clamp(0.02, 8.0);
                let factor = clamped / current;
                if (factor - 1.0).abs() < 1e-9 {
                    return;
                }
                // Zoom about the cursor: the canvas point under the pointer stays
                // under the pointer.
                let view = Affine::translate(origin.to_vec2())
                    * Affine::scale(factor)
                    * Affine::translate(-origin.to_vec2())
                    * self.view;
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

        // Clip to the viewport so children panned out of view cannot paint over the
        // surrounding UI, and so Masonry excludes them from hit testing.
        ctx.set_clip_path(Rect::from_origin_size(Point::ORIGIN, size));

        let visible_rect = self.visible_canvas_rect();
        let detail = Detail::for_scale(self.zoom());
        let view = self.view;
        let view_dirty = std::mem::take(&mut self.view_dirty);

        // Push the view down to the content widget. `set_transform` marks it as
        // needing compose, which runs after layout — so this does not violate the
        // "don't set flags for an earlier pass" rule that `get_raw_mut` warns about.
        {
            let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
            content.visible_rect = visible_rect;
            content.detail = Some(detail);
            if view_dirty {
                raw.set_transform(view);
            }
        }

        // Push the detail level down when, and only when, it changes. `applied_detail`
        // is updated at queue time rather than when the callback runs, so a threshold
        // crossing queues exactly one batch even though the rewrite loop may run
        // layout several times before the mutate pass fires.
        //
        // The mutate pass runs before the layout pass inside that loop, so a crossing
        // still takes effect in the same frame.
        if self.applied_detail != Some(detail) {
            self.applied_detail = Some(detail);
            ctx.mutate_child_later(&mut self.content, move |mut content| {
                CanvasContent::set_detail(&mut content, detail);
            });
        }

        let content_size = ctx.compute_size(&mut self.content, SizeDef::fixed(size), size.into());
        ctx.run_layout(&mut self.content, content_size);
        ctx.place_child(&mut self.content, Point::ORIGIN);

        let zoom = self.zoom();
        let (content, _) = ctx.get_raw(&mut self.content);
        self.stats.set(CanvasStats {
            total: content.children.len(),
            visible: content.visible_count.get(),
            content_layouts: content.layouts.get(),
            child_layouts: content.child_layouts.get(),
            composes: content.composes.get(),
            detail: content.detail,
            zoom,
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
