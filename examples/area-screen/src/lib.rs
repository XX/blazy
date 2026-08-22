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

pub mod header;

use blazy_areas::{AreaContent, AreaScreen, SplitTree};
use masonry::core::{NewWidget, Widget};
use masonry::peniko::Color;
use node_canvas::canvas_over;
use node_canvas::model::{GraphModel, SharedGraph, share};

use crate::header::ScaledHeader;

/// Height of a region header at `ui_scale` 1.0, in logical pixels.
pub const HEADER_HEIGHT: f64 = 24.0;

/// Default area count. Roughly what a working Blender screen carries.
pub const DEFAULT_AREAS: usize = 8;

/// Builds a screen of `areas` areas, each a header region above a canvas region.
///
/// The graph is returned alongside so a caller can hold it: the canvases keep only
/// a shared borrow, and the model is the source of truth that outlives every view.
pub fn build_screen(areas: usize, nodes: usize) -> (AreaScreen, SharedGraph) {
    build_screen_with(areas, nodes, true)
}

/// Interface scales handed out by [`build_screen_staggered`], cycled over the areas.
pub const STAGGERED_SCALES: [f64; 4] = [1.0, 1.25, 1.5, 1.75];

/// As [`build_screen`], with every area's header at a different interface scale.
///
/// What the window opens with. A screenshot of one staggered screen says what
/// per-region `ui_scale` means more directly than any number does: the same header,
/// built from the same widget, at four sizes in one window, while the canvases below
/// them are untouched.
pub fn build_screen_staggered(areas: usize, nodes: usize, forced: Option<f64>) -> (AreaScreen, SharedGraph) {
    let graph = share(GraphModel::generated(nodes));
    let screen = AreaScreen::new(SplitTree::balanced(areas), |area| {
        let scale = forced.unwrap_or(STAGGERED_SCALES[area % STAGGERED_SCALES.len()]);
        let content = AreaContent::header_and_main(HEADER_HEIGHT, area_header(area), area_canvas(&graph, nodes))
            .with_ui_scale(0, scale);
        NewWidget::new(content).erased()
    });
    (screen, graph)
}

/// As [`build_screen`], optionally without the header region.
///
/// The headerless form is what the sweep over region counts compares against: one
/// region per area, so the difference between the two is the price of a region and
/// nothing else.
pub fn build_screen_with(areas: usize, nodes: usize, with_header: bool) -> (AreaScreen, SharedGraph) {
    let graph = share(GraphModel::generated(nodes));
    let screen = AreaScreen::new(SplitTree::balanced(areas), |area| {
        let canvas = area_canvas(&graph, nodes);
        if with_header {
            NewWidget::new(AreaContent::header_and_main(HEADER_HEIGHT, area_header(area), canvas)).erased()
        } else {
            NewWidget::new(AreaContent::new(vec![(blazy_areas::RegionKind::Main, 0.0, canvas)])).erased()
        }
    });
    (screen, graph)
}

/// The canvas inside an area, as a `dyn Widget`.
pub fn area_canvas(graph: &SharedGraph, nodes: usize) -> NewWidget<dyn Widget> {
    NewWidget::new(canvas_over(graph, nodes, false)).erased()
}

/// The header of area `area`, tinted so the areas are told apart by eye.
pub fn area_header(area: usize) -> NewWidget<dyn Widget> {
    const TINTS: [Color; 4] = [
        Color::from_rgb8(0x6b, 0x4b, 0x8a),
        Color::from_rgb8(0x3c, 0x6e, 0x71),
        Color::from_rgb8(0x8a, 0x5a, 0x3c),
        Color::from_rgb8(0x44, 0x6b, 0x3c),
    ];
    NewWidget::new(ScaledHeader::new(TINTS[area % TINTS.len()])).erased()
}
