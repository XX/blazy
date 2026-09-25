//! What a canvas reports: the answer to a pick, its state, and its work counters.

use masonry::kurbo::Point;

use crate::detail::Detail;
use crate::links::Link;

/// What the canvas found under a point.
///
/// Nodes win over links, at every detail level, because that is the order they are
/// painted in (§25.3): a pointer that disagrees with the picture is worse than one
/// that is imprecise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CanvasHit {
    /// A node, with the canvas-space position of its top-left corner.
    Node {
        /// Index into the node array the canvas was built over.
        index: usize,
        /// Canvas-space position of the node's top-left corner.
        pos: Point,
    },
    /// A link, by its index in the edge list.
    Link {
        /// Index into the edge list given to [`CanvasLayer::with_links`](crate::CanvasLayer::with_links).
        edge: usize,
        /// The edge itself, so the caller need not index the list again.
        link: Link,
    },
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
#[non_exhaustive]
pub struct CanvasStats {
    /// Total number of nodes.
    pub total: usize,
    /// Nodes that currently have a widget in the tree.
    ///
    /// This is the number the passes walk, which is the point: it is bounded by the
    /// viewport, not by the graph. Zero in far-field mode, where nodes are painted
    /// rather than materialised — they are on screen but they are not widgets.
    pub materialised: usize,
    /// Nodes inside the visible rect, whether or not they have a widget.
    ///
    /// The number the budget is decided on, and it is not the same as
    /// [`materialised`](Self::materialised): in the far field every node on screen is
    /// visible and none is a widget. Published because a policy nobody can see the
    /// input of cannot be checked — the criteria in §29.3 are written on this and on
    /// [`CanvasCounters::level_switches`].
    pub visible: usize,
    /// Detail level applied at the last layout.
    ///
    /// The stricter of the two rules that choose it: readability by zoom
    /// ([`DetailThresholds`](crate::DetailThresholds)) and cost by widgets ([`DetailBudget`](crate::DetailBudget)).
    pub detail: Option<Detail>,
    /// Current zoom factor.
    pub zoom: f64,
    /// Nodes in the far-field recording, and therefore drawn on every repaint of it.
    pub recorded_far: usize,
    /// Link curves currently recorded, and therefore drawn on every repaint.
    ///
    /// Bounded by the region the set was chosen for rather than by the viewport, and
    /// the two part company as soon as the view zooms out (§28).
    pub recorded_links: usize,
    /// Link curves the last selection dropped as too short to see.
    ///
    /// The output of [`LinkStyle::min_screen_length`](crate::LinkStyle::min_screen_length), and the only way to tell a
    /// rule that is doing nothing from one that is quietly deleting the graph's structure.
    /// Zero at any zoom a node is still readable at.
    pub hidden_links: usize,
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
#[non_exhaustive]
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
    /// Nodes inserted into or removed from the canvas (§43).
    pub node_edits: u64,
    /// Links inserted into or removed from the canvas.
    pub link_edits: u64,
    /// Times the packed adjacency was rebuilt.
    ///
    /// A structural edit files its link in an overflow list and the packing is redone
    /// when those have grown to a quarter of the graph, so an edit costs what it touched
    /// and the re-packing is amortised. This is the counter that says how often the
    /// second half of that sentence happens.
    pub link_compactions: u64,
    /// Link names walked by structural edits.
    ///
    /// What an edit actually costs in the adjacency, and the counter the claim "a
    /// removal costs its own links, not the graph's" is decided on.
    pub edit_edge_scans: u64,
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
    /// Times the effective detail level actually changed.
    ///
    /// The counter the hysteresis is judged on. A pan near the budget boundary jitters
    /// the visible set by a fifth (§29.1), and a policy without a margin answers every
    /// wobble with a rebuild of every visible node — which is more expensive than the
    /// widgets it saves. Zero while panning at zoom 1–2 is the other half of the
    /// claim: ordinary work must not reach the policy at all.
    pub level_switches: u64,
    /// Times the far-field scene has been re-emitted into the widget's scene.
    ///
    /// Rises whenever the content widget repaints for any reason, so it measures work
    /// done rather than a decision made — the same relation `link_repaints` has to
    /// `link_reselects`. Informational; the decision is
    /// [`far_records`](Self::far_records).
    pub far_repaints: u64,
    /// Times the canvas has re-chosen *which* nodes the far field records.
    ///
    /// This is the one to bound, and it is the counter the overscan trades against
    /// (§35.2): the recorded scene is in canvas coordinates, so panning and zooming
    /// inside the region reuse it untouched and only leaving the region costs a new
    /// selection. A wider margin buys fewer of these with a bigger scene — and the
    /// scene is what the rasteriser is charged for in every frame (§32.3).
    pub far_records: u64,
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
    /// Link curves whose geometry a pick actually rebuilt and measured, summed.
    ///
    /// The expensive half, and the one that must not follow the zoom: a curve counted
    /// here was reconstructed from its endpoints and handed to `near_segment`. What
    /// keeps it small is the box stored with the recorded set — see
    /// [`hit_curve_scans`](Self::hit_curve_scans) for the other half.
    pub hit_curve_tests: u64,
    /// Recorded curves a pick walked past, summed — box tests, not curve tests.
    ///
    /// The candidate set is the links the canvas has recorded, so what can be clicked
    /// is exactly what can be seen; but that set is bounded by the recorded *region*
    /// and not by the viewport, so it grows as the view pulls back (§28). Counted
    /// separately from [`hit_curve_tests`](Self::hit_curve_tests) precisely because the
    /// box filter would otherwise hide it: a counter that only measures the work per
    /// candidate cannot see a defect that lives in the number of candidates, which is
    /// the lesson of §28.4.
    pub hit_curve_scans: u64,
}
