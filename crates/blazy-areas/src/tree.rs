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
pub type NodeId = usize;

/// Index of an area. Stable for the life of the tree.
pub type AreaId = usize;

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
}

/// One splitter, as laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    /// The split this bar divides. Pass it to [`SplitTree::set_ratio`].
    pub split: NodeId,
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
    areas: usize,
}

impl SplitTree {
    /// A screen holding one area.
    pub fn single() -> Self {
        Self {
            nodes: vec![Node::Area(0)],
            root: 0,
            areas: 1,
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
    /// Panics if `areas` is zero: a screen with no area has no meaning, and every
    /// caller here knows its count statically.
    pub fn balanced(areas: usize) -> Self {
        assert!(areas > 0, "a screen needs at least one area");
        let mut tree = Self {
            nodes: Vec::new(),
            root: 0,
            areas,
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

    /// Splits `area` in two, returning the id of the area that appears.
    ///
    /// The existing area keeps its id and the first `ratio` of the space, which is
    /// what makes a split non-destructive: whatever widget the caller has already
    /// built for `area` stays valid and stays where it was.
    pub fn split(&mut self, area: AreaId, axis: Axis, ratio: f64) -> Option<AreaId> {
        let node = self
            .nodes
            .iter()
            .position(|n| matches!(n, Node::Area(id) if *id == area))?;
        let fresh = self.areas;
        self.areas += 1;

        self.nodes.push(Node::Area(area));
        let a = self.nodes.len() - 1;
        self.nodes.push(Node::Area(fresh));
        let b = self.nodes.len() - 1;

        self.nodes[node] = Node::Split {
            axis,
            ratio: ratio.clamp(0.0, 1.0),
            a,
            b,
        };
        Some(fresh)
    }

    /// The share of its span the first child of `split` takes, if `split` is one.
    pub fn ratio(&self, split: NodeId) -> Option<f64> {
        match self.nodes.get(split)? {
            Node::Split { ratio, .. } => Some(*ratio),
            Node::Area(_) => None,
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
/// [`split_rect`], and an inverse that drifts from its forward function is a bug
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
