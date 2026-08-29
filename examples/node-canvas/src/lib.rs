//! Phase 0 feasibility experiment for blazy.
//!
//! `rnd/architecture.md` §16 says: before committing to Masonry, build a node
//! canvas of 5000 nodes on `masonry_core@main` — per-child `Affine`, culling, LOD,
//! ordinary sliders and checkboxes inside the nodes — and measure it. The pass
//! criteria are:
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
//! The benchmark is the deliverable. The window is there so the claims can be
//! checked by eye as well as by counter.
//!
//! This crate is a library so that the window (`src/main.rs`), the benchmark
//! (`benches/phase0/`) and the correctness tests can share one canvas construction.
//! Cargo bench targets are separate crates and can only reach a package's library,
//! so a binary-only layout would mean duplicating the graph generator — the one
//! thing every measurement depends on being identical.

pub mod editor;
pub mod model;
pub mod node;
pub mod ops;

#[cfg(test)]
mod ops_tests;
#[cfg(test)]
mod tests;

use blazy_canvas::{CanvasLayer, Link};

use crate::model::{GraphModel, NODE_SIZE, SharedGraph, share};
use crate::node::GraphSource;

/// Default graph size. The figure comes straight from the Phase 0 brief.
pub const DEFAULT_NODES: usize = 5000;

/// How a canvas is put together for a window, a test or a measurement.
///
/// One description with defaults rather than a constructor per combination: every knob
/// this example grew — controls on hover, an explicit edge set, the far-field levers of
/// §35 — used to add another `build_canvas_*`, and the combinations multiply while the
/// bodies stay the same three lines.
#[derive(Clone, Debug)]
pub struct CanvasSpec {
    /// Nodes in the generated graph.
    pub nodes: usize,
    /// Edges: the generated ones unless an exact set is given (the link sweep gives one).
    pub links: Option<Vec<Link>>,
    /// Whether only the node under the pointer gets interactive controls.
    ///
    /// Off by default: at `Full` the painted stand-in does not resemble Masonry's themed
    /// slider and checkbox closely enough, so swapping them in on hover reads as the
    /// interface changing under the cursor. The benchmark keeps measuring both so the
    /// price of that choice stays visible.
    pub controls_on_hover: bool,
    /// The far-field knobs (§35.2).
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
    ///
    /// Only geometry is handed to the canvas up front. Widgets are built on demand by
    /// the source, which reads current state from the shared model — so a node that
    /// scrolls out of view and back again comes back with the user's edits intact.
    pub fn build(self) -> (CanvasLayer, SharedGraph) {
        let graph = share(GraphModel::generated(self.nodes));
        let canvas = self.over(&graph);
        (canvas, graph)
    }

    /// Builds a canvas over a graph that already exists.
    ///
    /// Several views of one model is the normal arrangement — Blender shows the same
    /// scene in several editors at once — and it is the arrangement the area
    /// measurements need: comparing one area against sixteen only means something if
    /// all sixteen are looking at the same graph rather than sixteen graphs of their own.
    pub fn over(self, graph: &SharedGraph) -> CanvasLayer {
        let nodes = self.nodes;
        let geometry = {
            let graph = graph.clone();
            move |i: usize| (graph.borrow().node(i).pos, NODE_SIZE)
        };
        let source = GraphSource::new(graph.clone()).with_far_min_radius(self.far.min_radius_px);
        CanvasLayer::new(nodes, geometry, source)
            .with_controls_on_hover(self.controls_on_hover)
            .with_links(self.links.unwrap_or_else(|| generated_links(nodes)))
            .with_far_overscan(self.far.overscan)
            .with_link_style(blazy_canvas::LinkStyle {
                min_screen_length: self.far.min_link_px,
                ..blazy_canvas::LinkStyle::default()
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

/// The far-field knobs the §35 measurements sweep.
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
}

impl Default for FarTuning {
    fn default() -> Self {
        Self {
            overscan: CanvasLayer::DEFAULT_FAR_OVERSCAN,
            min_radius_px: crate::node::FAR_MIN_RADIUS_PX,
            min_link_px: blazy_canvas::LinkStyle::default().min_screen_length,
        }
    }
}
