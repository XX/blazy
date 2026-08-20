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
        if scale > 0.3 {
            Self::Full
        } else if scale > 0.1 {
            Self::Simplified
        } else {
            Self::Box
        }
    }
}

/// The detail level the canvas as a whole is showing.
///
/// Note this is the *global* level, not the level the individual node was built at:
/// a node under the pointer keeps its controls while everything around it is
/// simplified. Use it to decide how much effort a painted stand-in deserves — at
/// [`Detail::Simplified`] there are hundreds of nodes on screen and each draw command
/// is multiplied by that count, while at [`Detail::Full`] there are few and the
/// stand-in has to resemble the real controls closely enough that swapping them in on
/// hover is not jarring.
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
    /// Nodes that currently have a widget in the tree.
    ///
    /// With virtualisation this is also the number of widgets the passes walk, which
    /// is the point: it is bounded by the viewport, not by the graph.
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
    /// Times the far-field scene has been re-recorded since the canvas was created.
    ///
    /// The scene is in canvas coordinates, so panning and zooming inside the painted
    /// region reuse it untouched. If this climbs while panning, the region is too
    /// tight and the recording is being thrown away every frame.
    pub far_repaints: u64,
    /// Widgets built since the canvas was created.
    ///
    /// Each one is a node scrolling into view. If this climbs steeply while panning
    /// slowly, the cull margin is too tight and nodes are thrashing in and out.
    pub builds: u64,
    /// Detail level applied at the last layout.
    pub detail: Option<Detail>,
    /// Current zoom factor.
    pub zoom: f64,
}

/// Builds the widget for a node when it scrolls into view.
///
/// The canvas materialises widgets lazily, so a node's *state* cannot live in its
/// widget: the widget does not exist most of the time. The model behind this trait
/// is the source of truth, and the widget is a view over it — which is the normal
/// arrangement for a node editor anyway, since the graph outlives any view of it.
///
/// Implemented for any `FnMut(usize) -> NewWidget<dyn Widget>`.
pub trait NodeSource: 'static {
    /// Builds the widget for the node at `index`, at the given detail level.
    ///
    /// Called every time the node enters the materialised region, so it must read
    /// current state from the model rather than assuming defaults.
    ///
    /// `detail` is [`Detail::Full`] or [`Detail::Simplified`]; below that the canvas
    /// paints the node itself and never calls this. Implementations should build
    /// *fewer child widgets* at `Simplified`, not merely stash them: a stashed widget
    /// still costs a visit in every pass. A control a few pixels tall cannot be used,
    /// so it should be drawn rather than built.
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget>;

    /// Draws node `index` when it is too small to deserve a widget.
    ///
    /// Below the [`Detail::Box`] threshold the canvas stops materialising widgets
    /// entirely and paints the nodes itself, in one pass, into its own scene. A node
    /// a few pixels across does not need layout, hit testing, accessibility or an
    /// event route — it needs a filled rectangle, and a rectangle costs nanoseconds
    /// where a widget costs microseconds.
    ///
    /// `rect` is in canvas coordinates. The default draws nothing.
    fn paint_far(&mut self, index: usize, rect: Rect, painter: &mut Painter<'_>) {
        let _ = (index, rect, painter);
    }
}

impl<F> NodeSource for F
where
    F: FnMut(usize, Detail) -> NewWidget<dyn Widget> + 'static,
{
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget> {
        self(index, detail)
    }
}

/// One node's slot: geometry always, a widget only while it is in view.
struct Slot {
    /// Position of the node's top-left corner, in canvas coordinates.
    pos: Point,
    /// Size in canvas coordinates. Fixed, so children are never measured.
    size: Size,
    /// The materialised widget, if this node currently has one.
    pod: Option<WidgetPod<dyn Widget>>,
    /// How `pod` was built: its own detail level, and the canvas-wide one.
    ///
    /// Both matter. The first decides whether the node has controls; the second is
    /// handed to it as [`CanvasDetail`] so it can scale how much effort its painted
    /// stand-in deserves. A change in either makes the widget stale.
    built: Option<(Detail, Detail)>,
}

/// Whether `outer` fully contains `inner`.
fn contains_rect(outer: Rect, inner: Rect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

// --- MARK: CONTENT

/// The transformed inner half of a [`CanvasLayer`].
///
/// Holds the freely positioned children and carries the pan/zoom transform. Not
/// constructed directly; a [`CanvasLayer`] owns one.
pub struct CanvasContent {
    /// Geometry for every node; widgets for the materialised ones only.
    slots: Vec<Slot>,
    /// Builds widgets on demand.
    source: Box<dyn NodeSource>,
    /// Indices with a live widget, ascending.
    ///
    /// Empty in far-field mode: below the [`Detail::Box`] threshold no node gets a
    /// widget at all.
    live: Vec<usize>,
    /// Indices inside the visible rect, ascending.
    ///
    /// Equal to `live` above the far-field threshold. Below it, this is what gets
    /// painted directly.
    visible: Vec<usize>,
    /// Whether the canvas is painting nodes itself instead of materialising them.
    far_field: bool,
    /// The canvas-space region the far-field scene was painted for, if it is valid.
    ///
    /// The scene is recorded in canvas coordinates, so it stays correct under any
    /// pan or zoom — that is the whole point of a vector display list. It only has to
    /// be re-recorded when the viewport leaves this region, which with a generous
    /// margin means "almost never" rather than "every frame".
    far_region: Option<Rect>,
    /// The nodes in `far_region`, painted by `paint`.
    far_nodes: Vec<usize>,
    /// Set when the far-field scene must be re-recorded.
    far_dirty: bool,
    /// The node the pointer is on, if any.
    ///
    /// Only this node gets interactive controls; see [`CanvasContent::effective_detail`].
    active: Option<usize>,
    /// Set when `detail` or `active` changed, so stale widgets get rebuilt.
    detail_dirty: bool,
    /// Whether the queued `pending` needs a staleness sweep as well as a set diff.
    pending_stale: bool,
    /// Indices that should have a widget, computed by the last cull and applied in
    /// the next mutate pass.
    pending: Option<Vec<usize>>,
    /// Visible region in canvas coordinates, pushed down by the parent.
    visible_rect: Rect,
    /// Detail level pushed down by the parent.
    detail: Option<Detail>,
    layouts: Cell<u64>,
    child_layouts: Cell<u64>,
    composes: Cell<u64>,
    builds: Cell<u64>,
    far_repaints: Cell<u64>,
}

// `CanvasLayer` owns this widget completely and reaches into it during layout to
// push down the view transform, the visible rect and the detail level. This is the
// case the escape hatch is documented for: "a parent widget completely controls
// their child, but needs it to be a separate widget for user interaction to behave
// as expected".
impl AllowRawMut for CanvasContent {}

impl CanvasContent {
    fn new(slots: Vec<Slot>, source: Box<dyn NodeSource>) -> Self {
        Self {
            slots,
            source,
            live: Vec::new(),
            visible: Vec::new(),
            far_field: false,
            far_region: None,
            far_nodes: Vec::new(),
            far_dirty: false,
            active: None,
            detail_dirty: false,
            pending_stale: false,
            pending: None,
            visible_rect: Rect::ZERO,
            detail: None,
            layouts: Cell::new(0),
            child_layouts: Cell::new(0),
            composes: Cell::new(0),
            builds: Cell::new(0),
            far_repaints: Cell::new(0),
        }
    }

    /// The detail level node `index` should be built at.
    ///
    /// Only the node under the pointer gets real controls. Every other node on screen
    /// gets the painted stand-in, because a control nobody is touching is three
    /// widgets that every pass has to walk for nothing. At any moment a user is
    /// interacting with one node, so materialising one node's controls is enough.
    ///
    /// The cost of this trick is a visual swap when the pointer arrives, so the
    /// painted stand-in has to resemble the real controls closely.
    fn effective_detail(&self, index: usize) -> Detail {
        match self.detail.unwrap_or(Detail::Full) {
            Detail::Box => Detail::Box,
            _ if self.active == Some(index) => Detail::Full,
            _ => Detail::Simplified,
        }
    }

    /// How node `index` should be built right now.
    fn build_spec(&self, index: usize) -> (Detail, Detail) {
        (self.effective_detail(index), self.detail.unwrap_or(Detail::Full))
    }

    /// Materialises and dematerialises children to match the last cull.
    ///
    /// Runs in the mutate pass, which is where adding and removing children is
    /// legal. The mutate pass runs before the layout pass inside the rewrite loop,
    /// so a node entering the view is built, laid out and painted in the same frame.
    fn apply_pending(this: &mut WidgetMut<'_, Self>) {
        let Some(desired) = this.widget.pending.take() else {
            return;
        };
        let stale = std::mem::take(&mut this.widget.pending_stale);

        // Both lists are ascending, so a merge walk finds the difference in one pass
        // over the live set. Doing this by lookup instead costs a search per visible
        // node on every frame of a pan, which is measurably worse.
        let mut removed = Vec::new();
        let mut added = Vec::new();
        let (mut a, mut b) = (0, 0);
        while a < this.widget.live.len() || b < desired.len() {
            match (this.widget.live.get(a), desired.get(b)) {
                (Some(&l), Some(&d)) if l == d => {
                    // Kept. Only worth checking for a stale detail level when one
                    // actually changed, which is a threshold crossing or a new node
                    // under the pointer — never during a plain pan.
                    if stale && this.widget.slots[l].built != Some(this.widget.build_spec(l)) {
                        removed.push(l);
                        added.push(l);
                    }
                    a += 1;
                    b += 1;
                },
                (Some(&l), Some(&d)) if l < d => {
                    removed.push(l);
                    a += 1;
                },
                (Some(_), Some(&d)) => {
                    added.push(d);
                    b += 1;
                },
                (Some(&l), None) => {
                    removed.push(l);
                    a += 1;
                },
                (None, Some(&d)) => {
                    added.push(d);
                    b += 1;
                },
                (None, None) => break,
            }
        }

        // Removing takes the widget out of the tree entirely. This is the whole
        // point: a stashed widget is still walked by every pass, a removed one is
        // not. Its state is not lost — it lives in the model behind `NodeSource`.
        let changed = !removed.is_empty() || !added.is_empty();
        for index in removed {
            if let Some(pod) = this.widget.slots[index].pod.take() {
                this.widget.slots[index].built = None;
                this.ctx.remove_child(pod);
            }
        }

        for index in added {
            let (detail, global) = this.widget.build_spec(index);
            let widget = this.widget.source.build(index, detail).with_props(CanvasDetail(global));
            this.widget.slots[index].pod = Some(widget.to_pod());
            this.widget.slots[index].built = Some((detail, global));
            this.widget.builds.set(this.widget.builds.get() + 1);
        }

        this.widget.live = desired;
        if changed {
            this.ctx.children_changed();
            this.ctx.request_layout();
        }
        // The far-field repaint is requested by the parent, which is where culling
        // and the region check happen.
    }

    /// Computes the set of nodes inside the visible rect.
    ///
    /// A linear scan over the geometry array. That is O(total) per frame, but it
    /// touches 32 bytes per node and no widget state, so it costs a few microseconds
    /// where materialising the same nodes costs milliseconds. A spatial index drops
    /// in here when the scan itself starts to matter.
    fn cull(&mut self) {
        let mut visible = Vec::with_capacity(self.visible.len() + 8);
        for (index, slot) in self.slots.iter().enumerate() {
            if Rect::from_origin_size(slot.pos, slot.size).overlaps(self.visible_rect) {
                visible.push(index);
            }
        }

        // Below the box threshold the canvas paints nodes itself, so nothing is
        // materialised. This is what keeps a fully zoomed-out graph affordable: the
        // visible set stops bounding the cost, so the cost must stop depending on
        // widgets. See `paint`.
        let far_field = self.detail.unwrap_or(Detail::Full) == Detail::Box;
        let desired: Vec<usize> = if far_field { Vec::new() } else { visible.clone() };

        self.visible = visible;

        // A widget built at the wrong detail level has to be replaced, not repainted:
        // the levels differ in which child widgets exist, not only in how they look.
        let stale = std::mem::take(&mut self.detail_dirty)
            && self
                .live
                .iter()
                .any(|&i| self.slots[i].built != Some(self.build_spec(i)));

        if far_field {
            self.refresh_far_region();
        } else if self.far_region.take().is_some() {
            self.far_nodes.clear();
        }

        if desired != self.live || far_field != self.far_field || stale || self.far_dirty {
            self.far_field = far_field;
            self.pending_stale = stale;
            self.pending = Some(desired);
        } else {
            self.pending = None;
        }
    }

    /// Re-records the far-field node set when the viewport leaves the painted region.
    ///
    /// The recorded scene lives in canvas coordinates, so panning and zooming inside
    /// the region cost one `Affine` and nothing else. The margin is what turns
    /// "re-record every frame" into "re-record when you have travelled half a
    /// screen": it is bought with a larger scene, which the paint pass appends every
    /// frame either way, so it should be generous but not unbounded.
    fn refresh_far_region(&mut self) {
        const OVERSCAN: f64 = 0.5;

        if self.far_region.is_some_and(|r| contains_rect(r, self.visible_rect)) {
            return;
        }

        let region = self.visible_rect.inflate(
            self.visible_rect.width() * OVERSCAN,
            self.visible_rect.height() * OVERSCAN,
        );

        self.far_nodes.clear();
        for (index, slot) in self.slots.iter().enumerate() {
            if Rect::from_origin_size(slot.pos, slot.size).overlaps(region) {
                self.far_nodes.push(index);
            }
        }
        self.far_region = Some(region);
        self.far_dirty = true;
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
        // Never measures its children. Node sizes come from the model, so they are
        // known before layout starts.
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::ZERO,
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {
        self.layouts.set(self.layouts.get() + 1);

        // Children are laid out in canvas coordinates at their natural size; the
        // zoom lives entirely in this widget's transform. That separation is the
        // point. A per-region `ui_scale` would belong here, in layout — but `view`
        // must not, or every zoom step would relayout the whole graph.
        for i in 0..self.live.len() {
            let index = self.live[i];
            let size = self.slots[index].size;
            let pos = self.slots[index].pos;
            let Some(pod) = self.slots[index].pod.as_mut() else {
                continue;
            };
            if ctx.child_needs_layout(pod) {
                self.child_layouts.set(self.child_layouts.get() + 1);
            }
            // The size is known from the model, so there is nothing to resolve.
            ctx.run_layout(pod, size);
            ctx.place_child(pod, pos);
        }
    }

    fn compose(&mut self, _ctx: &mut ComposeCtx<'_>) {
        self.composes.set(self.composes.get() + 1);
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        if !self.far_field {
            return;
        }
        self.far_repaints.set(self.far_repaints.get() + 1);
        // One pass over the visible nodes, straight into this widget's cached scene.
        // The scene is in canvas coordinates, so panning and zooming re-use it via
        // the layer transform without re-encoding anything.
        for &index in &self.far_nodes {
            let slot = &self.slots[index];
            let rect = Rect::from_origin_size(slot.pos, slot.size);
            self.source.paint_far(index, rect, painter);
        }
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        for &index in &self.live {
            if let Some(pod) = self.slots[index].pod.as_mut() {
                ctx.register_child(pod);
            }
        }
    }

    fn children_ids(&self) -> ChildrenIds {
        self.live
            .iter()
            .filter_map(|&i| self.slots[i].pod.as_ref())
            .map(|pod| pod.id())
            .collect()
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
}

impl CanvasLayer {
    /// Creates a canvas over `count` nodes.
    ///
    /// `geometry` supplies each node's position and size, and `source` builds its
    /// widget when it scrolls into view. Only the geometry is stored up front: a
    /// graph of a million nodes costs a million `(Point, Size)` pairs, not a million
    /// widgets.
    pub fn new(count: usize, mut geometry: impl FnMut(usize) -> (Point, Size), source: impl NodeSource) -> Self {
        let slots = (0..count)
            .map(|i| {
                let (pos, size) = geometry(i);
                Slot {
                    pos,
                    size,
                    pod: None,
                    built: None,
                }
            })
            .collect();
        Self {
            content: WidgetPod::new(CanvasContent::new(slots, Box::new(source))),
            view: Affine::IDENTITY,
            view_dirty: true,
            viewport: Size::ZERO,
            cull_margin: 128.0,
            stats: Cell::new(CanvasStats {
                zoom: 1.0,
                ..CanvasStats::default()
            }),
            drag: Drag::None,
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
        if let Some(slot) = content.widget.slots.get_mut(index) {
            if slot.pos == pos {
                return;
            }
            slot.pos = pos;
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
            .slots
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| Rect::from_origin_size(s.pos, s.size).contains(canvas_pos))
            .map(|(i, _)| i)
    }

    /// The nodes that currently have a widget, as `(index, widget id)` pairs.
    ///
    /// Useful for tests and for apps that need to reach into a live node. The list
    /// changes as nodes scroll in and out, so ids must not be cached across frames.
    pub fn live_children(this: &mut WidgetMut<'_, Self>) -> Vec<(usize, masonry::core::WidgetId)> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content
            .widget
            .live
            .iter()
            .filter_map(|&i| content.widget.slots[i].pod.as_ref().map(|pod| (i, pod.id())))
            .collect()
    }

    /// The canvas-space position of a child.
    pub fn child_pos(this: &mut WidgetMut<'_, Self>, index: usize) -> Option<Point> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.slots.get(index).map(|s| s.pos)
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
        let far_field = content.far_field;
        if let Some(slot) = content.slots.get_mut(index) {
            if slot.pos == pos {
                return;
            }
            slot.pos = pos;
            // Only the content widget is dirtied. Sibling nodes keep their cached
            // scenes; the moved node keeps its own too, since only its position
            // changed, not its contents.
            raw.request_layout();
            if far_field {
                // In far-field mode the node is part of this widget's own scene, so
                // moving it means re-recording that scene.
                content.far_region = None;
                raw.request_paint_only();
            }
        }
    }

    /// Marks the node under the pointer, so only it gets interactive controls.
    ///
    /// Returns `true` if the active node changed.
    fn set_active(&mut self, active: Option<usize>, ctx: &mut EventCtx<'_>) -> bool {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        if content.active == active || content.far_field {
            return false;
        }
        content.active = active;
        content.detail_dirty = true;
        raw.request_layout();
        true
    }

    /// Finds the topmost child under a canvas-space point, from an event handler.
    fn hit_child(&mut self, canvas_pos: Point, ctx: &mut EventCtx<'_>) -> Option<(usize, Point)> {
        let (content, _) = ctx.get_raw(&mut self.content);
        // Hit testing uses the visible set, not the materialised one, so nodes stay
        // grabbable in far-field mode where they have no widget at all.
        content
            .visible
            .iter()
            .rev()
            .map(|&i| (i, &content.slots[i]))
            .find(|(_, s)| Rect::from_origin_size(s.pos, s.size).contains(canvas_pos))
            .map(|(i, s)| (i, s.pos))
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
                    Drag::None => {
                        // Materialise controls for the node under the pointer, and
                        // only that one. Everything else keeps its painted stand-in.
                        let canvas_pos = self.view.inverse() * pos;
                        let hit = self.hit_child(canvas_pos, ctx).map(|(index, _)| index);
                        self.set_active(hit, ctx);
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
                self.set_active(None, ctx);
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
            content.visible_rect = visible_rect;
            if content.detail != Some(detail) {
                content.detail = Some(detail);
                content.detail_dirty = true;
            }
            if view_dirty {
                raw.set_transform(view);
            }

            content.cull();
            if std::mem::take(&mut content.far_dirty) {
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

        let zoom = self.zoom();
        let (content, _) = ctx.get_raw(&mut self.content);
        self.stats.set(CanvasStats {
            total: content.slots.len(),
            visible: content.live.len(),
            content_layouts: content.layouts.get(),
            child_layouts: content.child_layouts.get(),
            composes: content.composes.get(),
            builds: content.builds.get(),
            far_repaints: content.far_repaints.get(),
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
