//! The link layer: edges between nodes, drawn as curves rather than built as widgets.
//!
//! `rnd/architecture.md` §10.4 settles the shape: not widgets, but a separate layer of
//! cubic béziers batched into one stroke stream and recorded in a scene of its own,
//! re-recorded only when the topology or an endpoint moves.
//!
//! The reason it cannot be widgets is the reason Phase 0 exists. Virtualisation
//! bounded the cost of nodes by removing the off-screen ones from the widget tree
//! (§20.3). Links cannot leave the tree the same way: there are as many of them as
//! there are nodes, and one with a single endpoint on screen still has to be drawn.
//! A link that is a widget is a link that puts the linear cost back.
//!
//! # Which links get drawn
//!
//! Selected through the adjacency list rather than through a spatial index of their
//! own: a link is recorded when **either endpoint** lies in the recorded region. That
//! is exact for a graph whose edges are shorter than the region margin — half a
//! viewport — which is what a node editor is, and it makes a drag cheap, because the
//! links a moved node disturbs are exactly its own.
//!
//! It is not exact in general. A link whose two endpoints both lie outside the region
//! while the curve between them crosses the screen is not drawn. Indexing link
//! bounding boxes would fix it, and would cost a second index whose cells degenerate
//! as soon as one edge is long. The limitation is pinned by a test rather than left
//! to be discovered.

use masonry::kurbo::{BezPath, CubicBez, Point, Rect};

/// A connection between two nodes, by index into the canvas's node array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub from: u32,
    pub to: u32,
}

impl Link {
    pub fn new(from: usize, to: usize) -> Self {
        Self {
            from: from as u32,
            to: to as u32,
        }
    }
}

/// Topology, plus which of it is currently recorded.
#[derive(Default)]
pub(crate) struct LinkLayer {
    edges: Vec<Link>,
    /// Edge indices incident to each node.
    adjacency: Vec<Vec<u32>>,
    /// The region the recorded set was chosen for, in canvas coordinates.
    region: Option<Rect>,
    /// Edges in the recorded scene, ascending and without duplicates.
    recorded: Vec<u32>,
    /// Set when the recorded scene no longer matches the curves and must be redrawn.
    ///
    /// Distinct from [`reselect`](Self::reselect) on purpose: a node being dragged
    /// moves a curve without changing *which* curves are on screen, and conflating
    /// the two would re-choose the whole set on every frame of a drag.
    repaint: bool,
    /// Set when the recorded *set* is no longer the right one.
    reselect: bool,
    /// Times the set has been re-chosen.
    refreshes: u64,
}

impl LinkLayer {
    pub(crate) fn new(edges: Vec<Link>, node_count: usize) -> Self {
        let mut adjacency = vec![Vec::new(); node_count];
        for (i, link) in edges.iter().enumerate() {
            for end in [link.from, link.to] {
                if let Some(list) = adjacency.get_mut(end as usize) {
                    list.push(i as u32);
                }
            }
        }
        Self {
            edges,
            adjacency,
            region: None,
            recorded: Vec::new(),
            repaint: false,
            reselect: false,
            refreshes: 0,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    pub(crate) fn recorded(&self) -> &[u32] {
        &self.recorded
    }

    pub(crate) fn edge(&self, index: u32) -> Link {
        self.edges[index as usize]
    }

    pub(crate) fn refreshes(&self) -> u64 {
        self.refreshes
    }

    /// Takes the "the scene must be redrawn" flag.
    pub(crate) fn take_repaint(&mut self) -> bool {
        std::mem::take(&mut self.repaint)
    }

    /// Marks the layer for re-recording because a node moved.
    ///
    /// Only if the node has links at all, and only if some of them are on screen: a
    /// drag in an empty corner of the graph should cost nothing.
    pub(crate) fn node_moved(&mut self, node: usize) {
        if self.repaint || self.edges.is_empty() {
            return;
        }
        let Some(incident) = self.adjacency.get(node) else {
            return;
        };
        if incident.iter().any(|edge| self.recorded.binary_search(edge).is_ok()) {
            self.repaint = true;
        }
    }

    /// Whether the recorded set has to be re-chosen for this viewport.
    pub(crate) fn needs_reselect(&self, visible_rect: Rect) -> bool {
        !self.edges.is_empty()
            && (self.reselect
                || !self
                    .region
                    .is_some_and(|region| crate::region_covers(region, visible_rect)))
    }

    /// Re-chooses the recorded set if [`needs_reselect`](Self::needs_reselect) says so.
    /// `nodes` are the node indices inside `region`.
    ///
    /// Returns whether the set changed. The caller checks `needs_reselect` first to
    /// avoid the index query it would need to produce `nodes` at all; this re-checks
    /// rather than trusting it, because the two are far apart in the source and the
    /// cost of asking again is a rectangle comparison.
    pub(crate) fn refresh(&mut self, region: Rect, visible_rect: Rect, nodes: &[usize]) -> bool {
        if !self.needs_reselect(visible_rect) {
            return false;
        }
        self.reselect = false;

        self.recorded.clear();
        for &node in nodes {
            if let Some(incident) = self.adjacency.get(node) {
                self.recorded.extend_from_slice(incident);
            }
        }
        // A link with both endpoints in the region is reached from each of them.
        self.recorded.sort_unstable();
        self.recorded.dedup();

        self.region = Some(region);
        self.refreshes += 1;
        self.repaint = true;
        true
    }

    /// Drops the recorded set, so the next refresh rebuilds it.
    pub(crate) fn invalidate(&mut self) {
        self.region = None;
        self.reselect = true;
    }
}

/// The curve for one link, from the right edge of `from` to the left edge of `to`.
///
/// A cubic with horizontal handles, which is what every node editor draws and what
/// makes two links between the same pair of columns distinguishable. Ports are the
/// midpoints of the facing edges: real ports are a node's business, and the canvas
/// does not know how many a node has.
///
/// The curve, not the path, is what both callers actually want: painting strokes it
/// and hit testing measures the distance to it. One function so that the two can
/// never disagree about where a link is — a pointer that picks a curve the eye does
/// not see there is worse than one that misses.
pub(crate) fn link_curve(from: Rect, to: Rect) -> CubicBez {
    let start = Point::new(from.x1, from.center().y);
    let end = Point::new(to.x0, to.center().y);
    // Handles scale with the gap so a short link does not loop and a long one does
    // not go slack.
    let reach = ((end.x - start.x).abs() * 0.5).max(24.0);

    CubicBez::new(
        start,
        Point::new(start.x + reach, start.y),
        Point::new(end.x - reach, end.y),
        end,
    )
}

/// The same curve as a path, for stroking.
pub(crate) fn link_path(from: Rect, to: Rect) -> BezPath {
    let mut path = BezPath::new();
    let curve = link_curve(from, to);
    path.move_to(curve.p0);
    path.curve_to(curve.p1, curve.p2, curve.p3);
    path
}

#[cfg(test)]
mod tests {
    use masonry::kurbo::Size;

    use super::*;

    fn rect(x: f64, y: f64) -> Rect {
        Rect::from_origin_size(Point::new(x, y), Size::new(100.0, 50.0))
    }

    /// A chain of five nodes, each linked to the next.
    fn chain() -> LinkLayer {
        LinkLayer::new((0..4).map(|i| Link::new(i, i + 1)).collect(), 5)
    }

    #[test]
    fn a_link_is_recorded_from_either_end() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);

        // Only node 2 is in the region; both of its links come along.
        layer.refresh(region, region, &[2]);
        assert_eq!(layer.recorded(), &[1, 2]);
    }

    #[test]
    fn a_link_reachable_from_both_ends_is_recorded_once() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);
        layer.refresh(region, region, &[1, 2]);
        assert_eq!(layer.recorded(), &[0, 1, 2]);
    }

    /// The documented limitation, pinned so it is a decision rather than a surprise.
    #[test]
    fn a_link_with_both_ends_outside_the_region_is_not_recorded() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);
        layer.refresh(region, region, &[]);
        assert!(layer.recorded().is_empty(), "nothing in the region, nothing drawn");
    }

    /// Dragging a node must redraw the curves without re-choosing which curves are on
    /// screen: at 60 frames a second, the difference is between touching one node's
    /// links and re-walking every node in the region.
    #[test]
    fn a_drag_repaints_without_reselecting() {
        let mut layer = chain();
        // The proportions a viewport really produces: the region is the visible rect
        // plus half of it on each side.
        let visible = Rect::new(0.0, 0.0, 100.0, 100.0);
        let region = visible.inflate(50.0, 50.0);
        layer.refresh(region, visible, &[0, 1, 2]);
        layer.take_repaint();
        let refreshes = layer.refreshes();

        for _ in 0..10 {
            layer.node_moved(1);
            assert!(layer.take_repaint());
            assert!(!layer.refresh(region, visible, &[0, 1, 2]));
        }
        assert_eq!(layer.refreshes(), refreshes, "a drag re-chose the recorded set");
    }

    /// Panning inside the region must not re-choose the set — the same trick the far
    /// field uses, and the reason a link scene can be reused across frames.
    #[test]
    fn staying_inside_the_region_does_not_refresh() {
        let mut layer = chain();
        let visible = Rect::new(0.0, 0.0, 100.0, 100.0);
        let region = visible.inflate(50.0, 50.0);
        layer.refresh(region, visible, &[0, 1]);
        layer.take_repaint();
        let after_first = layer.refreshes();

        assert!(!layer.refresh(region, Rect::new(10.0, 10.0, 110.0, 110.0), &[0, 1]));
        assert_eq!(layer.refreshes(), after_first);
    }

    /// A region chosen while zoomed out must not outlive the zoom that produced it.
    ///
    /// It contains every viewport that follows, so containment alone would keep it
    /// forever — and with it every edge it recorded. This is §28, and it is what made
    /// a canvas stay slow after one look at the whole graph.
    #[test]
    fn zooming_back_in_re_chooses_the_set() {
        let mut layer = chain();
        let wide = Rect::new(-5000.0, -5000.0, 5000.0, 5000.0);
        layer.refresh(wide, wide, &[0, 1, 2]);
        layer.take_repaint();
        assert_eq!(layer.recorded().len(), 3, "the whole graph is recorded");

        let close = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(
            layer.needs_reselect(close),
            "a region ten thousand times the viewport does not serve it"
        );
        assert!(layer.refresh(close.inflate(50.0, 50.0), close, &[0]));
        assert_eq!(layer.recorded().len(), 1, "and the set shrank with the viewport");
    }

    #[test]
    fn leaving_the_region_refreshes() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);
        layer.refresh(region, region, &[0]);
        assert!(layer.refresh(region, Rect::new(500.0, 500.0, 600.0, 600.0), &[3]));
        assert_eq!(layer.recorded(), &[2, 3]);
    }

    /// Dragging a node with links on screen has to redraw them, and dragging one
    /// without has to cost nothing.
    #[test]
    fn only_a_node_with_recorded_links_dirties_the_layer() {
        let mut layer = chain();
        let region = Rect::new(0.0, 0.0, 100.0, 100.0);
        layer.refresh(region, region, &[0]);
        layer.take_repaint();

        layer.node_moved(4);
        assert!(!layer.take_repaint(), "node 4's link is not recorded");

        layer.node_moved(0);
        assert!(layer.take_repaint(), "node 0's link is");
    }

    #[test]
    fn a_graph_without_links_never_refreshes() {
        let mut layer = LinkLayer::new(Vec::new(), 4);
        assert!(layer.is_empty());
        assert!(!layer.refresh(Rect::ZERO, Rect::ZERO, &[0, 1, 2, 3]));
        layer.node_moved(0);
        assert!(!layer.take_repaint());
    }

    #[test]
    fn the_curve_starts_and_ends_on_the_facing_edges() {
        let path = link_path(rect(0.0, 0.0), rect(300.0, 100.0));
        let start = path.elements()[0];
        assert!(
            matches!(start, masonry::kurbo::PathEl::MoveTo(p) if p == Point::new(100.0, 25.0)),
            "{start:?}"
        );
    }
}
