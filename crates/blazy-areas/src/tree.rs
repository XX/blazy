//! The split tree: which area sits where, and nothing at all about widgets.
//!
//! Kept free of Masonry on purpose. `rnd/architecture.md` §8 calls areas "a
//! subsystem over the widget tree, not another widget", and the split tree is the
//! part of that claim which has to be true first: it is the piece that will later
//! be serialised into a workspace file, diffed, and possibly swapped for a
//! vertex-and-edge graph if edge alignment ever becomes worth its complexity.
//! Anything that reaches for a `WidgetId` here would make all three harder.

use masonry::kurbo::{Axis, Point, Rect};

/// Index of a node in the tree.
///
/// Not stable across operations and not written to a workspace file: a [`Bar`] carries
/// one so a drag can name the split it is moving, and it is re-read from the layout that
/// produced it. [`AreaId`] is the identifier that keeps its meaning.
pub type NodeId = usize;

/// Index of an area. Stable for the life of the tree.
///
/// Nothing renumbers one: a join frees the id of the area that went, a later split hands
/// that id out again, and every area that stayed keeps the id it had. That is what lets a
/// caller key its own state by [`AreaId`] — `AreaScreen` keys the widgets by it, and a
/// workspace file names areas by it.
pub type AreaId = usize;

/// The most areas one tree holds, and with it the bound on every [`AreaId`].
///
/// A policy rather than a measurement: a screen holds tens of areas, not thousands. It
/// exists because an id is a size somebody allocates — a caller keys its own arrays by
/// id, and `AreaScreen` does — so without a bound one line of a hand-edited workspace
/// file, `area 4000000000`, is a multi-gigabyte allocation instead of an error.
///
/// One bound, enforced in both directions: [`SplitTree::split`] refuses to grow a tree
/// past it and the workspace reader refuses a file that goes past it, so whatever a tree
/// can become, a file can bring back. That rests on ids being reused before fresh ones
/// are handed out — a tree holding fewer than `MAX_AREAS` areas never hands out an id at
/// or above it.
pub const MAX_AREAS: usize = 1024;

#[derive(Clone, Copy, Debug)]
enum Node {
    /// Two children laid out along `axis`, `ratio` of the usable space to the first.
    Split {
        axis: Axis,
        ratio: f64,
        a: NodeId,
        b: NodeId,
    },
    Area(AreaId),
    /// A slot whose node has left the tree.
    ///
    /// Kept rather than removed because every other node names its children by index:
    /// compacting `nodes` would move ids that are still referenced, and a tree of tens
    /// of nodes has nothing to gain from it.
    Free,
}

/// One splitter, as laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    /// The split this bar divides. Pass it to [`SplitTree::set_ratio`].
    pub split: NodeId,
    /// The axis the split divides along.
    pub axis: Axis,
    /// The bar itself, for hit testing and painting.
    pub rect: Rect,
    /// The whole rect the split divides.
    ///
    /// A drag turns a pointer position into a ratio against this rect, so it has to
    /// travel with the bar: by the time the pointer moves, the recursion that knew
    /// the parent rect is long gone.
    pub span: Rect,
}

/// A binary tree of splits with an area at every leaf.
///
/// Binary rather than the vertex-and-edge graph Blender uses. The graph exists so
/// that resizing aligns the borders of areas which are not in a parent/child
/// relation; the tree cannot do that, and is an order of magnitude simpler. §8's
/// recommendation is to start here and keep the operations behind this type, so
/// that swapping the representation later touches nothing else.
#[derive(Clone, Debug)]
pub struct SplitTree {
    nodes: Vec<Node>,
    root: NodeId,
    /// Areas currently in the tree.
    areas: usize,
    /// Node slots a join emptied, for the next split to fill.
    free_nodes: Vec<NodeId>,
    /// Area ids a join freed, for the next split to hand out again.
    ///
    /// Reused rather than retired, because an id is an index into the caller's own
    /// arrays: retiring them means those arrays grow with the number of joins a session
    /// has seen rather than with the number of areas it holds.
    free_areas: Vec<AreaId>,
    /// The next never-used area id.
    next_area: AreaId,
    /// The area shown alone, if the screen is maximized.
    ///
    /// A flag on the tree rather than a saved copy of it (§41.3): the tree underneath is
    /// untouched, so restoring gives back exactly the rectangles that were there — a
    /// property a test can check bit for bit, which a rebuilt tree could only approximate.
    maximized: Option<AreaId>,
}

impl SplitTree {
    /// A screen holding one area.
    pub fn single() -> Self {
        Self {
            nodes: vec![Node::Area(0)],
            root: 0,
            areas: 1,
            free_nodes: Vec::new(),
            free_areas: Vec::new(),
            next_area: 1,
            maximized: None,
        }
    }

    /// A screen tiled into `areas` roughly equal parts.
    ///
    /// Splits alternate axis by depth, so the result stays close to square instead
    /// of degenerating into stripes. Deterministic, because it is what the
    /// measurements sweep over and a benchmark that tiles differently between runs
    /// measures nothing.
    ///
    /// # Panics
    ///
    /// Panics if `areas` is zero, because a screen with no area has no meaning, or above
    /// [`MAX_AREAS`], which no tree may hold.
    pub fn balanced(areas: usize) -> Self {
        assert!(areas > 0, "a screen needs at least one area");
        assert!(areas <= MAX_AREAS, "a screen holds at most {MAX_AREAS} areas");
        let mut tree = Self {
            nodes: Vec::new(),
            root: 0,
            areas,
            free_nodes: Vec::new(),
            free_areas: Vec::new(),
            next_area: areas,
            maximized: None,
        };
        let mut next = 0;
        tree.root = tree.build_balanced(areas, 0, &mut next);
        tree
    }

    fn build_balanced(&mut self, leaves: usize, depth: usize, next: &mut AreaId) -> NodeId {
        if leaves == 1 {
            let area = *next;
            *next += 1;
            self.nodes.push(Node::Area(area));
            return self.nodes.len() - 1;
        }
        let first = leaves / 2;
        let a = self.build_balanced(first, depth + 1, next);
        let b = self.build_balanced(leaves - first, depth + 1, next);
        let axis = if depth.is_multiple_of(2) {
            Axis::Horizontal
        } else {
            Axis::Vertical
        };
        self.nodes.push(Node::Split {
            axis,
            ratio: first as f64 / leaves as f64,
            a,
            b,
        });
        self.nodes.len() - 1
    }

    /// How many areas the screen holds.
    pub fn area_count(&self) -> usize {
        self.areas
    }

    /// The areas the tree holds, in tree order rather than by id.
    ///
    /// Ids are not dense once anything has been joined, so this is the only honest way
    /// to ask what is on the screen.
    pub fn areas(&self) -> impl Iterator<Item = AreaId> + '_ {
        self.nodes.iter().filter_map(|node| match node {
            Node::Area(area) => Some(*area),
            _ => None,
        })
    }

    /// Whether `area` is in the tree.
    pub fn holds(&self, area: AreaId) -> bool {
        self.node_of(area).is_some()
    }

    /// Splits `area` in two, returning the id of the area that appears.
    ///
    /// The existing area keeps its id and the first `ratio` of the space, which is
    /// what makes a split non-destructive: whatever widget the caller has already
    /// built for `area` stays valid and stays where it was.
    ///
    /// The new id is one a join freed, if there is one, and a fresh one otherwise.
    ///
    /// `None` if `area` is not in the tree, or if the tree already holds [`MAX_AREAS`].
    pub fn split(&mut self, area: AreaId, axis: Axis, ratio: f64) -> Option<AreaId> {
        let node = self.node_of(area)?;
        if self.areas >= MAX_AREAS {
            return None;
        }
        let fresh = self.free_areas.pop().unwrap_or_else(|| {
            let id = self.next_area;
            self.next_area += 1;
            id
        });
        self.areas += 1;

        let a = self.alloc_node(Node::Area(area));
        let b = self.alloc_node(Node::Area(fresh));

        self.nodes[node] = Node::Split {
            axis,
            ratio: ratio.clamp(0.0, 1.0),
            a,
            b,
        };
        Some(fresh)
    }

    /// The area `area` could be joined with, if any.
    ///
    /// **Its sibling, and only its sibling** — that is the whole of what a binary tree
    /// can express, and §41.1 measures what it costs: on a screen of eight areas, eleven
    /// pairs share a full border and four of them are siblings. An area whose sibling is
    /// a split rather than a leaf has no partner at all.
    pub fn joinable(&self, area: AreaId) -> Option<AreaId> {
        let node = self.node_of(area)?;
        let parent = self.parent_of(node)?;
        let Node::Split { a, b, .. } = self.nodes[parent] else {
            return None;
        };
        let sibling = if a == node { b } else { a };
        match self.nodes[sibling] {
            Node::Area(other) => Some(other),
            _ => None,
        }
    }

    /// Whether [`join`](Self::join) would do anything for this pair.
    pub fn can_join(&self, keep: AreaId, dropped: AreaId) -> bool {
        keep != dropped && self.joinable(keep) == Some(dropped)
    }

    /// Merges two areas into one, keeping `keep` and freeing `dropped`.
    ///
    /// Returns whether anything happened. The two have to be **siblings**: the parent
    /// split disappears and `keep` takes the whole rectangle the two of them shared.
    /// `keep` keeps its id, and therefore its widget and everything that widget holds —
    /// which is the requirement, not the optimisation (§30 already priced losing a
    /// view's state).
    ///
    /// Blender joins any two areas with a coincident border; this joins the pairs that
    /// happen to be siblings, which is a subset. §41.1 says which pairs are missing and
    /// on what screens.
    pub fn join(&mut self, keep: AreaId, dropped: AreaId) -> bool {
        if !self.can_join(keep, dropped) {
            return false;
        }
        let (Some(keep_node), Some(dropped_node)) = (self.node_of(keep), self.node_of(dropped)) else {
            return false;
        };
        let Some(parent) = self.parent_of(keep_node) else {
            return false;
        };

        self.nodes[parent] = Node::Area(keep);
        self.free_node(keep_node);
        self.free_node(dropped_node);
        self.free_areas.push(dropped);
        self.areas -= 1;
        // An area that is gone cannot be the one shown alone.
        if self.maximized == Some(dropped) {
            self.maximized = None;
        }
        true
    }

    /// Exchanges the places of two areas. Returns whether anything happened.
    ///
    /// The **leaves** are exchanged, not the widgets: an area is its id, so moving the id
    /// moves everything keyed by it — the editor, its view, its selection, its
    /// materialised nodes. Nothing is rebuilt and nothing is told, which is why this is
    /// the version that feels like swapping two editors rather than two rectangles
    /// (§41.4).
    pub fn swap(&mut self, a: AreaId, b: AreaId) -> bool {
        if a == b {
            return false;
        }
        let (Some(node_a), Some(node_b)) = (self.node_of(a), self.node_of(b)) else {
            return false;
        };
        self.nodes[node_a] = Node::Area(b);
        self.nodes[node_b] = Node::Area(a);
        true
    }

    /// Shows one area alone, hiding the rest. Returns whether anything happened.
    ///
    /// The tree underneath is left exactly as it was, so [`restore`](Self::restore) gives
    /// back the same rectangles rather than rebuilt ones. Splitting or joining while
    /// maximized is allowed and lands in that hidden tree; it becomes visible on restore.
    pub fn maximize(&mut self, area: AreaId) -> bool {
        if self.maximized == Some(area) || !self.holds(area) {
            return false;
        }
        self.maximized = Some(area);
        true
    }

    /// Shows the whole screen again. Returns whether anything happened.
    pub fn restore(&mut self) -> bool {
        self.maximized.take().is_some()
    }

    /// The area shown alone, if the screen is maximized.
    pub fn maximized(&self) -> Option<AreaId> {
        self.maximized
    }

    /// The node holding `area`.
    fn node_of(&self, area: AreaId) -> Option<NodeId> {
        self.nodes
            .iter()
            .position(|n| matches!(n, Node::Area(id) if *id == area))
    }

    /// Takes an area off the screen, giving its place to whatever shared the split.
    ///
    /// Not [`join`](Self::join) with one argument: join merges two *leaves* and refuses
    /// an area whose sibling is a split (§41.1). This removes one leaf and hands its
    /// space to the sibling subtree, whatever shape that has — which is always possible,
    /// because every leaf but the root has a parent split. That difference is why detach
    /// works on any area and join does not.
    ///
    /// `false` for the last area: a screen with no area is not expressible, and the
    /// answer to "detach the only area" is that it refuses (decision 5 of the detach
    /// task).
    pub fn remove(&mut self, area: AreaId) -> bool {
        let Some(node) = self.node_of(area) else {
            return false;
        };
        let Some(parent) = self.parent_of(node) else {
            // The root leaf: the only area of the screen.
            return false;
        };
        let Node::Split { a, b, .. } = self.nodes[parent] else {
            return false;
        };
        let sibling = if a == node { b } else { a };

        self.nodes[parent] = self.nodes[sibling];
        self.free_node(sibling);
        self.free_node(node);
        self.free_areas.push(area);
        self.areas -= 1;
        if self.maximized == Some(area) {
            self.maximized = None;
        }
        true
    }

    /// The node whose child `node` is.
    ///
    /// A search rather than a stored parent pointer: a screen holds tens of nodes, and a
    /// pointer is one more invariant for every operation to keep.
    fn parent_of(&self, node: NodeId) -> Option<NodeId> {
        self.nodes
            .iter()
            .position(|n| matches!(n, Node::Split { a, b, .. } if *a == node || *b == node))
    }

    /// Writes the tree as a prefix expression: `split h 0.5 area 0 area 1`.
    ///
    /// Node ids are not written. They are indices into an array with free slots in it,
    /// so writing them would put this crate's bookkeeping in a file; the shape and the
    /// area ids are what a workspace means, and both survive the trip.
    pub(crate) fn write_expr(&self, out: &mut String) {
        self.write_node(self.root, out);
    }

    fn write_node(&self, node: NodeId, out: &mut String) {
        match self.nodes[node] {
            Node::Area(area) => out.push_str(&format!("area {area}")),
            Node::Split { axis, ratio, a, b } => {
                let axis = match axis {
                    Axis::Horizontal => 'h',
                    Axis::Vertical => 'v',
                };
                // `{}` on an `f64` is the shortest decimal that reads back as the same
                // number, which is what makes the round trip exact rather than close.
                out.push_str(&format!("split {axis} {ratio} "));
                self.write_node(a, out);
                out.push(' ');
                self.write_node(b, out);
            },
            Node::Free => {},
        }
    }

    /// Reads back what [`write_expr`](Self::write_expr) wrote.
    ///
    /// The tree comes out with `next_area` past the largest id it holds and every id below
    /// that it does not hold on the free list, exactly as if the holes had been left by
    /// joins. That is not tidiness: [`MAX_AREAS`] bounds the ids only because a hole is
    /// reused before a fresh id is handed out, and a hole a file brought in has to count
    /// too — otherwise `area 0` and `area 1023` would let the next split hand out 1024.
    ///
    /// `None` for anything the tree could not have become: an id at or above
    /// [`MAX_AREAS`], or splits nested deeper than that many areas could need. The depth is
    /// checked on the way down, before any leaf could bound it, so a file of nothing but
    /// `split` cannot exhaust the stack.
    pub(crate) fn parse_expr<'a>(tokens: &mut impl Iterator<Item = &'a str>) -> Option<Self> {
        let mut tree = Self {
            nodes: Vec::new(),
            root: 0,
            areas: 0,
            free_nodes: Vec::new(),
            free_areas: Vec::new(),
            next_area: 0,
            maximized: None,
        };
        tree.root = tree.parse_node(tokens, 0)?;
        if tokens.next().is_some() {
            return None;
        }
        // Descending, so that the lowest hole is the first one a split takes back.
        tree.free_areas = (0..tree.next_area).rev().filter(|&area| !tree.holds(area)).collect();
        Some(tree)
    }

    fn parse_node<'a>(&mut self, tokens: &mut impl Iterator<Item = &'a str>, depth: usize) -> Option<NodeId> {
        match tokens.next()? {
            "area" => {
                let area: AreaId = tokens.next()?.parse().ok()?;
                if area >= MAX_AREAS || self.holds(area) {
                    // Two leaves with one id would give two widgets one identity.
                    return None;
                }
                self.areas += 1;
                self.next_area = self.next_area.max(area + 1);
                Some(self.alloc_node(Node::Area(area)))
            },
            "split" => {
                let axis = match tokens.next()? {
                    "h" => Axis::Horizontal,
                    "v" => Axis::Vertical,
                    _ => return None,
                };
                let ratio: f64 = tokens.next()?.parse().ok()?;
                // A chain of `d` splits needs `d + 1` areas, so no tree is deeper than this.
                if !ratio.is_finite() || depth + 1 >= MAX_AREAS {
                    return None;
                }
                let a = self.parse_node(tokens, depth + 1)?;
                let b = self.parse_node(tokens, depth + 1)?;
                Some(self.alloc_node(Node::Split {
                    axis,
                    ratio: ratio.clamp(0.0, 1.0),
                    a,
                    b,
                }))
            },
            _ => None,
        }
    }

    /// Sets the maximized area while reading a file, if it is one the tree holds.
    pub(crate) fn set_maximized(&mut self, area: AreaId) -> bool {
        self.maximize(area)
    }

    /// Puts a node in a free slot, or at the end.
    fn alloc_node(&mut self, node: Node) -> NodeId {
        match self.free_nodes.pop() {
            Some(at) => {
                self.nodes[at] = node;
                at
            },
            None => {
                self.nodes.push(node);
                self.nodes.len() - 1
            },
        }
    }

    /// Empties a slot and offers it to the next allocation.
    fn free_node(&mut self, node: NodeId) {
        self.nodes[node] = Node::Free;
        self.free_nodes.push(node);
    }

    /// The share of its span the first child of `split` takes, if `split` is one.
    pub fn ratio(&self, split: NodeId) -> Option<f64> {
        match self.nodes.get(split)? {
            Node::Split { ratio, .. } => Some(*ratio),
            Node::Area(_) | Node::Free => None,
        }
    }

    /// Moves a splitter. Returns whether anything changed.
    ///
    /// Clamped rather than rejected at the edges: a drag that runs past the end of
    /// the span should pin the splitter there, not stop tracking the pointer.
    pub fn set_ratio(&mut self, split: NodeId, ratio: f64) -> bool {
        let Some(Node::Split { ratio: current, .. }) = self.nodes.get_mut(split) else {
            return false;
        };
        let clamped = ratio.clamp(0.0, 1.0);
        if *current == clamped {
            return false;
        }
        *current = clamped;
        true
    }

    /// Computes where every area and every splitter goes inside `rect`.
    ///
    /// Both outputs are cleared first, so the caller can keep reusing two buffers
    /// and a resize allocates nothing.
    pub fn layout(&self, rect: Rect, bar_thickness: f64, areas: &mut Vec<(AreaId, Rect)>, bars: &mut Vec<Bar>) {
        areas.clear();
        bars.clear();
        // Maximized: one area, the whole rect, and no splitter to grab. The tree is not
        // consulted beyond checking that the area is still in it, which is what makes
        // restoring exact.
        if let Some(area) = self.maximized {
            areas.push((area, rect));
            return;
        }
        self.layout_node(self.root, rect, bar_thickness, areas, bars);
    }

    fn layout_node(
        &self,
        node: NodeId,
        rect: Rect,
        bar_thickness: f64,
        areas: &mut Vec<(AreaId, Rect)>,
        bars: &mut Vec<Bar>,
    ) {
        match self.nodes[node] {
            Node::Area(area) => areas.push((area, rect)),
            // Unreachable from the root: a slot is freed only when its node leaves the
            // tree, and nothing points at it afterwards.
            Node::Free => {},
            Node::Split { axis, ratio, a, b } => {
                let (first, bar, second) = split_rect(rect, axis, ratio, bar_thickness);
                bars.push(Bar {
                    split: node,
                    axis,
                    rect: bar,
                    span: rect,
                });
                self.layout_node(a, first, bar_thickness, areas, bars);
                self.layout_node(b, second, bar_thickness, areas, bars);
            },
        }
    }
}

/// Divides `rect` into first child, splitter bar and second child.
///
/// The first extent is rounded to a whole pixel, and that rounding is load-bearing
/// rather than cosmetic. An unrounded ratio makes every descendant rect drift by a
/// fraction of a pixel on every frame of a drag, so every area would count as
/// resized and the whole point of measuring how many areas a drag disturbs would be
/// lost — along with the layout work the measurement is there to catch.
fn split_rect(rect: Rect, axis: Axis, ratio: f64, bar: f64) -> (Rect, Rect, Rect) {
    let (extent, origin) = match axis {
        Axis::Horizontal => (rect.width(), rect.x0),
        Axis::Vertical => (rect.height(), rect.y0),
    };
    let bar = bar.min(extent);
    let usable = extent - bar;
    let first = (usable * ratio).round().clamp(0.0, usable);

    let bar_start = origin + first;
    let second_start = bar_start + bar;
    let end = origin + extent;

    match axis {
        Axis::Horizontal => (
            Rect::new(rect.x0, rect.y0, bar_start, rect.y1),
            Rect::new(bar_start, rect.y0, second_start, rect.y1),
            Rect::new(second_start, rect.y0, end, rect.y1),
        ),
        Axis::Vertical => (
            Rect::new(rect.x0, rect.y0, rect.x1, bar_start),
            Rect::new(rect.x0, bar_start, rect.x1, second_start),
            Rect::new(rect.x0, second_start, rect.x1, end),
        ),
    }
}

/// The ratio a pointer at `pos` implies for a bar.
///
/// Lives here rather than in the widget because it is the exact inverse of
/// `split_rect`, and an inverse that drifts from its forward function is a bug
/// nobody sees until the splitter starts lagging the pointer.
pub fn ratio_at(bar: &Bar, pos: Point, bar_thickness: f64) -> f64 {
    let (extent, origin, at) = match bar.axis {
        Axis::Horizontal => (bar.span.width(), bar.span.x0, pos.x),
        Axis::Vertical => (bar.span.height(), bar.span.y0, pos.y),
    };
    let usable = extent - bar_thickness.min(extent);
    if usable <= 0.0 {
        return 0.0;
    }
    ((at - origin - bar_thickness / 2.0) / usable).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::new(0.0, 0.0, 1400.0, 900.0);
    const BAR: f64 = 4.0;

    fn laid_out(tree: &SplitTree) -> (Vec<(AreaId, Rect)>, Vec<Bar>) {
        let (mut areas, mut bars) = (Vec::new(), Vec::new());
        tree.layout(SCREEN, BAR, &mut areas, &mut bars);
        (areas, bars)
    }

    #[test]
    fn one_area_fills_the_screen() {
        let (areas, bars) = laid_out(&SplitTree::single());
        assert_eq!(areas, vec![(0, SCREEN)]);
        assert!(bars.is_empty(), "a single area has nothing to divide");
    }

    /// A tiling that leaves gaps or overlaps is a tiling that paints garbage between
    /// areas or lets two of them fight over the same pixels, and neither shows up in
    /// a timing.
    #[test]
    fn areas_and_bars_tile_the_screen_exactly() {
        for count in [1, 2, 3, 4, 7, 8, 16] {
            let (areas, bars) = laid_out(&SplitTree::balanced(count));
            assert_eq!(areas.len(), count);
            assert_eq!(bars.len(), count - 1, "{count} areas need {} splitters", count - 1);

            let covered: f64 =
                areas.iter().map(|(_, r)| r.area()).sum::<f64>() + bars.iter().map(|b| b.rect.area()).sum::<f64>();
            // Bars nested inside a half are counted once each and never overlap an
            // area, so the three sums must add back up to the screen.
            assert!(
                (covered - SCREEN.area()).abs() < 1.0,
                "{count} areas cover {covered} of {}",
                SCREEN.area()
            );

            for (i, (_, a)) in areas.iter().enumerate() {
                for (_, b) in areas.iter().skip(i + 1) {
                    assert!(a.intersect(*b).area() < 1.0, "areas overlap: {a:?} and {b:?}");
                }
            }
        }
    }

    #[test]
    fn every_area_gets_a_distinct_id() {
        let (areas, _) = laid_out(&SplitTree::balanced(8));
        let mut ids: Vec<_> = areas.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..8).collect::<Vec<_>>());
    }

    /// Splitting has to be non-destructive: whatever widget the caller built for the
    /// area being split is still that area's widget afterwards.
    #[test]
    fn splitting_keeps_the_existing_area_id() {
        let mut tree = SplitTree::single();
        let fresh = tree.split(0, Axis::Horizontal, 0.5).expect("area 0 exists");
        assert_eq!(fresh, 1);
        assert_eq!(tree.area_count(), 2);

        let (areas, bars) = laid_out(&tree);
        assert_eq!(areas.len(), 2);
        assert_eq!(bars.len(), 1);
        assert!(areas.iter().any(|(id, _)| *id == 0), "area 0 survives the split");
    }

    #[test]
    fn splitting_a_missing_area_changes_nothing() {
        let mut tree = SplitTree::single();
        assert_eq!(tree.split(7, Axis::Horizontal, 0.5), None);
        assert_eq!(tree.area_count(), 1);
    }

    #[test]
    fn set_ratio_reports_change_and_clamps() {
        let mut tree = SplitTree::balanced(2);
        let (_, bars) = laid_out(&tree);
        let split = bars[0].split;

        assert!(tree.set_ratio(split, 0.25));
        assert_eq!(tree.ratio(split), Some(0.25));
        assert!(!tree.set_ratio(split, 0.25), "an unchanged ratio is not a change");

        assert!(tree.set_ratio(split, 5.0));
        assert_eq!(tree.ratio(split), Some(1.0), "past the end pins to the end");
        assert_eq!(tree.ratio(usize::MAX), None);
    }

    /// Whether two laid-out areas share a whole border, which is what Blender needs to
    /// offer a join.
    ///
    /// They do not touch — a splitter sits between them — so the test is that the gap is
    /// exactly the bar and that the other axis matches end to end.
    fn share_a_border(a: Rect, b: Rect) -> bool {
        let gap_x = (a.x1 + BAR - b.x0).abs() < 0.5 || (b.x1 + BAR - a.x0).abs() < 0.5;
        let same_y = (a.y0 - b.y0).abs() < 0.5 && (a.y1 - b.y1).abs() < 0.5;
        let gap_y = (a.y1 + BAR - b.y0).abs() < 0.5 || (b.y1 + BAR - a.y0).abs() < 0.5;
        let same_x = (a.x0 - b.x0).abs() < 0.5 && (a.x1 - b.x1).abs() < 0.5;
        (gap_x && same_y) || (gap_y && same_x)
    }

    /// Pairs that share a whole border, and how many of them the tree can actually join.
    fn joinable_pairs(count: usize) -> (usize, usize) {
        let tree = SplitTree::balanced(count);
        let (areas, _) = laid_out(&tree);
        let (mut bordering, mut siblings) = (0, 0);
        for (i, (a, ra)) in areas.iter().enumerate() {
            for (b, rb) in areas.iter().skip(i + 1) {
                if share_a_border(*ra, *rb) {
                    bordering += 1;
                    if tree.can_join(*a, *b) {
                        siblings += 1;
                    }
                }
            }
        }
        (bordering, siblings)
    }

    /// **The question this whole subsystem was asked**: does a binary tree survive join?
    ///
    /// It does not, and this is where. Blender merges any two areas whose border
    /// coincides; a tree can only merge **siblings**, and the two sets part company as
    /// soon as the screen is deeper than one split. The numbers are pinned rather than
    /// described, because "a tree is not enough" is a claim that has to be checkable.
    #[test]
    fn the_tree_can_join_only_some_of_the_pairs_that_share_a_border() {
        // (areas, pairs sharing a border, pairs the tree can join)
        for (count, bordering, joinable) in [(2, 1, 1), (4, 4, 2), (8, 10, 4), (16, 24, 8)] {
            assert_eq!(
                joinable_pairs(count),
                (bordering, joinable),
                "{count} areas: bordering pairs and the subset that are siblings"
            );
        }
    }

    /// The smallest screen where the difference above is a real screen, spelled out.
    ///
    /// Four areas: two columns of two. The two top areas share a whole border and are
    /// cousins, not siblings — Blender would offer that join and this tree cannot.
    #[test]
    fn two_areas_sharing_a_border_may_still_be_unjoinable() {
        let tree = SplitTree::balanced(4);
        let (areas, _) = laid_out(&tree);
        let rect = |id: AreaId| areas.iter().find(|(a, _)| *a == id).expect("area exists").1;

        assert!(
            share_a_border(rect(0), rect(2)),
            "the two top areas sit either side of the root splitter: {:?} and {:?}",
            rect(0),
            rect(2)
        );
        assert!(!tree.can_join(0, 2), "and the tree cannot express it");
        assert_eq!(tree.joinable(0), Some(1), "0 can only be joined with its sibling below");
    }

    #[test]
    fn joining_siblings_keeps_the_survivor_and_frees_the_other() {
        let mut tree = SplitTree::balanced(4);
        let before = laid_out(&tree).0;
        assert!(tree.can_join(0, 1));

        assert!(tree.join(0, 1));
        assert_eq!(tree.area_count(), 3);
        assert!(tree.holds(0) && !tree.holds(1));

        let (areas, bars) = laid_out(&tree);
        assert_eq!(areas.len(), 3);
        assert_eq!(bars.len(), 2, "three areas need two splitters");

        // Area 0 took the whole rectangle the pair shared, and nobody else moved.
        let rect = |list: &[(AreaId, Rect)], id: AreaId| list.iter().find(|(a, _)| *a == id).map(|(_, r)| *r);
        assert_eq!(rect(&areas, 2), rect(&before, 2), "the other column did not move");
        assert_eq!(rect(&areas, 3), rect(&before, 3));
        let grown = rect(&areas, 0).expect("area 0 survives");
        assert!(grown.height() > rect(&before, 0).expect("area 0 was there").height());
    }

    /// Ids are the caller's index into its own arrays, so a join must not renumber.
    #[test]
    fn a_join_renumbers_nothing_and_the_id_comes_back() {
        let mut tree = SplitTree::balanced(4);
        assert!(tree.join(2, 3));
        let mut left: Vec<AreaId> = tree.areas().collect();
        left.sort_unstable();
        assert_eq!(left, vec![0, 1, 2], "everyone kept the id they had");

        // The freed id is handed out again rather than retired.
        assert_eq!(tree.split(0, Axis::Horizontal, 0.5), Some(3));
        assert_eq!(tree.area_count(), 4);
    }

    #[test]
    fn a_join_needs_two_areas_that_are_siblings() {
        let mut tree = SplitTree::balanced(4);
        assert!(!tree.join(0, 0), "an area is not its own sibling");
        assert!(!tree.join(0, 9), "a missing area joins nothing");
        assert!(!tree.join(0, 2), "cousins are not siblings");
        assert_eq!(tree.area_count(), 4, "and nothing happened");
    }

    /// An area whose sibling is a split, not a leaf, has no partner at all.
    #[test]
    fn an_area_whose_sibling_is_a_split_cannot_join() {
        let mut tree = SplitTree::single();
        let right = tree.split(0, Axis::Horizontal, 0.5).expect("area 0 exists");
        tree.split(right, Axis::Vertical, 0.5).expect("the new area exists");
        assert_eq!(tree.joinable(0), None, "area 0's sibling is a split");
    }

    /// Restoring has to give back the rectangles that were there, not ones like them.
    #[test]
    fn maximize_and_restore_return_the_same_rectangles() {
        let mut tree = SplitTree::balanced(8);
        let (before, bars_before) = laid_out(&tree);

        assert!(tree.maximize(3));
        assert_eq!(tree.maximized(), Some(3));
        let (areas, bars) = laid_out(&tree);
        assert_eq!(areas, vec![(3, SCREEN)], "one area, the whole screen");
        assert!(bars.is_empty(), "nothing to drag while maximized");

        assert!(tree.restore());
        assert_eq!(tree.maximized(), None);
        let (after, bars_after) = laid_out(&tree);
        assert_eq!(after, before, "bit for bit, not approximately");
        assert_eq!(bars_after, bars_before);
    }

    #[test]
    fn maximizing_what_is_not_there_changes_nothing() {
        let mut tree = SplitTree::balanced(4);
        assert!(!tree.maximize(9));
        assert!(!tree.restore(), "nothing was maximized");
        assert!(tree.maximize(1));
        assert!(!tree.maximize(1), "already maximized");
    }

    /// A maximized area that is joined away must not leave the screen showing nothing.
    #[test]
    fn joining_the_maximized_area_restores_the_screen() {
        let mut tree = SplitTree::balanced(4);
        assert!(tree.maximize(1));
        assert!(tree.join(0, 1));
        assert_eq!(tree.maximized(), None);
        assert_eq!(laid_out(&tree).0.len(), 3);
    }

    /// Swap moves the areas, so whatever is keyed by an id moves with it.
    #[test]
    fn swapping_exchanges_the_rectangles_and_nothing_else() {
        let mut tree = SplitTree::balanced(4);
        let before = laid_out(&tree).0;
        let rect = |list: &[(AreaId, Rect)], id: AreaId| list.iter().find(|(a, _)| *a == id).map(|(_, r)| *r);

        assert!(tree.swap(0, 3));
        let after = laid_out(&tree).0;

        assert_eq!(rect(&after, 0), rect(&before, 3));
        assert_eq!(rect(&after, 3), rect(&before, 0));
        assert_eq!(rect(&after, 1), rect(&before, 1), "the others stayed put");
        assert_eq!(tree.area_count(), 4);
        assert!(!tree.swap(2, 2), "an area does not swap with itself");
        assert!(!tree.swap(2, 9), "nor with one that is not there");
    }

    /// `ratio_at` is the inverse of the layout, and an inverse that drifts from its
    /// forward function is a splitter that lags the pointer by a growing amount.
    #[test]
    fn dragging_puts_the_bar_under_the_pointer() {
        let mut tree = SplitTree::balanced(2);
        let (_, bars) = laid_out(&tree);
        let bar = bars[0];

        for target in [200.0, 700.0, 1100.0] {
            let pos = Point::new(target, 450.0);
            assert!(tree.set_ratio(bar.split, ratio_at(&bar, pos, BAR)));
            let (_, bars) = laid_out(&tree);
            let centre = bars[0].rect.center().x;
            assert!(
                (centre - target).abs() <= 1.0,
                "bar landed at {centre}, pointer was at {target}"
            );
        }
    }
}
