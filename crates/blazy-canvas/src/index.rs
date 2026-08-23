//! A uniform grid over canvas coordinates, so that finding what is on screen does
//! not cost a walk over the whole graph.
//!
//! Phase 0 removed the linear costs one at a time and left this one deliberately:
//! the scan touches 32 bytes per node and no widget state, so it stayed invisible
//! next to the cost of materialising widgets. Measured, it stays invisible up to
//! about 64 000 nodes and then does not: panning a million-node graph spends 6.8 ms
//! a frame walking geometry for the 24 nodes that are actually visible (§24).
//!
//! # Why a grid and not an R-tree
//!
//! A grid is an order of magnitude simpler and needs no rebalancing, which matters
//! because nodes move: dragging one has to update the index without touching the
//! rest. The case a grid is supposed to lose on is clustering — everything piled
//! into a few cells — but the query only ever visits cells that overlap the
//! viewport, so a dense cluster costs its own size only when it is on screen, and
//! then those nodes have to be dealt with anyway. §24 has the measurement on a
//! clustered graph.

use masonry::kurbo::{Point, Rect};

/// Nodes a cell should hold on average.
///
/// Smaller cells mean fewer candidates per query and more memory: at one node per
/// cell a million-node graph would allocate a million `Vec`s. Eight keeps the
/// candidate set small while keeping the cell count an eighth of the node count.
const TARGET_PER_CELL: f64 = 8.0;

/// The largest grid we will build, in cells.
///
/// A guard against a graph whose bounding box is enormous but whose nodes are few,
/// where the cell size derived from the node count would produce an absurd number of
/// empty cells.
const MAX_CELLS: usize = 1 << 20;

/// Where a node currently sits in the grid.
type CellId = u32;

/// A uniform grid of node indices, keyed by canvas position.
#[derive(Debug)]
pub(crate) struct SpatialIndex {
    /// Top-left corner of the grid in canvas coordinates.
    origin: Point,
    /// Side of one square cell, in canvas units.
    cell: f64,
    cols: usize,
    rows: usize,
    /// Node indices per cell.
    cells: Vec<Vec<u32>>,
    /// Which cell each node is filed under, so a move can remove it from the old one.
    filed: Vec<CellId>,
    /// Cells of slack a query needs on the low side, per axis.
    ///
    /// A node is filed by its top-left corner, so a node filed to the left of the
    /// query rect can still reach into it. How far back to look is decided by the
    /// widest node rather than assumed: one cell would be wrong the moment a node is
    /// bigger than a cell, and nothing would report it — the node would simply stop
    /// being drawn near the left edge.
    slack: (usize, usize),
}

impl SpatialIndex {
    /// Builds an index over node rectangles.
    pub(crate) fn build(rects: impl ExactSizeIterator<Item = Rect> + Clone) -> Self {
        let count = rects.len();
        let bounds = rects
            .clone()
            .fold(Rect::ZERO, |acc, r| if acc == Rect::ZERO { r } else { acc.union(r) });

        let (width, height) = (bounds.width().max(1.0), bounds.height().max(1.0));
        let cell = if count == 0 {
            width
        } else {
            (width * height * TARGET_PER_CELL / count as f64).sqrt().max(1.0)
        };

        let mut grid = Self {
            origin: bounds.origin(),
            cell,
            cols: 1,
            rows: 1,
            cells: Vec::new(),
            filed: Vec::new(),
            slack: (0, 0),
        };
        grid.resize_for(width, height);
        grid.cells = vec![Vec::new(); grid.cols * grid.rows];
        grid.filed = Vec::with_capacity(count);

        let (mut widest, mut tallest) = (0.0_f64, 0.0_f64);
        for (index, rect) in rects.enumerate() {
            widest = widest.max(rect.width());
            tallest = tallest.max(rect.height());
            let cell = grid.cell_of(rect.origin());
            grid.cells[cell as usize].push(index as u32);
            grid.filed.push(cell);
        }
        grid.slack = (
            (widest / grid.cell).ceil() as usize,
            (tallest / grid.cell).ceil() as usize,
        );
        grid
    }

    /// Chooses a grid shape that covers `width` x `height` without exceeding [`MAX_CELLS`].
    fn resize_for(&mut self, width: f64, height: f64) {
        loop {
            self.cols = (width / self.cell).ceil().max(1.0) as usize;
            self.rows = (height / self.cell).ceil().max(1.0) as usize;
            if self.cols.saturating_mul(self.rows) <= MAX_CELLS {
                return;
            }
            self.cell *= 2.0;
        }
    }

    /// The cell a canvas point falls in, clamped to the grid.
    ///
    /// Clamping rather than rejecting is what lets a node be dragged outside the
    /// original bounds: it is filed in a border cell, and a query from out there is
    /// clamped to the same border cell, so it is still found.
    fn cell_of(&self, point: Point) -> CellId {
        let col = (((point.x - self.origin.x) / self.cell).floor().max(0.0) as usize).min(self.cols - 1);
        let row = (((point.y - self.origin.y) / self.cell).floor().max(0.0) as usize).min(self.rows - 1);
        (row * self.cols + col) as CellId
    }

    /// Refiles a node whose position changed.
    pub(crate) fn moved(&mut self, index: usize, to: Point) {
        let cell = self.cell_of(to);
        let old = self.filed[index];
        if old == cell {
            return;
        }
        let bucket = &mut self.cells[old as usize];
        if let Some(at) = bucket.iter().position(|&i| i == index as u32) {
            bucket.swap_remove(at);
        }
        self.cells[cell as usize].push(index as u32);
        self.filed[index] = cell;
    }

    /// Appends the indices of every node whose cell overlaps `rect`.
    ///
    /// Candidates, not answers: a cell overlapping the rect may hold nodes that do
    /// not, so the caller still tests each rectangle. Output is sorted ascending,
    /// which is what the canvas's set difference expects.
    ///
    /// The length of the output is also the number of geometries examined, which is
    /// the counter the spatial-index criterion is decided on: exact, machine
    /// independent, and unlike the microseconds it replaced it cannot be lost in
    /// noise. There is no separate counter for it, because a second way to count the
    /// same thing is a second thing to keep in step.
    pub(crate) fn candidates(&self, rect: Rect, out: &mut Vec<usize>) {
        out.clear();
        if self.cells.is_empty() {
            return;
        }

        let low = self.cell_coords(rect.origin());
        let high = self.cell_coords(Point::new(rect.x1, rect.y1));
        let (col0, row0) = (low.0.saturating_sub(self.slack.0), low.1.saturating_sub(self.slack.1));

        for row in row0..=high.1 {
            let base = row * self.cols;
            for col in col0..=high.0 {
                out.extend(self.cells[base + col].iter().map(|&i| i as usize));
            }
        }
        out.sort_unstable();
    }

    fn cell_coords(&self, point: Point) -> (usize, usize) {
        let cell = self.cell_of(point) as usize;
        (cell % self.cols, cell / self.cols)
    }
}

#[cfg(test)]
mod tests {
    use masonry::kurbo::Size;

    use super::*;

    fn grid(n: usize, step: f64) -> (SpatialIndex, Vec<Rect>) {
        let rects: Vec<Rect> = (0..n)
            .map(|i| {
                let (col, row) = (i % 40, i / 40);
                Rect::from_origin_size(Point::new(col as f64 * step, row as f64 * step), Size::new(20.0, 20.0))
            })
            .collect();
        (SpatialIndex::build(rects.iter().copied()), rects)
    }

    /// The candidate set has to be a superset of the true answer, always. Everything
    /// else the index does is an optimisation; this is correctness.
    #[test]
    fn candidates_never_miss_a_node() {
        let (index, rects) = grid(2000, 60.0);
        let mut out = Vec::new();

        for query in [
            Rect::new(0.0, 0.0, 100.0, 100.0),
            Rect::new(500.0, 500.0, 900.0, 700.0),
            Rect::new(-1000.0, -1000.0, 1000.0, 1000.0),
            Rect::new(1e6, 1e6, 1e6 + 10.0, 1e6 + 10.0),
        ] {
            index.candidates(query, &mut out);
            let truth: Vec<usize> = rects
                .iter()
                .enumerate()
                .filter(|(_, r)| r.overlaps(query))
                .map(|(i, _)| i)
                .collect();
            for wanted in truth {
                assert!(out.contains(&wanted), "query {query:?} missed node {wanted}");
            }
        }
    }

    #[test]
    fn candidates_come_out_sorted() {
        let (index, _) = grid(500, 60.0);
        let mut out = Vec::new();
        index.candidates(Rect::new(0.0, 0.0, 400.0, 400.0), &mut out);
        assert!(out.windows(2).all(|w| w[0] < w[1]), "{out:?}");
    }

    /// The whole point: a query costs what is nearby, not what exists.
    #[test]
    fn a_small_query_does_not_visit_the_whole_graph() {
        let mut out = Vec::new();
        let (small, _) = grid(1_000, 60.0);
        small.candidates(Rect::new(0.0, 0.0, 100.0, 100.0), &mut out);
        let visited_small = out.len();

        let (large, _) = grid(64_000, 60.0);
        large.candidates(Rect::new(0.0, 0.0, 100.0, 100.0), &mut out);

        assert!(
            out.len() < visited_small * 4,
            "visiting {} against {visited_small} for 64x the nodes",
            out.len()
        );
    }

    /// Dragging a node has to keep the index correct without rebuilding it.
    #[test]
    fn a_moved_node_is_found_at_its_new_place() {
        let (mut index, _) = grid(500, 60.0);
        let mut out = Vec::new();
        let far = Point::new(50_000.0, 50_000.0);

        index.moved(7, far);

        index.candidates(Rect::new(0.0, 0.0, 100.0, 100.0), &mut out);
        assert!(!out.contains(&7), "the node left its old cell");

        index.candidates(Rect::from_origin_size(far, Size::new(20.0, 20.0)), &mut out);
        assert!(out.contains(&7), "and arrived in the new one");
    }

    /// Nodes wider than a cell are the case a fixed one-cell slack would get wrong,
    /// silently, by dropping them near the left edge of the viewport.
    #[test]
    fn nodes_wider_than_a_cell_are_still_found() {
        let rects: Vec<Rect> = (0..400)
            .map(|i| {
                Rect::from_origin_size(
                    Point::new((i % 20) as f64 * 500.0, (i / 20) as f64 * 500.0),
                    Size::new(2_000.0, 40.0),
                )
            })
            .collect();
        let index = SpatialIndex::build(rects.iter().copied());
        let mut out = Vec::new();

        for (i, rect) in rects.iter().enumerate() {
            // A query touching the right-hand end of a wide node, far from its origin.
            let query = Rect::new(rect.x1 - 5.0, rect.y0, rect.x1 - 1.0, rect.y1);
            index.candidates(query, &mut out);
            assert!(out.contains(&i), "node {i} lost at its far end");
        }
    }

    /// A node parked outside the original bounds still has to be findable, which is
    /// what clamping the query to the grid buys.
    #[test]
    fn nodes_outside_the_original_bounds_are_still_found() {
        let (mut index, _) = grid(200, 60.0);
        let mut out = Vec::new();
        index.moved(3, Point::new(-9_000.0, -9_000.0));
        index.candidates(Rect::new(-9_100.0, -9_100.0, -8_900.0, -8_900.0), &mut out);
        assert!(out.contains(&3));
    }

    #[test]
    fn an_empty_graph_is_not_a_special_case() {
        let index = SpatialIndex::build(std::iter::empty());
        let mut out = vec![1, 2, 3];
        index.candidates(Rect::new(0.0, 0.0, 10.0, 10.0), &mut out);
        assert!(out.is_empty());
    }
}
