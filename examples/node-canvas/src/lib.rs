//! Phase 0 feasibility experiment for blazy.
//!
//! A node canvas of 5000 nodes on Masonry — per-child `Affine`, culling, LOD, ordinary
//! sliders and checkboxes inside the nodes — and its measurements. The criteria:
//!
//! * panning must not re-run layout on the children;
//! * moving one node must not rebuild the whole window;
//! * controls inside a zoomed node must keep working.
//!
//! ```text
//! cargo make run-node-canvas   # interactive window
//! cargo make bench             # headless measurements and the criteria
//! cargo make bench-report          # what CI runs, plus a JSON report
//! ```
//!
//! A library so the window, the benchmark and the tests build the same canvas (a bench
//! target can reach only the library).

pub mod editor;
pub mod model;
pub mod node;
pub mod ops;

#[cfg(test)]
mod ops_tests;
#[cfg(test)]
mod tests;

use blazy::canvas::{CanvasLayer, Link};

use crate::model::{GraphModel, NODE_SIZE, SharedGraph, share};
use crate::node::GraphSource;

/// The theme, plus what a selected node looks like (a layer of `GraphNode`'s property
/// stack, matched by the `selected` class). Used by the window, the tests and the benchmarks.
pub fn property_set() -> blazy::masonry::core::DefaultProperties {
    use blazy::masonry::core::{PropertyStack, Selector};
    use blazy::masonry::peniko::Color;
    use blazy::masonry::properties::BorderColor;
    let mut properties = blazy::masonry::theme::default_property_set();
    let mut stack = PropertyStack::new();
    stack.push_layer(Selector::classes(&[blazy::node_editor::SELECTED]), BorderColor {
        color: Color::from_rgb8(0xff, 0xa5, 0x2c),
    });
    properties.insert_stack::<crate::node::GraphNode>(stack);
    properties
}

/// Default graph size. The figure comes straight from the Phase 0 brief.
pub const DEFAULT_NODES: usize = 5000;

/// How a canvas is put together for a window, a test or a measurement.
#[derive(Clone, Debug)]
pub struct CanvasSpec {
    /// Nodes in the generated graph.
    pub nodes: usize,
    /// Edges: the generated ones unless an exact set is given (the link sweep gives one).
    pub links: Option<Vec<Link>>,
    /// Whether only the node under the pointer gets interactive controls. Off by default:
    /// the painted stand-in does not look enough like Masonry's controls.
    pub controls_on_hover: bool,
    /// The far-field knobs.
    pub far: FarTuning,
}

impl CanvasSpec {
    #[must_use]
    pub fn new(nodes: usize) -> Self {
        Self {
            nodes,
            links: None,
            controls_on_hover: false,
            far: FarTuning::default(),
        }
    }

    #[must_use]
    pub fn with_controls_on_hover(mut self, enabled: bool) -> Self {
        self.controls_on_hover = enabled;
        self
    }

    #[must_use]
    pub fn with_links(mut self, links: Vec<Link>) -> Self {
        self.links = Some(links);
        self
    }

    #[must_use]
    pub fn with_far(mut self, far: FarTuning) -> Self {
        self.far = far;
        self
    }

    /// Builds the canvas over a graph of its own.
    pub fn build(self) -> (CanvasLayer, SharedGraph) {
        let graph = share(GraphModel::generated(self.nodes));
        let canvas = self.over(&graph);
        (canvas, graph)
    }

    /// Builds a canvas over a graph that already exists (another view of it).
    pub fn over(self, graph: &SharedGraph) -> CanvasLayer {
        // Names rather than nodes: a graph that has been edited has holes, and the view
        // has to agree about them.
        let names = graph.borrow().names();
        let geometry = {
            let graph = graph.clone();
            move |i: usize| graph.borrow().try_node(i).map(|node| (node.pos, NODE_SIZE))
        };
        let source = GraphSource::new(graph.clone()).with_far_min_radius(self.far.min_radius_px);
        CanvasLayer::new(names, geometry, source)
            .with_controls_on_hover(self.controls_on_hover)
            // The graph's links unless the caller names its own, because topology is the
            // model's: two views over one graph must not hold two copies of it.
            .with_links(self.links.unwrap_or_else(|| graph.borrow().links().to_vec()))
            .with_far_overscan(self.far.overscan)
            .with_ports(crate::node::PORTS)
            .with_link_style(blazy::canvas::LinkStyle {
                min_screen_length: self.far.min_link_px,
                far_fill: self.far.fill_links,
                far_min_width_px: self.far.link_min_px,
                ..blazy::canvas::LinkStyle::default()
            })
    }
}

/// The canvas a window opens with: a generated graph and every default.
pub fn build_canvas(nodes: usize) -> (CanvasLayer, SharedGraph) {
    CanvasSpec::new(nodes).build()
}

/// Edges for a generated graph: each node wired to its neighbour on the right and
/// the one below.
///
/// Local edges on purpose. A node editor's graph is mostly local — that is what makes
/// it readable — and it is also the case the link layer's region selection is exact
/// for (`blazy-canvas::links`). Roughly two edges per node, so a 5000-node graph
/// carries about 10 000 of them.
pub fn generated_links(count: usize) -> Vec<Link> {
    let cols = model::GRID_COLS;
    let mut links = Vec::with_capacity(count * 2);
    for i in 0..count {
        if i % cols != cols - 1 && i + 1 < count {
            links.push(Link::new(i, i + 1));
        }
        if i + cols < count {
            links.push(Link::new(i, i + cols));
        }
    }
    links
}

/// The far-field knobs the measurements sweep.
///
/// Not a public API of the library — knobs of *this example*, so that the levers
/// `blazy-canvas` offers (the recorded margin) and the ones an application owns (how
/// coarsely it draws a node) can be priced in the same table.
#[derive(Clone, Copy, Debug)]
pub struct FarTuning {
    /// Fraction of the viewport recorded on each side. `CanvasLayer::with_far_overscan`.
    pub overscan: f64,
    /// Corner size on screen, in pixels, below which nodes are drawn as plain
    /// rectangles. Zero rounds always.
    pub min_radius_px: f64,
    /// `LinkStyle::min_screen_length`: how long a link must be on screen to be drawn.
    pub min_link_px: f64,
    /// `LinkStyle::far_fill`: links filled rather than stroked in the far field.
    pub fill_links: bool,
    /// `LinkStyle::far_min_width_px`: the narrowest a far-field link is on screen.
    pub link_min_px: f64,
}

impl Default for FarTuning {
    fn default() -> Self {
        Self {
            overscan: CanvasLayer::DEFAULT_FAR_OVERSCAN,
            min_radius_px: crate::node::FAR_MIN_RADIUS_PX,
            min_link_px: blazy::canvas::LinkStyle::default().min_screen_length,
            fill_links: blazy::canvas::LinkStyle::default().far_fill,
            link_min_px: blazy::canvas::LinkStyle::default().far_min_width_px,
        }
    }
}
