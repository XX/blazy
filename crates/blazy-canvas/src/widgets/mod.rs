//! The two widgets of a canvas (§20.3), and the machinery they share.
//!
//! [`CanvasContent`] lives here and [`CanvasLayer`] in the child module `layer`, and
//! the nesting is deliberate: the layer drives the content through some thirty private
//! fields and methods. A child module sees its parent's private items; a sibling would
//! need every one of them widened to `pub(crate)`, which would turn the split into a
//! loss of encapsulation rather than a gain in readability.

use std::cell::Cell;

use blazy_shape::near_segment;
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AllowRawMut, ChildrenIds, ComposeCtx, LayoutCtx, MeasureCtx, MutateCtx, NoAction, PaintCtx,
    PropertiesRef, RawCtx, RegisterCtx, Widget, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, BezPath, Point, Rect, Shape, Size, Stroke};
use masonry::layout::{LenReq, Length};

use crate::detail::{CanvasDetail, Detail, DetailBudget};
use crate::index::SpatialIndex;
use crate::links::{Link, LinkLayer, LinkStyle, link_curve, push_link};
use crate::source::NodeSource;
use crate::stats::{CanvasHit, CanvasStats};

mod layer;
#[cfg(test)]
mod tests;

pub use self::layer::CanvasLayer;

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
///
/// A removed node leaves its slot behind, dead. The slot is the node's *name* — a
/// canvas, a selection, a link and a history are all written in it — and §41.2 made the
/// rule for areas that nothing renumbers what survives an operation. Compacting the
/// array on a removal would renumber every node after it; a hole costs one slot and is
/// handed out again by the next insertion.
struct Slot {
    /// Whether a node is here at all.
    alive: bool,
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
/// avoid entirely.
///
/// **A quarter, measured rather than guessed (§35.2).** It started at a half, from the
/// reasoning above and nothing else; once the frame could be counted in path segments
/// the trade turned out to be priced steeply. At a far zoom over a graph that carries
/// on past the viewport, a half records 47 502 segments and a quarter 27 580 — a fifth
/// to a quarter of the frame's rasterisation — and the re-selections it buys back are
/// 0.02 per frame against 0.00, which is one every fifty. Tighter still is cheaper
/// again, and 0.10 crosses the line where a pan starts re-choosing the link set often
/// enough for `pan_does_not_reselect_links` to see it.
pub(crate) const FAR_OVERSCAN: f64 = 0.25;

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

/// A dead slot: the hole a removal leaves, and what a gap in the names is filled with.
const EMPTY_SLOT: Slot = Slot {
    alive: false,
    pos: Point::ZERO,
    size: Size::ZERO,
    pod: None,
    built: None,
};

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

/// The canvas-space box the curve between two nodes occupies.
///
/// One place, because the box is used for three different decisions — whether a link is
/// too short to draw, whether the pointer can be near it, and nothing else may disagree
/// with either.
fn link_bounds(from: &Slot, to: &Slot) -> Rect {
    link_curve(
        Rect::from_origin_size(from.pos, from.size),
        Rect::from_origin_size(to.pos, to.size),
    )
    .bounding_box()
}

/// Whether `outer` fully contains `inner`.
pub(crate) fn contains_rect(outer: Rect, inner: Rect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

/// How far the viewport may zoom *in* on a recorded region before it is redone.
///
/// Four times the area, that is two times the zoom. The rule cannot be an absolute
/// multiple of the viewport, and that took a measurement to notice (§35.3): the region
/// starts out `(1 + 2 * overscan)^2` times the viewport, so a fixed ceiling means a
/// tighter margin tolerates *more* zooming before it re-records — the recorded set goes
/// staler as the margin gets smaller, which is backwards. Narrowing the default margin
/// from a half to a quarter left 1015 link curves hidden at a zoom whose nodes were
/// widgets again, and it was this constant, not the margin, that was wrong.
const ZOOM_SLACK: f64 = 4.0;

/// How much larger than the viewport a region recorded with this margin may be before
/// it is redone.
pub(crate) fn region_slack(far_overscan: f64) -> f64 {
    let recorded = (1.0 + 2.0 * far_overscan).powi(2);
    recorded * ZOOM_SLACK
}

/// Whether a region recorded earlier still serves this viewport.
///
/// Containment alone is not enough, and that was a real bug rather than a subtlety
/// (§28): a region chosen while zoomed out contains every viewport that follows, so
/// asking only "did the viewport leave it?" means the recorded set never shrinks
/// again. Zoom out to see the whole graph, zoom back in, and the canvas keeps drawing
/// every edge in it — measured at 9857 curves at a zoom whose viewport holds 96.
pub(crate) fn region_covers(region: Rect, visible: Rect, slack: f64) -> bool {
    contains_rect(region, visible) && region.area() <= visible.area() * slack
}

// --- MARK: CONTENT

/// The transformed inner half of a [`CanvasLayer`].
///
/// Holds the freely positioned children and carries the pan/zoom transform. Not
/// constructed directly; a [`CanvasLayer`] owns one.
pub struct CanvasContent {
    /// Geometry for every node; widgets for the materialised ones only.
    ///
    /// Indexed by name, holes included (§43).
    slots: Vec<Slot>,
    /// Live nodes among the slots.
    ///
    /// Counted rather than derived: `stats` reports it on every layout, and a walk over
    /// the names would put a cost proportional to the graph back into every frame —
    /// which is the one thing §24 took out.
    alive_nodes: usize,
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
    /// How far past the viewport the far field and the link set are recorded, pushed
    /// down by the parent.
    far_overscan: f64,
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
    /// A spare index buffer, so the set handed to the mutate pass is not a fresh
    /// allocation every frame.
    ///
    /// `pending` and `live` swap through here: the cull fills this one, `apply_pending`
    /// makes it the live set and hands back the buffer it replaced. Without it a pan —
    /// which changes the set on most frames — allocates and frees one `Vec` per frame
    /// per canvas, which is exactly what the other `scratch_*` fields exist to avoid.
    scratch_desired: Vec<usize>,
    /// Reused paths for the two link batches, so a repaint allocates nothing.
    ///
    /// One holds every ordinary link, the other the hovered one; each is stroked with
    /// a single command. `BezPath` has no `clear`, but `truncate(0)` keeps the
    /// capacity, which is the whole reason these are fields.
    scratch_links: BezPath,
    scratch_hot_links: BezPath,
    /// Reused buffer for the far-field batch handed to [`NodeSource::paint_far`].
    scratch_far: Vec<(usize, Rect)>,
    /// Indices that should have a widget, computed by the last cull and applied in
    /// the next mutate pass.
    pending: Option<Vec<usize>>,
    /// The canvas-space region nodes are kept live in: the viewport **plus the
    /// overscan margin**, pushed down by the parent.
    ///
    /// Not the viewport, and the name says so because the difference is load-bearing:
    /// `region_covers` compares a recorded region against this rect by proportion as
    /// well as by containment (§35.3), and the margin is part of the proportion.
    live_rect: Rect,
    /// How many screen pixels one canvas unit covers, pushed down by the parent.
    ///
    /// The zoom only. The rest of the chain — a region's `ui_scale`, the device scale
    /// — would need `window_transform`, which `LayoutCtx` does not offer; the pointer
    /// path uses the full product because `EventCtx` does (see `hit_scale`). It
    /// matters for one thing, [`LinkStyle::min_screen_length`], where the error is a
    /// factor on a two-pixel threshold, and it is written down rather than hidden.
    scale: f64,
    /// The coarsest level the zoom still makes readable, pushed down by the parent.
    ///
    /// Half of the decision. The other half is [`DetailBudget`], and it cannot be
    /// taken by the parent because it needs the number of visible nodes, which only
    /// the cull knows.
    readable: Option<Detail>,
    /// Cost ceiling, pushed down by the parent.
    budget: DetailBudget,
    /// Effective level, decided in `cull` as the stricter of the two rules.
    detail: Option<Detail>,
    layouts: u64,
    child_layouts: u64,
    composes: u64,
    builds: u64,
    /// Nodes inserted or removed.
    node_edits: u64,
    /// Links inserted or removed.
    link_edits: u64,
    level_switches: u64,
    far_repaints: u64,
    far_records: u64,
    visits: u64,
    link_repaints: u64,
    hit_queries: u64,
    hit_node_tests: u64,
    hit_curve_tests: u64,
    hit_curve_scans: u64,
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
        let alive_nodes = slots.iter().filter(|slot| slot.alive).count();
        Self {
            index,
            alive_nodes,
            slots,
            source,
            live: Vec::new(),
            visible: Vec::new(),
            far: FarField::default(),
            links: LinkLayer::default(),
            link_style: LinkStyle::default(),
            far_overscan: FAR_OVERSCAN,
            active: None,
            hovered: None,
            controls_on_hover: false,
            detail_dirty: false,
            pending_stale: false,
            scratch_removed: Vec::new(),
            scratch_added: Vec::new(),
            scratch_candidates: Vec::new(),
            scratch_desired: Vec::new(),
            scratch_links: BezPath::new(),
            scratch_hot_links: BezPath::new(),
            scratch_far: Vec::new(),
            pending: None,
            live_rect: Rect::ZERO,
            scale: 1.0,
            readable: None,
            budget: DetailBudget::default(),
            detail: None,
            layouts: 0,
            child_layouts: 0,
            composes: 0,
            builds: 0,
            node_edits: 0,
            link_edits: 0,
            level_switches: 0,
            far_repaints: 0,
            far_records: 0,
            visits: 0,
            link_repaints: 0,
            hit_queries: 0,
            hit_node_tests: 0,
            hit_curve_tests: 0,
            hit_curve_scans: 0,
        }
    }

    /// The slot of a live node, or `None` for a name nothing holds.
    fn live_slot(&self, index: usize) -> Option<&Slot> {
        self.slots.get(index).filter(|slot| slot.alive)
    }

    /// The rectangle of a live node.
    fn live_rect_of(&self, index: usize) -> Option<Rect> {
        self.live_slot(index)
            .map(|slot| Rect::from_origin_size(slot.pos, slot.size))
    }

    /// Nodes the canvas currently holds. Holes left by removals do not count.
    fn node_count(&self) -> usize {
        self.alive_nodes
    }

    // --- MARK: STRUCTURE

    /// Puts a node into the canvas under the name `index`.
    ///
    /// The name is the caller's, not the canvas's, and that is a decision of §43: the
    /// model is the truth (§30), a name is part of the truth, and this array is a mirror
    /// of it. A name a removal freed is handed out again by whoever owns the model; a
    /// name beyond the end grows the mirror with holes in between.
    fn insert_node(&mut self, index: usize, pos: Point, size: Size) -> Invalidate {
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || EMPTY_SLOT);
        }
        if !self.slots[index].alive {
            self.alive_nodes += 1;
        }
        self.slots[index] = Slot {
            alive: true,
            pos,
            size,
            pod: None,
            built: None,
        };
        self.index.insert(index, Rect::from_origin_size(pos, size));
        self.node_edits += 1;
        self.structure_changed();
        Invalidate::LayoutAndPaint
    }

    /// Takes the node named `index` out, and hands back the links that went with it.
    ///
    /// A widget it had is dropped in the next mutate pass, which is the only pass that
    /// may remove a child: the cull no longer finds the node, so `apply_pending` removes
    /// it like any other node that left the view.
    fn remove_node(&mut self, index: usize) -> Vec<(u32, Link)> {
        if self.live_slot(index).is_none() {
            return Vec::new();
        }
        let links = self.links.remove_node(index);
        self.slots[index].alive = false;
        self.alive_nodes -= 1;
        self.index.remove(index);
        self.node_edits += 1;
        self.structure_changed();
        links
    }

    /// Adds a link and hands back the name it got.
    fn insert_link(&mut self, link: Link) -> u32 {
        let name = self.links.insert(link, self.slots.len());
        self.link_edits += 1;
        self.structure_changed();
        name
    }

    /// Removes the link named `name`.
    fn remove_link(&mut self, name: u32) -> Option<Link> {
        let link = self.links.remove(name)?;
        self.link_edits += 1;
        self.structure_changed();
        Some(link)
    }

    /// Puts a link back under the name it had, for undo.
    fn restore_link(&mut self, name: u32, link: Link) {
        self.links.restore(name, link, self.slots.len());
        self.link_edits += 1;
        self.structure_changed();
    }

    /// What every structural edit has to invalidate.
    ///
    /// Both recorded sets are chosen for a *region* and re-chosen when the view leaves
    /// it (§28, §35.2). A structural edit does not move the view, so nothing else would
    /// ever notice it — the new node would not be drawn and the removed one would keep
    /// being drawn, both until the next pan. That is §28.4's lesson from the other side:
    /// a counter of work per frame cannot see what is held, and a set chosen by the view
    /// cannot see a change that is not the view's.
    fn structure_changed(&mut self) {
        self.far.region = None;
        self.far.dirty = true;
        self.links.invalidate();
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
            let Some(rect) = self.live_rect_of(index) else {
                continue;
            };
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

    /// A link under a point, or `None`.
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
        let mut scanned = 0_u64;
        let mut found = None;
        // Any of the links under the pointer, not the topmost one: the curves are
        // stroked batched by style, so recorded order is no longer drawing order and
        // "the last one drawn" cannot be recovered from this list (§31.2). Declaring
        // the order undefined is the cheap half of that trade — the alternative is a
        // command per style change, which is the cost the batch exists to remove. What
        // still holds is the property that matters: the candidates are exactly the
        // links that are drawn.
        for (at, &edge) in self.links.recorded().iter().enumerate() {
            scanned += 1;
            // The stored box first, and it is what keeps this off the zoom: the
            // candidate set is bounded by the recorded region rather than by the
            // viewport, so at an overview zoom it holds thousands of curves and all but
            // a handful are nowhere near the pointer. A box that has not been measured
            // yet answers `None` and the curve is tested, which is the safe direction.
            if let Some(box_of_a_curve) = self.links.recorded_bounds(at)
                && !box_of_a_curve.inflate(radius, radius).contains(canvas_pos)
            {
                continue;
            }
            let Some(link) = self.links.edge(edge) else {
                continue;
            };
            let (Some(from), Some(to)) = (
                self.live_rect_of(link.from as usize),
                self.live_rect_of(link.to as usize),
            ) else {
                continue;
            };
            examined += 1;
            let curve = link_curve(from, to);
            if near_segment(curve.into(), canvas_pos, radius) {
                found = Some(CanvasHit::Link {
                    edge: edge as usize,
                    link,
                });
                break;
            }
        }
        self.hit_curve_tests += examined;
        self.hit_curve_scans += scanned;
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
        // The set becomes the live one and the buffer it replaces goes to the pool,
        // which is what makes the next cull allocation-free.
        this.widget.scratch_desired = std::mem::replace(&mut this.widget.live, desired);
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
        let Some(slot) = self.slots.get_mut(index).filter(|slot| slot.alive) else {
            return Invalidate::Nothing;
        };
        if slot.pos == pos {
            return Invalidate::Nothing;
        }
        slot.pos = pos;
        self.index.moved(index, pos);
        let slots = &self.slots;
        self.links.node_moved(index, |link| {
            match (slots.get(link.from as usize), slots.get(link.to as usize)) {
                (Some(from), Some(to)) if from.alive && to.alive => link_bounds(from, to),
                _ => Rect::ZERO,
            }
        });
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
    /// milliseconds a frame beyond that — `index.rs` has the figures (§24).
    fn cull(&mut self) {
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(self.live_rect, &mut candidates);

        // The last frame's buffer, emptied: the visible set is rebuilt on every layout
        // and is the same size from one frame to the next.
        let mut visible = std::mem::take(&mut self.visible);
        visible.clear();
        for &index in &candidates {
            let slot = &self.slots[index];
            if slot.alive && Rect::from_origin_size(slot.pos, slot.size).overlaps(self.live_rect) {
                visible.push(index);
            }
        }
        self.visits += candidates.len() as u64;
        self.scratch_candidates = candidates;

        // The level is decided here rather than by the parent, and this is the only
        // place it can be: the cost rule needs the number of visible nodes, and that
        // number is what the loop above has just computed. Deciding it earlier, from
        // the zoom alone, is what tied the tree's size to the graph's density —
        // 442 nodes at zoom 0.21 on 5000 nodes, 224 on 20 000, same zoom, same
        // decision, twice the tree (§29.1). Nothing here costs an extra pass: the
        // estimate is a multiplication, and the set it changes is computed below.
        //
        // The two rules are independent and the coarser wins. `Detail` is ordered
        // finest-first, so that is `max`.
        let readable = self.readable.unwrap_or(Detail::Full);
        let affordable = self.budget.level_for(visible.len(), self.detail);
        let level = readable.max(affordable);
        if self.detail != Some(level) {
            // Only a change between two known levels is a switch; the first layout is
            // not one, or every canvas would report one before it has shown anything.
            if self.detail.is_some() {
                self.level_switches += 1;
            }
            self.detail = Some(level);
            self.detail_dirty = true;
        }

        // Below the box threshold the canvas paints nodes itself, so nothing is
        // materialised. This is what keeps a fully zoomed-out graph affordable: the
        // visible set stops bounding the cost, so the cost must stop depending on
        // widgets. See `paint`.
        let far_field = level == Detail::Box;
        // A cull whose `pending` nobody applied yet owns a buffer; otherwise the pool
        // has one. Either way this is the last allocation of it.
        let mut desired = self
            .pending
            .take()
            .unwrap_or_else(|| std::mem::take(&mut self.scratch_desired));
        desired.clear();
        if !far_field {
            desired.extend_from_slice(&visible);
        }

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
            // Nothing to apply, so the buffer goes back to the pool rather than away.
            self.scratch_desired = desired;
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
        let region = self.live_rect.inflate(
            self.live_rect.width() * self.far_overscan,
            self.live_rect.height() * self.far_overscan,
        );
        if !self.links.needs_reselect(self.live_rect) {
            return;
        }

        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(region, &mut candidates);
        self.visits += candidates.len() as u64;
        candidates.retain(|&i| self.live_rect_of(i).is_some_and(|rect| rect.overlaps(region)));
        let reselected = self.links.refresh(region, self.live_rect, &candidates);
        self.scratch_candidates = candidates;
        if reselected {
            self.measure_links();
        }
    }

    /// Measures the recorded curves and drops the ones too short to be seen.
    ///
    /// Here rather than in `paint`, and that placement is the design (§31.4). The set
    /// chosen here is the one both the picture and the pointer read, so a link leaves
    /// both at once and they cannot disagree — the property `link_curve` exists to
    /// protect. It also adds no invalidation of its own: the rule is evaluated when the
    /// set is re-chosen anyway, and between selections it is simply a little stale,
    /// which shows a hairline slightly longer than needed and costs a few curves. There
    /// is no error in the other direction.
    ///
    /// The bounding box each curve occupies is *kept* rather than thrown away with the
    /// verdict, because a pick needs exactly the same box: the candidate set is the
    /// recorded one, which is bounded by the region and therefore grows as the view
    /// pulls back (§28), and rebuilding every curve to answer a hover is what that cost
    /// used to be spent on.
    fn measure_links(&mut self) {
        // Screen pixels into canvas units, the same conversion the pick tolerance
        // makes and for the same reason: canvas units span a factor of 400 across the
        // zoom range, so a threshold expressed in them would mean something different
        // at each end (§25.2). A threshold of zero, or a degenerate scale, measures the
        // curves and drops none.
        let min_screen = self.link_style.min_screen_length;
        let min_canvas = if min_screen > 0.0 && self.scale > f64::EPSILON {
            min_screen / self.scale
        } else {
            0.0
        };
        let slots = &self.slots;
        self.links.measure_recorded(|link| {
            let (Some(from), Some(to)) = (
                slots.get(link.from as usize).filter(|s| s.alive),
                slots.get(link.to as usize).filter(|s| s.alive),
            ) else {
                // An edge naming a node that does not exist is skipped when drawn.
                // Keeping it here keeps "hidden" meaning "too short to see"; the empty
                // box it gets rejects it from every pick, which is the same answer.
                return Some(Rect::ZERO);
            };
            let bounds = link_bounds(from, to);
            // The diagonal of the box the curve occupies, not the chord: a link that
            // bows away and comes back is visible even when its endpoints nearly
            // coincide. It is also the conservative choice — never smaller than either
            // side — and a rule that removes picture should err towards keeping it.
            let size = bounds.size();
            (size.width.hypot(size.height) >= min_canvas).then_some(bounds)
        });
    }

    /// Re-records the far-field node set when the viewport leaves the painted region.
    ///
    /// The recorded scene lives in canvas coordinates, so panning and zooming inside
    /// the region cost one `Affine` and nothing else. How wide the region is, and what
    /// that margin trades against, is [`FAR_OVERSCAN`].
    fn refresh_far_region(&mut self) {
        if self
            .far
            .region
            .is_some_and(|r| region_covers(r, self.live_rect, region_slack(self.far_overscan)))
        {
            return;
        }

        let region = self.live_rect.inflate(
            self.live_rect.width() * self.far_overscan,
            self.live_rect.height() * self.far_overscan,
        );

        self.far_records += 1;
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(region, &mut candidates);
        self.far.nodes.clear();
        for &index in &candidates {
            let slot = &self.slots[index];
            if slot.alive && Rect::from_origin_size(slot.pos, slot.size).overlaps(region) {
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

            // One path per style, and one command per path. The curves keep apart
            // because each link starts a subpath; what they lose is their order
            // relative to each other, which is why picking no longer claims to return
            // the topmost link. Stroking each link on its own instead costs 15x per
            // curve in every frame the scene is appended — which is every frame, idle
            // or not (§31.1).
            let mut plain = std::mem::take(&mut self.scratch_links);
            let mut hot = std::mem::take(&mut self.scratch_hot_links);
            plain.truncate(0);
            hot.truncate(0);
            for &edge in self.links.recorded() {
                let Some(link) = self.links.edge(edge) else {
                    continue;
                };
                let (Some(from), Some(to)) = (
                    self.live_rect_of(link.from as usize),
                    self.live_rect_of(link.to as usize),
                ) else {
                    continue;
                };
                let path = if hovered == Some(edge as usize) {
                    &mut hot
                } else {
                    &mut plain
                };
                push_link(path, from, to);
            }
            if !plain.is_empty() {
                painter.stroke(&plain, &stroke, self.link_style.color).draw();
            }
            // Last, so the highlighted link is the one on top — the only ordering the
            // batch still guarantees, and the only one that matters.
            if !hot.is_empty() {
                painter.stroke(&hot, &stroke, self.link_style.hover_color).draw();
            }
            self.scratch_links = plain;
            self.scratch_hot_links = hot;
        }

        if !self.far.active {
            return;
        }
        self.far_repaints += 1;
        // The whole set in one call, straight into this widget's cached scene, which
        // is in canvas coordinates so panning and zooming re-use it via the layer
        // transform without re-encoding anything. Handing over the set rather than
        // calling per node is what lets the application group its fills: the frame
        // costs commands, and a command's content is nearly free next to its
        // existence (§31.1).
        let mut batch = std::mem::take(&mut self.scratch_far);
        batch.clear();
        batch.extend(self.far.nodes.iter().map(|&index| {
            let slot = &self.slots[index];
            (index, Rect::from_origin_size(slot.pos, slot.size))
        }));
        self.source.paint_far(&batch, self.scale, painter);
        self.scratch_far = batch;
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
    current.counters.hit_curve_scans = content.hit_curve_scans;
    stats.set(current);
}
