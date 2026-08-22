//! Phase 0.5 feasibility experiment: a window tiled into Blender-style areas.
//!
//! Phase 0 measured one editor filling one window and found that frame cost is the
//! cost of walking the widget tree, which is a per-window quantity
//! (`rnd/architecture.md` §20.2). A Blender screen is six or eight editors in that
//! same window. This experiment asks the two questions that leaves open:
//!
//! * do the areas' costs add up, or does splitting a window merely divide the same viewport into smaller pieces?
//! * does dragging a splitter re-lay-out the screen, or only what it moved?
//!
//! ```text
//! cargo make run-area-screen      # interactive window
//! cargo make bench-areas          # headless measurements and the criteria
//! ```
//!
//! Every area holds a node canvas over **one shared graph**, so the numbers line up
//! with Phase 0's and so the sweep over area counts changes only the tiling.

use blazy_areas::{AreaScreen, SplitTree};
use masonry::core::{NewWidget, Widget};
use node_canvas::canvas_over;
use node_canvas::model::{GraphModel, SharedGraph, share};

/// Default area count. Roughly what a working Blender screen carries.
pub const DEFAULT_AREAS: usize = 8;

/// Builds a screen of `areas` areas, each showing the same graph of `nodes` nodes.
///
/// The graph is returned alongside so a caller can hold it: the canvases keep only
/// a shared borrow, and the model is the source of truth that outlives every view.
pub fn build_screen(areas: usize, nodes: usize) -> (AreaScreen, SharedGraph) {
    let graph = share(GraphModel::generated(nodes));
    let screen = AreaScreen::new(SplitTree::balanced(areas), |_area| {
        NewWidget::new(canvas_over(&graph, nodes, false)).erased()
    });
    (screen, graph)
}

/// The canvas inside an area, as a `dyn Widget`, for callers that build their own tree.
pub fn area_canvas(graph: &SharedGraph, nodes: usize) -> NewWidget<dyn Widget> {
    NewWidget::new(canvas_over(graph, nodes, false)).erased()
}
