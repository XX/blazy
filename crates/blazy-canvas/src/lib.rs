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
//! # What is missing
//!
//! Not a finished node editor yet. Virtualisation, level of detail, the link layer
//! and the spatial index are in and measured (§20, §24), and so is picking by shape
//! rather than by rectangle (§25) — but there is no selection model and no
//! serialisation of the graph. Those are domain work on top of a canvas whose shape
//! is no longer in question.

mod index;
mod links;

use std::any::TypeId;
use std::cell::Cell;

use blazy_shape::{near_segment, scale_of};
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AllowRawMut, ChildrenIds, ComposeCtx, EventCtx, LayoutCtx, MeasureCtx, MutateCtx, NewWidget, NoAction,
    PaintCtx, PointerEvent, PropertiesMut, PropertiesRef, Property, RawCtx, RegisterCtx, UpdateCtx, Widget, WidgetId,
    WidgetMut, WidgetPod,
};
use masonry::dpi::{LogicalPosition, PhysicalPosition};
use masonry::imaging::Painter;
use masonry::kurbo::{Affine, Axis, Point, Rect, Size, Stroke, Vec2};
use masonry::layout::{AsUnit, LenReq, Length, SizeDef};
use masonry::peniko::Color;
use masonry::ui_events::pointer::{PointerButton, PointerScrollEvent, PointerUpdate};
use strum::IntoStaticStr;

use crate::index::SpatialIndex;
pub use crate::links::Link;
use crate::links::{LinkLayer, link_curve, link_path};

/// How much detail a canvas child should draw at the current zoom level.
///
/// Level of detail serves two purposes, and the second matters more. The obvious
/// one is fewer draw commands per node. The important one is that at
/// [`Detail::Box`] a node can stash its contents entirely — and layout, not
/// painting, is what makes a large graph expensive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Detail {
    /// Full contents: header, body and interactive controls.
    Full,
    /// Header only; controls are stashed.
    Simplified,
    /// A flat filled rectangle. Contents are stashed and not laid out.
    Box,
}

impl Detail {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// The zoom levels at which a canvas switches between [`Detail`] levels.
///
/// Policy, not mechanism: how small a node has to get before its controls stop being
/// usable depends on how the application draws it. It lives here, on the canvas,
/// rather than baked into the library — the alternative is editing this crate to
/// retune a demo, which is a smell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetailThresholds {
    /// Above this zoom, nodes are drawn in full and carry interactive controls.
    pub full: f64,
    /// Above this zoom (and below `full`), nodes keep widgets but drop their controls.
    /// Below it the canvas paints nodes itself and materialises nothing.
    pub simplified: f64,
}

impl Default for DetailThresholds {
    fn default() -> Self {
        Self {
            full: 0.2,
            simplified: 0.05,
        }
    }
}

impl DetailThresholds {
    /// Chooses a detail level for an effective scale factor.
    pub fn for_scale(&self, scale: f64) -> Detail {
        if scale > self.full {
            Detail::Full
        } else if scale > self.simplified {
            Detail::Simplified
        } else {
            Detail::Box
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

/// How the canvas strokes its links.
///
/// Style rather than mechanism, like [`DetailThresholds`]: how a link should look
/// depends on the application, and baking it into the crate would mean editing this
/// file to retune a demo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkStyle {
    pub color: Color,
    /// Colour of the link under the pointer.
    pub hover_color: Color,
    /// Stroke width in canvas units, so links thicken with the zoom like everything
    /// else the canvas draws.
    pub width: f64,
    /// How far the pointer may miss a link and still pick it, in **screen pixels**.
    ///
    /// Screen pixels rather than canvas units, because the tolerance is about the
    /// pointer and not about the drawing: four canvas units are 0.08 px at the bottom
    /// of the zoom range and 32 px at the top, which would make a link unpickable
    /// exactly where it is thinnest (`rnd/architecture.md` §25.2).
    pub slop: f64,
}

impl Default for LinkStyle {
    fn default() -> Self {
        Self {
            color: Color::from_rgb8(0x8a, 0x8a, 0x96),
            hover_color: Color::from_rgb8(0xd8, 0xd8, 0xe4),
            width: 2.0,
            slop: blazy_shape::DEFAULT_SLOP,
        }
    }
}

/// What the canvas found under a point.
///
/// Nodes win over links, at every detail level, because that is the order they are
/// painted in (§25.3): a pointer that disagrees with the picture is worse than one
/// that is imprecise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CanvasHit {
    /// A node, with the canvas-space position of its top-left corner.
    Node { index: usize, pos: Point },
    /// A link, by its index in the edge list.
    Link { edge: usize, link: Link },
}

impl CanvasHit {
    /// The node index, if this is a node.
    pub fn node(self) -> Option<usize> {
        match self {
            Self::Node { index, .. } => Some(index),
            Self::Link { .. } => None,
        }
    }

    /// The edge index, if this is a link.
    pub fn link(self) -> Option<usize> {
        match self {
            Self::Link { edge, .. } => Some(edge),
            Self::Node { .. } => None,
        }
    }
}

/// What the canvas is doing right now, plus counters for the Phase 0 measurements.
///
/// Deliberately cheap to collect: Phase 0 exists to produce numbers, and numbers
/// nobody can see are not evidence.
///
/// Captured during the canvas's layout. That matters for [`CanvasCounters::far_repaints`],
/// which is bumped during *paint* and is therefore always one frame behind the rest.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CanvasStats {
    /// Total number of nodes.
    pub total: usize,
    /// Nodes that currently have a widget in the tree.
    ///
    /// This is the number the passes walk, which is the point: it is bounded by the
    /// viewport, not by the graph. Zero in far-field mode, where nodes are painted
    /// rather than materialised — they are on screen but they are not widgets.
    pub materialised: usize,
    /// Detail level applied at the last layout.
    pub detail: Option<Detail>,
    /// Current zoom factor.
    pub zoom: f64,
    /// What the pointer was last found to be over, as of the last pointer move.
    ///
    /// One frame behind whatever moved the pointer, because it is recorded during
    /// event handling and read here during layout — which is exactly the point: a
    /// pick must not ask for a layout pass of its own (§25.4).
    pub hovered: Option<CanvasHit>,
    /// Cumulative work counters.
    pub counters: CanvasCounters,
}

/// Cumulative counters, for spotting work that should not be happening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CanvasCounters {
    /// Layout passes run on the content widget.
    pub content_layouts: u64,
    /// Nodes that actually needed layout, summed over all passes.
    ///
    /// `run_layout_on` early-returns for a clean widget of unchanged size, so panning
    /// should leave this flat. If it climbs with every pan, something is dirtying
    /// children that should not be.
    pub child_layouts: u64,
    /// Compose passes run on the content widget.
    pub composes: u64,
    /// Widgets built, one per node entering the materialised region.
    ///
    /// If this climbs steeply while panning slowly, the overscan is too tight and
    /// nodes are thrashing in and out.
    pub builds: u64,
    /// Node geometries examined while deciding what is on screen, summed over all
    /// passes.
    ///
    /// The number the spatial index exists to bound. A count rather than a duration
    /// on purpose: it is exact and machine-independent, where the microseconds it
    /// replaced are the kind of measurement that disappears into noise until the
    /// graph is large enough for the problem to be urgent.
    pub slot_visits: u64,
    /// Times the link curves have been re-emitted into the scene.
    ///
    /// Rises whenever the content widget repaints for any reason — a node entering
    /// view is enough — so it measures work done rather than a decision made.
    /// Informational; the decision is [`link_reselects`](Self::link_reselects).
    pub link_repaints: u64,
    /// Times the canvas has re-chosen *which* links are recorded.
    ///
    /// This is the one to bound. Panning inside the recorded region must not raise
    /// it, because the curves are in canvas coordinates and the layer transform does
    /// the work; neither must dragging a node, which moves its own curves without
    /// changing which curves are on screen. Only leaving the region should.
    pub link_reselects: u64,
    /// Times the far-field scene has been re-recorded.
    ///
    /// The scene is in canvas coordinates, so panning and zooming inside the painted
    /// region reuse it untouched. If this climbs while panning, the region is too
    /// tight and the recording is being thrown away every frame.
    pub far_repaints: u64,
    /// Picks performed: pointer moves, button presses, explicit hit tests.
    ///
    /// The denominator of the two counters below. On its own it says only how often
    /// the question was asked.
    pub hit_queries: u64,
    /// Node geometries examined while answering picks, summed.
    ///
    /// Bounded by the grid cell the point falls in plus its slack, so it is bounded
    /// by node *density* and not by the size of the graph — which is the difference
    /// between a pointer that stays cheap on a million nodes and one that does not.
    pub hit_node_tests: u64,
    /// Link curves examined while answering picks, summed.
    ///
    /// Candidates are the links the canvas has actually recorded, so what can be
    /// clicked is exactly what can be seen and the count is bounded by the viewport
    /// region rather than by the edge list.
    pub hit_curve_tests: u64,
}

/// Builds the widget for a node when it scrolls into view.
///
/// The canvas materialises widgets lazily, so a node's *state* cannot live in its
/// widget: the widget does not exist most of the time. The model behind this trait
/// is the source of truth, and the widget is a view over it — which is the normal
/// arrangement for a node editor anyway, since the graph outlives any view of it.
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
    /// Every command drawn here is multiplied by the number of nodes in the recorded
    /// region, so keep it to a few cheap shapes.
    ///
    /// `rect` is in canvas coordinates. The default draws nothing.
    fn paint_far(&mut self, index: usize, rect: Rect, painter: &mut Painter<'_>) {
        let _ = (index, rect, painter);
    }

    /// Whether the canvas-space `point` is inside node `index`, whose rectangle is
    /// `rect`.
    ///
    /// The canvas knows where a node is and how big it is; only the application knows
    /// what it looks like, and a node is not usually its bounding box — a rounded
    /// corner, a notch, a circular port. This is the seam: the canvas narrows the
    /// candidates down through its index and asks this about each survivor, so an
    /// implementation is called a handful of times per pick and can afford to be
    /// exact. `blazy_shape::ShapeHit` is the intended tool, kept by the implementor
    /// so that its flattened cache survives between picks.
    ///
    /// Called for nodes that have no widget as well — the far field, and anything
    /// off the materialised set — which is why it cannot be a method on the widget.
    ///
    /// The default is the rectangle, which is what the canvas would answer on its own.
    fn hit(&mut self, index: usize, rect: Rect, point: Point) -> bool {
        let _ = index;
        rect.contains(point)
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

/// The canvas's own recording of nodes too small to deserve widgets.
///
/// Recorded in canvas coordinates, so it stays correct under any pan or zoom — that
/// is the whole point of a vector display list. It only has to be re-recorded when
/// the viewport leaves `region`, which with a generous margin means "almost never"
/// rather than "every frame".
#[derive(Default)]
struct FarField {
    /// Whether the canvas is painting nodes itself instead of materialising them.
    active: bool,
    /// The canvas-space region `nodes` was recorded for. `None` means "needs redoing".
    region: Option<Rect>,
    /// The nodes inside `region`. Meaningless unless `region` is `Some`.
    nodes: Vec<usize>,
    /// Set when the recording must be redone; cleared by the parent once it has asked
    /// for a repaint.
    dirty: bool,
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

/// How far past the viewport nodes stay materialised, as a fraction of the viewport.
///
/// Small on purpose. A node entering the viewport is built in the mutate pass and laid
/// out in the same frame, so the margin buys smoothness under a fast drag rather than
/// correctness — and every extra node it keeps alive is paid for in every pass. At
/// 0.02 of a 1100 px viewport it is about four frames of slack at a typical drag
/// speed; larger values were measurably more expensive at low zoom, where a fraction
/// of the viewport covers a lot of canvas.
const DEFAULT_OVERSCAN: f64 = 0.02;
/// How far past the visible region the far-field scene is recorded.
///
/// Larger than [`DEFAULT_OVERSCAN`] because the trade is different: a bigger recorded
/// scene costs more to append every frame, but re-recording it is what a pan must
/// avoid entirely. Half a screen of margin turns "every frame" into "every few
/// hundred".
const FAR_OVERSCAN: f64 = 0.5;

/// Zoom changes smaller than this are treated as no change at all.
const ZOOM_EPSILON: f64 = 1e-9;
/// How fast a wheel notch zooms, as an exponent on the scroll distance in pixels.
const WHEEL_ZOOM_RATE: f64 = 0.0015;
/// A wheel notch, in pixels, matching what `Portal` assumes.
const WHEEL_LINE_PX: f64 = 120.0;

/// What a state change asks Masonry to redo.
///
/// Exists so the operations below can be written once against `&mut self` and then
/// applied through whichever context the caller happens to hold — a `MutateCtx` from
/// the public API, a `RawCtx` from the pointer handler. Without it each operation
/// needs two copies that drift apart; they already had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
enum Invalidate {
    Nothing,
    Layout,
    LayoutAndPaint,
}

/// The subset of a Masonry context this crate needs to invalidate a widget.
trait Invalidator {
    fn request_layout(&mut self);
    fn request_paint_only(&mut self);
}

impl Invalidator for MutateCtx<'_> {
    fn request_layout(&mut self) {
        Self::request_layout(self);
    }

    fn request_paint_only(&mut self) {
        Self::request_paint_only(self);
    }
}

impl Invalidator for RawCtx<'_> {
    fn request_layout(&mut self) {
        Self::request_layout(self);
    }

    fn request_paint_only(&mut self) {
        Self::request_paint_only(self);
    }
}

impl Invalidate {
    fn apply(self, ctx: &mut impl Invalidator) {
        match self {
            Self::Nothing => {},
            Self::Layout => ctx.request_layout(),
            Self::LayoutAndPaint => {
                ctx.request_layout();
                ctx.request_paint_only();
            },
        }
    }
}

/// Splits two ascending index lists into what left and what arrived.
///
/// `keep_stale` is asked about indices present in both: returning `true` puts the
/// index in *both* outputs, which is how a widget gets rebuilt in place at a new
/// detail level.
///
/// A merge walk rather than a lookup per element: during a pan this runs over the
/// whole visible set every frame, and a search per node was measurably worse.
fn diff_sorted(
    live: &[usize],
    desired: &[usize],
    mut keep_stale: impl FnMut(usize) -> bool,
    removed: &mut Vec<usize>,
    added: &mut Vec<usize>,
) {
    removed.clear();
    added.clear();
    let (mut a, mut b) = (0, 0);
    loop {
        match (live.get(a), desired.get(b)) {
            (Some(&l), Some(&d)) if l == d => {
                if keep_stale(l) {
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
    /// The far-field recording, used below the [`Detail::Box`] threshold.
    far: FarField,
    /// Edges between nodes, drawn as curves rather than built as widgets.
    links: LinkLayer,
    /// How links are stroked.
    link_style: LinkStyle,
    /// The node the pointer is on, if any. Only meaningful with `controls_on_hover`.
    active: Option<usize>,
    /// What the pointer is over, node or link. Repainted, never relaid out.
    hovered: Option<CanvasHit>,
    /// Whether only the node under the pointer gets interactive controls.
    controls_on_hover: bool,
    /// Set when `detail` or `active` changed, so stale widgets get rebuilt.
    detail_dirty: bool,
    /// Whether the queued `pending` needs a staleness sweep as well as a set diff.
    pending_stale: bool,
    /// Finds what is on screen without walking the graph.
    index: SpatialIndex,
    /// Reused buffers for the set difference, so a pan allocates nothing.
    scratch_removed: Vec<usize>,
    scratch_added: Vec<usize>,
    /// Reused buffer for index candidates, so culling allocates nothing either.
    scratch_candidates: Vec<usize>,
    /// Indices that should have a widget, computed by the last cull and applied in
    /// the next mutate pass.
    pending: Option<Vec<usize>>,
    /// Visible region in canvas coordinates, pushed down by the parent.
    visible_rect: Rect,
    /// Detail level pushed down by the parent.
    detail: Option<Detail>,
    layouts: u64,
    child_layouts: u64,
    composes: u64,
    builds: u64,
    far_repaints: u64,
    visits: u64,
    link_repaints: u64,
    hit_queries: u64,
    hit_node_tests: u64,
    hit_curve_tests: u64,
}

// `CanvasLayer` owns this widget completely and reaches into it during layout to
// push down the view transform, the visible rect and the detail level. This is the
// case the escape hatch is documented for: "a parent widget completely controls
// their child, but needs it to be a separate widget for user interaction to behave
// as expected".
impl AllowRawMut for CanvasContent {}

impl CanvasContent {
    fn new(slots: Vec<Slot>, source: Box<dyn NodeSource>) -> Self {
        let index = SpatialIndex::build(slots.iter().map(|s| Rect::from_origin_size(s.pos, s.size)));
        Self {
            index,
            slots,
            source,
            live: Vec::new(),
            visible: Vec::new(),
            far: FarField::default(),
            links: LinkLayer::default(),
            link_style: LinkStyle::default(),
            active: None,
            hovered: None,
            controls_on_hover: false,
            detail_dirty: false,
            pending_stale: false,
            scratch_removed: Vec::new(),
            scratch_added: Vec::new(),
            scratch_candidates: Vec::new(),
            pending: None,
            visible_rect: Rect::ZERO,
            detail: None,
            layouts: 0,
            child_layouts: 0,
            composes: 0,
            builds: 0,
            far_repaints: 0,
            visits: 0,
            link_repaints: 0,
            hit_queries: 0,
            hit_node_tests: 0,
            hit_curve_tests: 0,
        }
    }

    // --- MARK: HIT TESTING

    /// What is under a canvas-space point.
    ///
    /// `scale` is how many screen pixels one canvas unit covers, which is what turns
    /// a tolerance in pixels into one in canvas units (§25.2).
    ///
    /// Answered from the model rather than from the widget tree, and that is the
    /// whole reason this exists: below the far-field threshold no node has a widget
    /// at all, and a link never has one. `find_widget_under_pointer` cannot see
    /// either of them, so the canvas has to answer for both.
    fn hit(&mut self, canvas_pos: Point, scale: f64) -> Option<CanvasHit> {
        self.hit_queries += 1;
        self.hit_node(canvas_pos).or_else(|| self.hit_link(canvas_pos, scale))
    }

    /// The topmost node under a point, or `None`.
    fn hit_node(&mut self, canvas_pos: Point) -> Option<CanvasHit> {
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        // A point, not a rect: the grid widens the query by its own slack, which is
        // derived from the widest node, so a node reaching into the cell from
        // outside is still a candidate (§24.2).
        self.index
            .candidates(Rect::from_points(canvas_pos, canvas_pos), &mut candidates);
        self.hit_node_tests += candidates.len() as u64;

        // Descending index order: children are placed in ascending order, so the
        // highest index is the one painted last and therefore on top.
        let mut found = None;
        for &index in candidates.iter().rev() {
            let slot = &self.slots[index];
            let rect = Rect::from_origin_size(slot.pos, slot.size);
            // The rectangle first because it is free and rejects almost everything;
            // the exact shape only for what survives.
            if rect.contains(canvas_pos) && self.source.hit(index, rect, canvas_pos) {
                found = Some(CanvasHit::Node {
                    index,
                    pos: rect.origin(),
                });
                break;
            }
        }
        self.scratch_candidates = candidates;
        found
    }

    /// The topmost link under a point, or `None`.
    ///
    /// Candidates are the links the canvas has recorded for this viewport, so what
    /// can be picked is exactly what is drawn — the selection rule of §24.4 and its
    /// limitation, inherited rather than reinvented. Choosing candidates a second
    /// way would mean a pointer that picks a curve nobody can see, or misses one
    /// everybody can.
    fn hit_link(&mut self, canvas_pos: Point, scale: f64) -> Option<CanvasHit> {
        if self.links.is_empty() {
            return None;
        }
        let radius = self.link_style.width / 2.0
            + if scale > f64::EPSILON {
                self.link_style.slop / scale
            } else {
                0.0
            };

        let mut examined = 0_u64;
        let mut found = None;
        // Reverse recorded order: the curves are stroked in ascending order, so the
        // last one drawn is the one on top.
        for &edge in self.links.recorded().iter().rev() {
            let link = self.links.edge(edge);
            let (Some(from), Some(to)) = (self.slots.get(link.from as usize), self.slots.get(link.to as usize)) else {
                continue;
            };
            examined += 1;
            let curve = link_curve(
                Rect::from_origin_size(from.pos, from.size),
                Rect::from_origin_size(to.pos, to.size),
            );
            if near_segment(curve.into(), canvas_pos, radius) {
                found = Some(CanvasHit::Link {
                    edge: edge as usize,
                    link,
                });
                break;
            }
        }
        self.hit_curve_tests += examined;
        found
    }

    /// Records what the pointer is over. Returns whether the canvas has to repaint.
    ///
    /// A repaint, never a layout: highlighting the link under the pointer changes
    /// pixels and nothing else. Materialising controls for the node under the pointer
    /// is a different question with a different answer, and it lives in
    /// [`CanvasLayer::set_active`].
    fn set_hovered(&mut self, hit: Option<CanvasHit>) -> bool {
        if self.hovered == hit {
            return false;
        }
        let was = self.hovered.and_then(CanvasHit::link);
        self.hovered = hit;
        was.is_some() || hit.and_then(CanvasHit::link).is_some()
    }

    /// The detail level node `index` should be built at.
    ///
    /// By default this is simply the canvas-wide level: at [`Detail::Full`] every node
    /// gets real controls. With [`CanvasLayer::with_controls_on_hover`] only the node
    /// under the pointer does, which is several times cheaper but swaps a painted
    /// stand-in for real widgets as the pointer arrives. See that method.
    fn effective_detail(&self, index: usize) -> Detail {
        let global = self.detail.unwrap_or(Detail::Full);
        if !self.controls_on_hover || global == Detail::Box {
            return global;
        }
        if self.active == Some(index) {
            Detail::Full
        } else {
            Detail::Simplified
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

        // Checking staleness is only worth it when a detail level actually changed —
        // a threshold crossing, or a new node under the pointer. Never during a pan.
        let mut removed = std::mem::take(&mut this.widget.scratch_removed);
        let mut added = std::mem::take(&mut this.widget.scratch_added);
        {
            let content = &*this.widget;
            diff_sorted(
                &content.live,
                &desired,
                |i| stale && content.slots[i].built != Some(content.build_spec(i)),
                &mut removed,
                &mut added,
            );
        }

        // Removing takes the widget out of the tree entirely. This is the whole
        // point: a stashed widget is still walked by every pass, a removed one is
        // not. Its state is not lost — it lives in the model behind `NodeSource`.
        let changed = !removed.is_empty() || !added.is_empty();
        for &index in &removed {
            if let Some(pod) = this.widget.slots[index].pod.take() {
                this.widget.slots[index].built = None;
                this.ctx.remove_child(pod);
            }
        }

        for &index in &added {
            let (detail, global) = this.widget.build_spec(index);
            let widget = this.widget.source.build(index, detail).with_props(CanvasDetail(global));
            this.widget.slots[index].pod = Some(widget.to_pod());
            this.widget.slots[index].built = Some((detail, global));
            this.widget.builds += 1;
        }

        this.widget.scratch_removed = removed;
        this.widget.scratch_added = added;
        this.widget.live = desired;
        if changed {
            this.ctx.children_changed();
            this.ctx.request_layout();
        }
        // The far-field repaint is requested by the parent, which is where culling
        // and the region check happen.
    }

    /// Records a new canvas-space position for a node.
    ///
    /// In far-field mode the node is part of this widget's own scene, so moving it
    /// means re-recording that scene rather than re-placing a child widget.
    fn store_child_pos(&mut self, index: usize, pos: Point) -> Invalidate {
        let far_field = self.far.active;
        let Some(slot) = self.slots.get_mut(index) else {
            return Invalidate::Nothing;
        };
        if slot.pos == pos {
            return Invalidate::Nothing;
        }
        slot.pos = pos;
        self.index.moved(index, pos);
        self.links.node_moved(index);
        if far_field {
            self.far.region = None;
            Invalidate::LayoutAndPaint
        } else {
            // Only this widget is dirtied. Sibling nodes keep their cached scenes;
            // the moved node keeps its own too, since only its position changed.
            Invalidate::Layout
        }
    }

    /// Computes the set of nodes inside the visible rect.
    ///
    /// Asks the grid for candidates and tests their rectangles exactly. The grid is
    /// what keeps this proportional to what is on screen rather than to the graph:
    /// the linear scan it replaced was invisible up to about 64 000 nodes and cost
    /// 6.8 ms a frame at a million (§24).
    fn cull(&mut self) {
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(self.visible_rect, &mut candidates);

        let mut visible = Vec::with_capacity(self.visible.len() + 8);
        for &index in &candidates {
            let slot = &self.slots[index];
            if Rect::from_origin_size(slot.pos, slot.size).overlaps(self.visible_rect) {
                visible.push(index);
            }
        }
        self.visits += candidates.len() as u64;
        self.scratch_candidates = candidates;

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
        } else if self.far.region.take().is_some() {
            self.far.nodes.clear();
        }

        self.refresh_links();

        if desired != self.live || far_field != self.far.active || stale || self.far.dirty {
            self.far.active = far_field;
            self.pending_stale = stale;
            self.pending = Some(desired);
        } else {
            self.pending = None;
        }
    }

    /// Re-chooses which links are recorded when the viewport leaves their region.
    ///
    /// The same margin the far field uses, and for the same reason: the scene is in
    /// canvas coordinates, so panning inside the region reuses it untouched and only
    /// leaving it costs anything.
    fn refresh_links(&mut self) {
        if self.links.is_empty() {
            return;
        }
        let region = self.visible_rect.inflate(
            self.visible_rect.width() * FAR_OVERSCAN,
            self.visible_rect.height() * FAR_OVERSCAN,
        );
        if !self.links.needs_reselect(self.visible_rect) {
            return;
        }

        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(region, &mut candidates);
        self.visits += candidates.len() as u64;
        candidates.retain(|&i| Rect::from_origin_size(self.slots[i].pos, self.slots[i].size).overlaps(region));
        self.links.refresh(region, self.visible_rect, &candidates);
        self.scratch_candidates = candidates;
    }

    /// Re-records the far-field node set when the viewport leaves the painted region.
    ///
    /// The recorded scene lives in canvas coordinates, so panning and zooming inside
    /// the region cost one `Affine` and nothing else. The margin is what turns
    /// "re-record every frame" into "re-record when you have travelled half a
    /// screen": it is bought with a larger scene, which the paint pass appends every
    /// frame either way, so it should be generous but not unbounded.
    fn refresh_far_region(&mut self) {
        if self.far.region.is_some_and(|r| contains_rect(r, self.visible_rect)) {
            return;
        }

        let region = self.visible_rect.inflate(
            self.visible_rect.width() * FAR_OVERSCAN,
            self.visible_rect.height() * FAR_OVERSCAN,
        );

        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(region, &mut candidates);
        self.far.nodes.clear();
        for &index in &candidates {
            let slot = &self.slots[index];
            if Rect::from_origin_size(slot.pos, slot.size).overlaps(region) {
                self.far.nodes.push(index);
            }
        }
        self.visits += candidates.len() as u64;
        self.scratch_candidates = candidates;
        self.far.region = Some(region);
        self.far.dirty = true;
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
        self.layouts += 1;

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
                self.child_layouts += 1;
            }
            // The size is known from the model, so there is nothing to resolve.
            ctx.run_layout(pod, size);
            ctx.place_child(pod, pos);
        }
    }

    fn compose(&mut self, _ctx: &mut ComposeCtx<'_>) {
        self.composes += 1;
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        // Links go under the nodes, at every detail level: they are the graph's
        // structure, and a graph too small to show a node's controls still has to
        // show what is wired to what.
        if !self.links.is_empty() {
            self.link_repaints += 1;
            let stroke = Stroke::new(self.link_style.width);
            let hovered = self.hovered.and_then(CanvasHit::link);
            for &edge in self.links.recorded() {
                let link = self.links.edge(edge);
                let (Some(from), Some(to)) = (self.slots.get(link.from as usize), self.slots.get(link.to as usize))
                else {
                    continue;
                };
                let path = link_path(
                    Rect::from_origin_size(from.pos, from.size),
                    Rect::from_origin_size(to.pos, to.size),
                );
                let color = if hovered == Some(edge as usize) {
                    self.link_style.hover_color
                } else {
                    self.link_style.color
                };
                painter.stroke(&path, &stroke, color).draw();
            }
        }

        if !self.far.active {
            return;
        }
        self.far_repaints += 1;
        // One pass over the visible nodes, straight into this widget's cached scene.
        // The scene is in canvas coordinates, so panning and zooming re-use it via
        // the layer transform without re-encoding anything.
        for &index in &self.far.nodes {
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

/// Copies what a pick just learned into the published statistics.
///
/// Everything else in [`CanvasStats`] is refreshed during layout, which is the
/// cheapest honest place for it. Picks cannot use it: a hover is meant *not* to run
/// layout (§25.4), so counters that waited for one would report zero for the very
/// scenario they exist to measure — and the one place they would be seen is a
/// benchmark comparing them before and after.
fn publish_hit_stats(stats: &Cell<CanvasStats>, content: &CanvasContent) {
    let mut current = stats.get();
    current.hovered = content.hovered;
    current.counters.hit_queries = content.hit_queries;
    current.counters.hit_node_tests = content.hit_node_tests;
    current.counters.hit_curve_tests = content.hit_curve_tests;
    stats.set(current);
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
    /// How far past the viewport to keep nodes alive, as a fraction of the viewport.
    ///
    /// Culling exactly at the viewport edge makes nodes pop in mid-drag. Expressed as
    /// a fraction rather than in canvas units on purpose: a fixed canvas-space margin
    /// means a huge screen margin when zoomed in and a sliver when zoomed out, which
    /// is backwards.
    overscan: f64,
    /// Mirror of the content's counters, refreshed at the end of each layout.
    stats: Cell<CanvasStats>,
    /// Current pointer gesture.
    drag: Drag,
    /// Whether only the node under the pointer gets interactive controls.
    controls_on_hover: bool,
    /// Where the detail levels switch over.
    thresholds: DetailThresholds,
    /// Smallest and largest permitted zoom.
    zoom_limits: (f64, f64),
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
            overscan: DEFAULT_OVERSCAN,
            stats: Cell::new(CanvasStats {
                zoom: 1.0,
                ..CanvasStats::default()
            }),
            drag: Drag::None,
            controls_on_hover: false,
            thresholds: DetailThresholds::default(),
            zoom_limits: (0.02, 8.0),
            pending_links: None,
            link_style: LinkStyle::default(),
        }
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

    /// Restyles the links.
    pub fn with_link_style(mut self, style: LinkStyle) -> Self {
        self.link_style = style;
        self
    }

    pub fn with_controls_on_hover(mut self, enabled: bool) -> Self {
        self.controls_on_hover = enabled;
        self
    }

    /// The canvas-space to viewport-space transform.
    ///
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

    /// The region of canvas space currently visible, plus the overscan margin.
    fn visible_canvas_rect(&self) -> Rect {
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

    /// The canvas-space position of a child.
    pub fn child_pos(this: &mut WidgetMut<'_, Self>, index: usize) -> Option<Point> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.slots.get(index).map(|s| s.pos)
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

    /// Moves a child from an event handler.
    fn move_child_at(&mut self, index: usize, pos: Point, ctx: &mut EventCtx<'_>) {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        content.store_child_pos(index, pos).apply(&mut raw);
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
    fn hover(&mut self, pos: Point, ctx: &mut EventCtx<'_>) {
        let hit = self.hit_at(pos, ctx);
        self.set_hovered(hit, ctx);
        if self.controls_on_hover {
            self.set_active(hit.and_then(CanvasHit::node), ctx);
        }
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
                self.drag = match e.button {
                    // Left button drags a node if there is one under the pointer,
                    // and pans otherwise.
                    Some(PointerButton::Primary) => match self.hit_at(pos, ctx) {
                        Some(CanvasHit::Node { index, pos: child_pos }) => Drag::Node {
                            index,
                            grab: canvas_pos - child_pos,
                        },
                        // A link is pickable but not yet draggable: selection and
                        // rewiring are operators, and operators are `blazy-ops`
                        // (§11). Until then a press on a curve pans, as it did
                        // before curves could be picked at all.
                        Some(CanvasHit::Link { .. }) | None => Drag::Pan { last: pos },
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
                    Drag::None => self.hover(pos, ctx),
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

        // Clip to the viewport so children panned out of view cannot paint over the
        // surrounding UI, and so Masonry excludes them from hit testing.
        ctx.set_clip_path(Rect::from_origin_size(Point::ORIGIN, size));

        let visible_rect = self.visible_canvas_rect();
        let detail = self.thresholds.for_scale(self.zoom());
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
            content.controls_on_hover = self.controls_on_hover;
            content.visible_rect = visible_rect;
            if content.detail != Some(detail) {
                content.detail = Some(detail);
                content.detail_dirty = true;
            }
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

        let zoom = self.zoom();
        let (content, _) = ctx.get_raw(&mut self.content);
        self.stats.set(CanvasStats {
            total: content.slots.len(),
            materialised: content.live.len(),
            detail: content.detail,
            zoom,
            hovered: content.hovered,
            counters: CanvasCounters {
                content_layouts: content.layouts,
                child_layouts: content.child_layouts,
                composes: content.composes,
                builds: content.builds,
                far_repaints: content.far_repaints,
                slot_visits: content.visits,
                link_repaints: content.link_repaints,
                link_reselects: content.links.refreshes(),
                hit_queries: content.hit_queries,
                hit_node_tests: content.hit_node_tests,
                hit_curve_tests: content.hit_curve_tests,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(live: &[usize], desired: &[usize]) -> (Vec<usize>, Vec<usize>) {
        let (mut removed, mut added) = (Vec::new(), Vec::new());
        diff_sorted(live, desired, |_| false, &mut removed, &mut added);
        (removed, added)
    }

    #[test]
    fn diff_of_equal_sets_is_empty() {
        assert_eq!(diff(&[1, 2, 3], &[1, 2, 3]), (vec![], vec![]));
    }

    #[test]
    fn diff_reports_arrivals_and_departures() {
        assert_eq!(diff(&[1, 3, 5], &[3, 4, 5, 6]), (vec![1], vec![4, 6]));
        assert_eq!(diff(&[], &[0, 1]), (vec![], vec![0, 1]));
        assert_eq!(diff(&[0, 1], &[]), (vec![0, 1], vec![]));
    }

    #[test]
    fn stale_entries_are_rebuilt_in_place() {
        let (mut removed, mut added) = (Vec::new(), Vec::new());
        diff_sorted(&[1, 2, 3], &[1, 2, 3], |i| i == 2, &mut removed, &mut added);
        assert_eq!((removed, added), (vec![2], vec![2]));
    }

    #[test]
    fn diff_reuses_its_buffers() {
        let (mut removed, mut added) = (vec![99], vec![99]);
        diff_sorted(&[1], &[1], |_| false, &mut removed, &mut added);
        assert!(removed.is_empty() && added.is_empty(), "stale contents must be cleared");
    }

    #[test]
    fn contains_rect_is_inclusive() {
        let outer = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(contains_rect(outer, outer));
        assert!(contains_rect(outer, Rect::new(1.0, 1.0, 9.0, 9.0)));
        assert!(!contains_rect(outer, Rect::new(-1.0, 0.0, 5.0, 5.0)));
    }

    #[test]
    fn detail_thresholds_are_ordered() {
        let thresholds = DetailThresholds::default();
        assert_eq!(thresholds.for_scale(1.0), Detail::Full);
        assert_eq!(thresholds.for_scale(0.2), Detail::Simplified);
        assert_eq!(thresholds.for_scale(0.01), Detail::Box);
    }

    // --- MARK: HIT TESTS

    /// A source whose nodes are rounded rectangles, like a real one.
    struct RoundedSource {
        shape: blazy_shape::ShapeHit,
        size: Size,
    }

    impl RoundedSource {
        fn new(size: Size) -> Self {
            Self {
                shape: blazy_shape::ShapeHit::fill(masonry::kurbo::RoundedRect::from_rect(
                    Rect::from_origin_size(Point::ORIGIN, size),
                    12.0,
                )),
                size,
            }
        }
    }

    impl NodeSource for RoundedSource {
        fn build(&mut self, _index: usize, _detail: Detail) -> NewWidget<dyn Widget> {
            unimplemented!("these tests never materialise a widget: that is the point")
        }

        fn hit(&mut self, _index: usize, rect: Rect, point: Point) -> bool {
            assert_eq!(rect.size(), self.size);
            self.shape.contains(point - rect.origin().to_vec2(), 1.0)
        }
    }

    /// A canvas content over the given node rectangles and edges, already culled.
    ///
    /// Built directly rather than through a harness: everything under test answers
    /// from the model, so a widget tree would only add a way for the test to be
    /// about something else.
    fn content(rects: &[Rect], edges: Vec<Link>, visible: Rect) -> CanvasContent {
        let size = rects.first().map_or(Size::ZERO, Rect::size);
        let slots = rects
            .iter()
            .map(|r| Slot {
                pos: r.origin(),
                size: r.size(),
                pod: None,
                built: None,
            })
            .collect();
        let mut content = CanvasContent::new(slots, Box::new(RoundedSource::new(size)));
        content.links = LinkLayer::new(edges, rects.len());
        content.links.invalidate();
        content.detail = Some(Detail::Full);
        content.visible_rect = visible;
        content.cull();
        content
    }

    fn node_rect(x: f64, y: f64) -> Rect {
        Rect::from_origin_size(Point::new(x, y), Size::new(100.0, 60.0))
    }

    const EVERYTHING: Rect = Rect::new(-1000.0, -1000.0, 1000.0, 1000.0);

    /// The claim of §6.1: the hit geometry is the shape, not the box it sits in.
    #[test]
    fn a_point_in_the_corner_of_a_node_misses_it() {
        let mut canvas = content(&[node_rect(0.0, 0.0)], Vec::new(), EVERYTHING);
        let corner = Point::new(1.0, 1.0);

        assert!(node_rect(0.0, 0.0).contains(corner), "inside the rectangle");
        assert_eq!(canvas.hit(corner, 1.0), None, "outside the rounded body");
        assert!(matches!(
            canvas.hit(Point::new(50.0, 30.0), 1.0),
            Some(CanvasHit::Node { index: 0, .. })
        ));
    }

    /// Picking a link is the case Masonry cannot answer at all: a curve is not a
    /// widget, so nothing in the tree knows it is there.
    #[test]
    fn a_link_is_picked_along_its_curve() {
        let mut canvas = content(
            &[node_rect(0.0, 0.0), node_rect(400.0, 0.0)],
            vec![Link::new(0, 1)],
            EVERYTHING,
        );

        // The curve runs from the right edge of one node to the left edge of the
        // other, both centred on y = 30.
        assert!(matches!(
            canvas.hit(Point::new(250.0, 30.0), 1.0),
            Some(CanvasHit::Link { edge: 0, .. })
        ));
        assert_eq!(canvas.hit(Point::new(250.0, 90.0), 1.0), None);
    }

    /// Nodes are painted over links, so they are picked over links too (§25.3).
    #[test]
    fn a_node_wins_over_a_link_running_under_it() {
        let mut canvas = content(
            &[node_rect(0.0, 0.0), node_rect(400.0, 0.0), node_rect(200.0, 0.0)],
            vec![Link::new(0, 1)],
            EVERYTHING,
        );
        let on_both = Point::new(250.0, 30.0);

        assert!(
            blazy_shape::near_segment(
                crate::links::link_curve(node_rect(0.0, 0.0), node_rect(400.0, 0.0)).into(),
                on_both,
                4.0
            ),
            "the point really is on the curve"
        );
        assert!(matches!(
            canvas.hit(on_both, 1.0),
            Some(CanvasHit::Node { index: 2, .. })
        ));
    }

    /// The tolerance is in screen pixels, so the same canvas point picks a link when
    /// the canvas is zoomed out and misses it when zoomed in (§25.2).
    #[test]
    fn the_link_tolerance_follows_the_zoom() {
        let mut canvas = content(
            &[node_rect(0.0, 0.0), node_rect(400.0, 0.0)],
            vec![Link::new(0, 1)],
            EVERYTHING,
        );
        // Three canvas units off the curve, with a stroke one unit wide either side.
        let near = Point::new(250.0, 33.0);

        assert!(canvas.hit(near, 1.0).is_some(), "3 px away at 1x");
        assert_eq!(canvas.hit(near, 8.0), None, "24 px away at 8x");
        assert!(canvas.hit(near, 0.25).is_some(), "well under a pixel at 0.25x");
    }

    /// What can be picked is what is drawn, including where that is not enough: a
    /// link whose ends are both outside the recorded region is neither (§24.4).
    #[test]
    fn a_link_that_is_not_drawn_is_not_picked() {
        let mut canvas = content(
            &[node_rect(-5000.0, 0.0), node_rect(5000.0, 0.0)],
            vec![Link::new(0, 1)],
            Rect::new(-200.0, -200.0, 200.0, 200.0),
        );

        assert!(canvas.links.recorded().is_empty(), "neither end is near the viewport");
        assert_eq!(canvas.hit(Point::new(0.0, 30.0), 1.0), None);
    }

    /// Nodes below the far-field threshold have no widget at all, and must still be
    /// pickable — the reason picking asks the model and not the tree (§20.6).
    #[test]
    fn a_node_with_no_widget_is_still_picked() {
        let mut canvas = content(&[node_rect(0.0, 0.0)], Vec::new(), EVERYTHING);
        canvas.detail = Some(Detail::Box);
        canvas.cull();

        assert!(canvas.live.is_empty(), "far field: nothing is materialised");
        assert!(matches!(
            canvas.hit(Point::new(50.0, 30.0), 1.0),
            Some(CanvasHit::Node { index: 0, .. })
        ));
    }

    /// Picking must not walk the graph: the candidates come from the grid.
    #[test]
    fn picking_does_not_examine_the_whole_graph() {
        let rects: Vec<Rect> = (0..4000)
            .map(|i| node_rect((i % 80) as f64 * 220.0, (i / 80) as f64 * 220.0))
            .collect();
        let mut canvas = content(&rects, Vec::new(), Rect::new(0.0, 0.0, 1100.0, 750.0));

        let before = canvas.hit_node_tests;
        canvas.hit(Point::new(50.0, 30.0), 1.0);
        let examined = canvas.hit_node_tests - before;

        assert!(examined < 64, "examined {examined} geometries of 4000");
    }

    /// A hover highlights a curve, and a highlight is a repaint. Nothing here is
    /// allowed to ask for layout — that is what `set_active` is for.
    #[test]
    fn only_a_link_hover_asks_for_a_repaint() {
        let mut canvas = content(&[node_rect(0.0, 0.0)], Vec::new(), EVERYTHING);
        let node = Some(CanvasHit::Node {
            index: 0,
            pos: Point::ORIGIN,
        });
        let link = Some(CanvasHit::Link {
            edge: 0,
            link: Link::new(0, 1),
        });

        assert!(!canvas.set_hovered(node), "a node highlight is not drawn");
        assert!(!canvas.set_hovered(node), "and an unchanged hover is not a change");
        assert!(canvas.set_hovered(link), "arriving on a curve repaints it");
        assert!(canvas.set_hovered(None), "and leaving it repaints it back");
    }

    #[test]
    fn detail_thresholds_are_configurable() {
        let thresholds = DetailThresholds {
            full: 0.6,
            simplified: 0.25,
        };
        assert_eq!(thresholds.for_scale(0.4), Detail::Simplified);
        assert_eq!(thresholds.for_scale(0.1), Detail::Box);
    }
}
