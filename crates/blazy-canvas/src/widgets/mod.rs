//! The two widgets of a canvas, and the machinery they share.
//!
//! [`CanvasLayer`] is in the child module `layer` so that it can reach the content's
//! private fields.

use std::cell::Cell;

use blazy_shape::near_segment;
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AllowRawMut, ChildrenIds, ComposeCtx, LayoutCtx, MeasureCtx, MutateCtx, NoAction, PaintCtx,
    PropertiesRef, RawCtx, RegisterCtx, Widget, WidgetMut, WidgetPod,
};
use masonry::dpi::{LogicalPosition, PhysicalPosition};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, BezPath, Point, Rect, Shape, Size, Stroke};
use masonry::layout::{LenReq, Length};

use crate::detail::{CanvasDetail, Detail, DetailBudget};
use crate::index::SpatialIndex;
use crate::links::{Link, LinkLayer, LinkStyle, PortLayout, PortSide, link_curve, push_link, push_link_ribbon};
use crate::source::NodeSource;
use crate::stats::{CanvasHit, CanvasStats};

mod layer;
#[cfg(test)]
mod tests;

pub use self::layer::CanvasLayer;

/// The canvas's own recording of nodes too small to deserve widgets, in canvas
/// coordinates, so a pan or zoom inside `region` reuses it.
#[derive(Default)]
struct FarField {
    /// Whether the canvas is painting nodes itself instead of materialising them.
    active: bool,
    /// The canvas-space region `nodes` was recorded for. `None` means "needs redoing".
    region: Option<Rect>,
    /// The nodes inside `region`. Meaningless unless `region` is `Some`.
    nodes: Vec<usize>,
    /// Set when the recording must be redone; cleared once a repaint is asked for.
    dirty: bool,
}

/// One node's slot: geometry always, a widget only while it is in view. A removed node
/// leaves a dead slot: names are never renumbered.
struct Slot {
    /// Whether a node is here at all.
    alive: bool,
    /// Position of the node's top-left corner, in canvas coordinates.
    pos: Point,
    /// Size in canvas coordinates. Fixed, so children are never measured.
    size: Size,
    /// The materialised widget, if this node currently has one.
    pod: Option<WidgetPod<dyn Widget>>,
    /// How `pod` was built: its own detail level and the canvas-wide one (as
    /// [`CanvasDetail`]); a change in either makes the widget stale.
    built: Option<(Detail, Detail)>,
    /// Classes the node's widget wears, kept here because the widget comes and goes.
    classes: Vec<&'static str>,
    /// When the node was last raised, from a counter; zero for never. Nodes stack by
    /// `(raised, index)`.
    raised: u64,
}

/// How far past the viewport nodes stay materialised, as a fraction of the viewport.
/// Small: every extra node costs every pass, and a node entering view is built in the same
/// frame anyway.
const DEFAULT_OVERSCAN: f64 = 0.02;
/// How far past the visible region the far-field scene and the link set are recorded.
///
/// A bigger region costs path segments every frame; a smaller one is re-chosen more often
/// during a pan. A quarter was measured as the balance; at 0.10
/// `pan_does_not_reselect_links` fails.
pub(crate) const FAR_OVERSCAN: f64 = 0.25;

/// Zoom changes smaller than this are treated as no change at all.
const ZOOM_EPSILON: f64 = 1e-9;
/// How fast the wheel zooms: the factor is `exp(-dy * WHEEL_ZOOM_RATE)` for a scroll of `dy`
/// logical pixels. Public so that an operator taking the wheel zooms at the same speed.
pub const WHEEL_ZOOM_RATE: f64 = 0.0015;
/// Screen pixels of a far-field link's reach per straight piece of its ribbon.
const FAR_RIBBON_PIECE_PX: f64 = 8.0;
/// The least a link's handles reach, in canvas units — what `link_curve` gives a short
/// link, and what makes a link to the node below loop out and back.
const FAR_RIBBON_HANDLE: f64 = 24.0;
/// How close to a port the pointer has to be to pick it, in screen pixels.
const PORT_SLOP_PX: f64 = 7.0;
/// A wheel notch, in pixels, matching what `Portal` assumes.
const WHEEL_LINE_PX: f64 = 120.0;

/// How far a scroll went, in logical pixels, positive towards the user (lines and pages
/// converted as `Portal` does).
pub fn wheel_pixels(delta: masonry::ui_events::ScrollDelta, scale_factor: f64, viewport: Size) -> f64 {
    let line_px = PhysicalPosition {
        x: WHEEL_LINE_PX * scale_factor,
        y: WHEEL_LINE_PX * scale_factor,
    };
    let page_px = PhysicalPosition {
        x: viewport.width * scale_factor,
        y: viewport.height * scale_factor,
    };
    let LogicalPosition { y, .. } = delta.to_pixel_delta(line_px, page_px).to_logical::<f64>(scale_factor);
    y
}

/// What a state change asks Masonry to redo, so an operation is written once and applied
/// through whichever context the caller holds.
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
    classes: Vec::new(),
    raised: 0,
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

/// Splits two ascending index lists into what left and what arrived, by a merge walk.
///
/// `keep_stale` is asked about indices in both: `true` puts the index in both outputs,
/// which rebuilds the widget in place.
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
fn link_bounds(link: Link, from: &Slot, to: &Slot, ports: &PortLayout) -> Rect {
    let (start, end) = ports.ends(
        link,
        Rect::from_origin_size(from.pos, from.size),
        Rect::from_origin_size(to.pos, to.size),
    );
    link_curve(start, end).bounding_box()
}

/// Whether `outer` fully contains `inner`.
pub(crate) fn contains_rect(outer: Rect, inner: Rect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

/// How far the viewport may zoom in on a recorded region before it is redone: four times
/// the area it started at, relative to the margin rather than to the viewport.
const ZOOM_SLACK: f64 = 4.0;

/// How much larger than the viewport a region recorded with this margin may be before
/// it is redone.
pub(crate) fn region_slack(far_overscan: f64) -> f64 {
    let recorded = (1.0 + 2.0 * far_overscan).powi(2);
    recorded * ZOOM_SLACK
}

/// Whether a region recorded earlier still serves this viewport: it must contain it and
/// not be too much larger, or after zooming out and back in the set never shrinks.
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
    /// Indexed by name, holes included.
    slots: Vec<Slot>,
    /// Live nodes among the slots, counted so that no frame walks the graph.
    alive_nodes: usize,
    /// Builds widgets on demand.
    source: Box<dyn NodeSource>,
    /// Indices with a live widget, ascending; empty in far-field mode.
    live: Vec<usize>,
    /// `live` in stacking order, bottom first: what `children_ids` hands Masonry, which
    /// paints in that order and hit-tests in reverse.
    order: Vec<usize>,
    /// The last stamp handed to [`Slot::raised`].
    raise_seq: u64,
    /// Indices inside the visible rect, ascending; painted directly in far-field mode.
    visible: Vec<usize>,
    /// The far-field recording, used below the [`Detail::Box`] threshold.
    far: FarField,
    /// Edges between nodes, drawn as curves rather than built as widgets.
    links: LinkLayer,
    /// How links are stroked.
    link_style: LinkStyle,
    /// Where the ends of a link are on a node.
    pub(crate) ports: PortLayout,
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
    /// Nodes whose widget was built in the mutate pass that is running, and is not in
    /// the tree until the pass after it registers them.
    ///
    /// A widget that is not in the tree cannot be reached through `get_mut` — Masonry
    /// panics, "child not found" — and a class change can arrive in the same pass that
    /// built it (an editor rebuilt in another window builds its nodes and tells them
    /// what is selected at once). Cleared by the layout that follows registration.
    fresh: Vec<usize>,
    /// The canvas-space region nodes are kept live in: the viewport **plus the
    /// overscan margin**, pushed down by the parent.
    ///
    /// Not the viewport: `region_covers` compares by proportion, and the margin counts.
    live_rect: Rect,
    /// Screen pixels per canvas unit, pushed down by the parent: the zoom only (`LayoutCtx`
    /// has no window transform), which is enough for [`LinkStyle::min_screen_length`].
    scale: f64,
    /// The coarsest level the zoom still makes readable, pushed down by the parent; the
    /// cull combines it with [`DetailBudget`].
    readable: Option<Detail>,
    /// Cost ceiling, pushed down by the parent.
    budget: DetailBudget,
    /// Effective level, decided in `cull` as the stricter of the two rules.
    detail: Option<Detail>,
    layouts: u64,
    child_layouts: u64,
    composes: u64,
    builds: u64,
    class_changes: u64,
    /// Nodes raised to the top of the stacking order.
    raises: u64,
    hit_port_tests: u64,
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

// `CanvasLayer` owns this widget completely and reaches into it during layout: the case
// Masonry documents the escape hatch for.
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
            order: Vec::new(),
            raise_seq: 0,
            visible: Vec::new(),
            far: FarField::default(),
            links: LinkLayer::default(),
            link_style: LinkStyle::default(),
            ports: PortLayout::default(),
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
            class_changes: 0,
            raises: 0,
            hit_port_tests: 0,
            fresh: Vec::new(),
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

    /// Puts a node into the canvas under the name `index`, which the model chose; a name
    /// beyond the end leaves holes.
    ///
    /// A name already alive is the same insertion delivered again (push, then pull): only
    /// its geometry is taken, and the widget stays.
    fn insert_node(&mut self, index: usize, pos: Point, size: Size) -> Invalidate {
        if self.live_slot(index).is_some() {
            return self.store_child_rect(index, Rect::from_origin_size(pos, size));
        }
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || EMPTY_SLOT);
        }
        self.alive_nodes += 1;
        self.slots[index] = Slot {
            alive: true,
            pos,
            size,
            pod: None,
            built: None,
            // A reused name is a new node.
            classes: Vec::new(),
            raised: 0,
        };
        self.index.insert(index, Rect::from_origin_size(pos, size));
        self.node_edits += 1;
        self.structure_changed();
        Invalidate::LayoutAndPaint
    }

    /// Takes the node named `index` out and returns its links. Its widget goes in the next
    /// mutate pass.
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

    // --- MARK: STACKING

    /// Where node `index` stacks: higher is drawn later and picked first.
    fn stacking(&self, index: usize) -> (u64, usize) {
        (self.slots[index].raised, index)
    }

    /// Orders pick candidates topmost first.
    fn sort_topmost_first(&self, candidates: &mut [usize]) {
        if self.raise_seq == 0 {
            candidates.reverse();
        } else {
            candidates.sort_unstable_by_key(|&index| std::cmp::Reverse(self.stacking(index)));
        }
    }

    /// Rebuilds `order` from `live`. Bounded by the widget budget, not by the graph.
    fn restack(&mut self) {
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend_from_slice(&self.live);
        if self.raise_seq > 0 {
            order.sort_unstable_by_key(|&index| self.stacking(index));
        }
        self.order = order;
    }

    /// Puts node `index` above every other node of this canvas, and says whether its place
    /// changed.
    fn raise_node(&mut self, index: usize) -> bool {
        if self.raise_seq > 0 && self.live_slot(index).is_some_and(|slot| slot.raised == self.raise_seq) {
            return false;
        }
        self.stack_node(index, self.raise_seq + 1)
    }

    /// Gives node `index` the stacking stamp `stamp`, and says whether its place changed.
    /// Idempotent, so a change delivered twice is harmless.
    fn stack_node(&mut self, index: usize, stamp: u64) -> bool {
        if self.live_slot(index).is_none_or(|slot| slot.raised == stamp) {
            return false;
        }
        self.slots[index].raised = stamp;
        if stamp >= self.raise_seq {
            self.raise_seq = stamp;
            if let Some(at) = self.order.iter().position(|&i| i == index) {
                self.order.remove(at);
                self.order.push(index);
            }
        } else {
            // A stamp older than the newest: the node goes somewhere in the middle.
            self.restack();
        }
        self.raises += 1;
        true
    }

    /// What every structural edit has to invalidate: the recorded sets are chosen for a
    /// region, and an edit does not move the view.
    fn structure_changed(&mut self) {
        self.far.region = None;
        self.far.dirty = true;
        self.links.invalidate();
    }

    // --- MARK: HIT TESTING

    /// What is under a canvas-space point, from the model (far-field nodes and links have no
    /// widgets). `scale` is screen pixels per canvas unit, for the tolerances.
    fn hit(&mut self, canvas_pos: Point, scale: f64) -> Option<CanvasHit> {
        self.hit_queries += 1;
        // Ports before nodes: a port sits half inside its node.
        self.hit_port(canvas_pos, scale)
            .or_else(|| self.hit_node(canvas_pos))
            .or_else(|| self.hit_link(canvas_pos, scale))
    }

    /// The port under a point, or `None`. Only nodes near the point are asked for their
    /// ports; the tolerance is in screen pixels, at least the drawn dot.
    fn hit_port(&mut self, canvas_pos: Point, scale: f64) -> Option<CanvasHit> {
        // Not in the far field, where ports are not drawn.
        if self.far.active {
            return None;
        }
        let slop = if scale > f64::EPSILON {
            PORT_SLOP_PX / scale
        } else {
            0.0
        };
        let radius = slop.max(self.ports.dot_radius);
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(
            Rect::from_points(canvas_pos, canvas_pos).inflate(radius, radius),
            &mut candidates,
        );
        self.sort_topmost_first(&mut candidates);
        let mut found = None;
        'nodes: for &index in &candidates {
            let Some(rect) = self.live_rect_of(index) else {
                continue;
            };
            // Only a node with a widget has ports to grab.
            if self.slots[index].pod.is_none() || !rect.inflate(radius, radius).contains(canvas_pos) {
                continue;
            }
            let ports = self.source.ports(index);
            for (side, count) in [(PortSide::Output, ports.outputs), (PortSide::Input, ports.inputs)] {
                for port in 0..count {
                    self.hit_port_tests += 1;
                    let at = self.ports.position(rect, side, port);
                    if at.distance(canvas_pos) <= radius {
                        found = Some(CanvasHit::Port {
                            index,
                            side,
                            port,
                            pos: at,
                        });
                        break 'nodes;
                    }
                }
            }
            // A node covering the point (by its shape) hides the ports stacked under it.
            if rect.contains(canvas_pos) && self.source.hit(index, rect, canvas_pos) {
                break;
            }
        }
        self.scratch_candidates = candidates;
        found
    }

    /// The topmost node under a point, or `None`.
    fn hit_node(&mut self, canvas_pos: Point) -> Option<CanvasHit> {
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        // A point: the grid widens the query by its slack.
        self.index
            .candidates(Rect::from_points(canvas_pos, canvas_pos), &mut candidates);
        self.hit_node_tests += candidates.len() as u64;

        // Topmost first.
        self.sort_topmost_first(&mut candidates);
        let mut found = None;
        for &index in &candidates {
            let Some(rect) = self.live_rect_of(index) else {
                continue;
            };
            // The rectangle first, the exact shape only for what survives.
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

    /// A link under a point, or `None`, among the links recorded for this viewport: what
    /// can be picked is exactly what is drawn.
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
        // Any of the links under the pointer: curves are stroked batched by style, so
        // drawing order is undefined.
        for (at, &edge) in self.links.recorded().iter().enumerate() {
            scanned += 1;
            // The stored box first; an unmeasured box answers `None` and the curve is tested.
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
            let (start, end) = self.ports.ends(link, from, to);
            let curve = link_curve(start, end);
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

    /// Records what the pointer is over, and returns whether the canvas has to repaint.
    fn set_hovered(&mut self, hit: Option<CanvasHit>) -> bool {
        if self.hovered == hit {
            return false;
        }
        let was = self.hovered.and_then(CanvasHit::link);
        self.hovered = hit;
        was.is_some() || hit.and_then(CanvasHit::link).is_some()
    }

    /// The detail level node `index` should be built at: the canvas-wide one, or with
    /// [`CanvasLayer::with_controls_on_hover`] full controls only under the pointer.
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

    /// Takes every node widget out and starts over with `slots` and `links`.
    fn reset(this: &mut WidgetMut<'_, Self>, slots: Vec<Slot>, links: Vec<Link>) {
        for slot in &mut this.widget.slots {
            if let Some(pod) = slot.pod.take() {
                this.ctx.remove_child(pod);
            }
        }
        let content = &mut *this.widget;
        content.index = SpatialIndex::build(slots.iter().map(|s| Rect::from_origin_size(s.pos, s.size)));
        content.alive_nodes = slots.iter().filter(|slot| slot.alive).count();
        content.links = LinkLayer::new(links, slots.len());
        content.slots = slots;
        content.live.clear();
        content.order.clear();
        content.fresh.clear();
        content.active = None;
        content.hovered = None;
        content.pending = None;
        content.pending_stale = false;
        content.structure_changed();
        this.ctx.children_changed();
        this.ctx.request_layout();
        this.ctx.request_paint_only();
    }

    /// Builds and drops children to match the last cull, in the mutate pass (the one where
    /// children may change), so a node entering the view shows in the same frame.
    fn apply_pending(this: &mut WidgetMut<'_, Self>) {
        let Some(desired) = this.widget.pending.take() else {
            return;
        };
        let stale = std::mem::take(&mut this.widget.pending_stale);

        // Staleness is checked only when a detail level changed, never during a pan.
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

        // Removed, not stashed: a stashed widget is still walked by every pass.
        let changed = !removed.is_empty() || !added.is_empty();
        for &index in &removed {
            if let Some(pod) = this.widget.slots[index].pod.take() {
                this.widget.slots[index].built = None;
                this.ctx.remove_child(pod);
            }
        }

        for &index in &added {
            let (detail, global) = this.widget.build_spec(index);
            let mut widget = this.widget.source.build(index, detail).with_props(CanvasDetail(global));
            for class in &this.widget.slots[index].classes {
                widget = widget.with_class(class);
            }
            this.widget.slots[index].pod = Some(widget.to_pod());
            this.widget.fresh.push(index);
            this.widget.slots[index].built = Some((detail, global));
            this.widget.builds += 1;
        }

        this.widget.scratch_removed = removed;
        this.widget.scratch_added = added;
        // The old buffer goes back to the pool.
        this.widget.scratch_desired = std::mem::replace(&mut this.widget.live, desired);
        if changed {
            this.widget.restack();
            this.ctx.children_changed();
            this.ctx.request_layout();
        }
        // The parent requests the far-field repaint.
    }

    /// Puts `class` on node `index` or takes it off, and says whether that changed anything.
    /// The slot remembers it for widgets built later.
    fn set_node_class(this: &mut WidgetMut<'_, Self>, index: usize, class: &'static str, on: bool) -> bool {
        let Some(slot) = this.widget.slots.get_mut(index).filter(|slot| slot.alive) else {
            return false;
        };
        let wears = slot.classes.contains(&class);
        if wears == on {
            return false;
        }
        if on {
            slot.classes.push(class);
        } else {
            slot.classes.retain(|&worn| worn != class);
        }
        this.widget.class_changes += 1;
        let fresh = this.widget.fresh.contains(&index);
        if let Some(pod) = this.widget.slots[index].pod.as_mut() {
            let apply = move |mut widget: WidgetMut<'_, dyn Widget>| {
                if on {
                    widget.ctx.add_class(class);
                } else {
                    widget.ctx.remove_class(class);
                }
            };
            if fresh {
                // Built in this pass and not in the tree yet; built wearing the slot's classes.
                let id = pod.id();
                this.ctx.mutate_later(id, apply);
            } else {
                apply(this.ctx.get_mut(pod));
            }
        }
        true
    }

    /// Marks node `index`'s widget stale, so the next cull rebuilds it from the source, and
    /// re-records the far field if that draws the node. For a change to what a node holds;
    /// two changes in one pull rebuild once.
    fn refresh_node(&mut self, index: usize) -> Invalidate {
        if self.live_slot(index).is_none() {
            return Invalidate::Nothing;
        }
        if self.far.active {
            // Drawn by this widget's own scene.
            self.far.region = None;
            return Invalidate::LayoutAndPaint;
        }
        if self.slots[index].pod.is_none() {
            // No widget: the node reads the model when it is next built.
            return Invalidate::Nothing;
        }
        self.slots[index].built = None;
        self.detail_dirty = true;
        Invalidate::Layout
    }

    /// Records a new rectangle for a node: re-filed in the grid and its curves re-measured,
    /// without re-choosing the recorded set.
    fn store_child_rect(&mut self, index: usize, rect: Rect) -> Invalidate {
        let Some(slot) = self.live_slot(index) else {
            return Invalidate::Nothing;
        };
        if slot.size == rect.size() {
            return self.store_child_pos(index, rect.origin());
        }
        let slot = &mut self.slots[index];
        slot.pos = rect.origin();
        slot.size = rect.size();
        self.index.insert(index, rect);
        let slots = &self.slots;
        let ports = &self.ports;
        self.links.node_moved(index, |link| {
            match (slots.get(link.from as usize), slots.get(link.to as usize)) {
                (Some(from), Some(to)) if from.alive && to.alive => link_bounds(link, from, to, ports),
                _ => Rect::ZERO,
            }
        });
        if self.far.active {
            self.far.region = None;
        }
        Invalidate::LayoutAndPaint
    }

    /// Records a new canvas-space position for a node (in far-field mode, a re-recording).
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
        let ports = &self.ports;
        self.links.node_moved(index, |link| {
            match (slots.get(link.from as usize), slots.get(link.to as usize)) {
                (Some(from), Some(to)) if from.alive && to.alive => link_bounds(link, from, to, ports),
                _ => Rect::ZERO,
            }
        });
        if far_field {
            self.far.region = None;
            Invalidate::LayoutAndPaint
        } else {
            // Only this widget is dirtied; the nodes keep their cached scenes.
            Invalidate::Layout
        }
    }

    /// Computes the set of nodes inside the visible rect, from the grid's candidates.
    fn cull(&mut self) {
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(self.live_rect, &mut candidates);

        // The last frame's buffer, emptied.
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

        // The level is decided here: the budget rule needs the number of visible nodes. The
        // coarser of the two rules wins (`Detail` is ordered finest first, so `max`).
        let readable = self.readable.unwrap_or(Detail::Full);
        let affordable = self.budget.level_for(visible.len(), self.detail);
        let level = readable.max(affordable);
        if self.detail != Some(level) {
            // The first layout is not a switch.
            if self.detail.is_some() {
                self.level_switches += 1;
            }
            self.detail = Some(level);
            self.detail_dirty = true;
        }

        // Below the box threshold the canvas paints the nodes itself: nothing is built.
        let far_field = level == Detail::Box;
        // Reuse the unapplied `pending` buffer, or the pool's.
        let mut desired = self
            .pending
            .take()
            .unwrap_or_else(|| std::mem::take(&mut self.scratch_desired));
        desired.clear();
        if !far_field {
            desired.extend_from_slice(&visible);
        }

        self.visible = visible;

        // A widget at the wrong detail level is replaced: levels differ in their children.
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
            // Nothing to apply: the buffer goes back to the pool.
            self.scratch_desired = desired;
            self.pending = None;
        }
    }

    /// Re-chooses which links are recorded when the viewport leaves their region (the far
    /// field's margin).
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

    /// Measures the recorded curves and drops the ones too short to be seen, when the set is
    /// re-chosen: the picture and the pick read the same set. Keeps each curve's box for the
    /// pick.
    fn measure_links(&mut self) {
        // Screen pixels into canvas units. A zero threshold or degenerate scale drops none.
        let min_screen = self.link_style.min_screen_length;
        let min_canvas = if min_screen > 0.0 && self.scale > f64::EPSILON {
            min_screen / self.scale
        } else {
            0.0
        };
        let slots = &self.slots;
        let ports = &self.ports;
        self.links.measure_recorded(|link| {
            let (Some(from), Some(to)) = (
                slots.get(link.from as usize).filter(|s| s.alive),
                slots.get(link.to as usize).filter(|s| s.alive),
            ) else {
                // An edge to a missing node: skipped when drawn, its empty box never picked.
                return Some(Rect::ZERO);
            };
            let bounds = link_bounds(link, from, to, ports);
            // The box's diagonal, not the chord: a link that loops out is visible.
            let size = bounds.size();
            (size.width.hypot(size.height) >= min_canvas).then_some(bounds)
        });
    }

    /// Re-records the far-field node set when the viewport leaves the recorded region.
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
        // Node sizes come from the model: children are never measured.
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::ZERO,
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {
        self.layouts += 1;
        // Whatever was built before this layout is in the tree by now.
        self.fresh.clear();

        // Children are laid out in canvas coordinates; the zoom is only this widget's
        // transform, or every zoom step would relayout the graph.
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
        // Links under the nodes, at every detail level.
        if !self.links.is_empty() {
            self.link_repaints += 1;
            let hovered = self.hovered.and_then(CanvasHit::link);

            // One path per style, one command per path (a command per link costs 15x).
            let mut plain = std::mem::take(&mut self.scratch_links);
            let mut hot = std::mem::take(&mut self.scratch_hot_links);
            plain.truncate(0);
            hot.truncate(0);
            // Far field: links filled, never thinner on screen than a floor.
            let far = self.far.active;
            let fill = far && self.link_style.far_fill;
            let floor = if far && self.scale > f64::EPSILON {
                self.link_style.far_min_width_px / self.scale
            } else {
                0.0
            };
            let width = self.link_style.width.max(floor);
            let stroke = Stroke::new(width);
            let mut filled = BezPath::new();
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
                let (start, end) = self.ports.ends(link, from, to);
                if fill && hovered != Some(edge as usize) {
                    // A piece per eight screen pixels of reach.
                    let reach = (end - start).hypot() + 2.0 * FAR_RIBBON_HANDLE;
                    let pieces = ((reach * self.scale / FAR_RIBBON_PIECE_PX).ceil() as usize).clamp(1, 8);
                    push_link_ribbon(&mut filled, start, end, width / 2.0, pieces);
                } else {
                    push_link(path, start, end);
                }
            }
            if !filled.is_empty() {
                painter.fill(&filled, self.link_style.color).draw();
            }
            if !plain.is_empty() {
                painter.stroke(&plain, &stroke, self.link_style.color).draw();
            }
            // Last, so the highlighted link is on top.
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
        // The whole set in one call, so the application can group its fills: a frame costs
        // commands, not their content.
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
        self.order
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

/// Copies what a pick just learned into the published statistics (a pick runs no layout,
/// where the rest are refreshed).
fn publish_hit_stats(stats: &Cell<CanvasStats>, content: &CanvasContent) {
    let mut current = stats.get();
    current.hovered = content.hovered;
    current.counters.hit_queries = content.hit_queries;
    current.counters.hit_node_tests = content.hit_node_tests;
    current.counters.hit_port_tests = content.hit_port_tests;
    current.counters.hit_curve_tests = content.hit_curve_tests;
    current.counters.hit_curve_scans = content.hit_curve_scans;
    stats.set(current);
}
