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
//! cargo make bench-ci          # what CI runs, plus a JSON report
//! ```
//!
//! The benchmark is the deliverable. The window is there so the claims can be
//! checked by eye as well as by counter.
//!
//! This crate is a library so that the window (`src/main.rs`), the benchmark
//! (`benches/phase0.rs`) and the correctness tests can share one canvas
//! construction. Cargo bench targets are separate crates and can only reach a
//! package's library, so a binary-only layout would mean duplicating the graph
//! generator — the one thing every measurement depends on being identical.

pub mod editor;
pub mod model;
pub mod node;

#[cfg(test)]
mod tests;

use blazy_canvas::CanvasLayer;

use crate::model::{GraphModel, NODE_SIZE, SharedGraph, share};
use crate::node::GraphSource;

/// Default graph size. The figure comes straight from the Phase 0 brief.
pub const DEFAULT_NODES: usize = 5000;

/// Builds a virtualised canvas over a generated graph.
///
/// Only geometry is handed to the canvas up front. Widgets are built on demand by
/// the closure, which reads current state from the shared model — so a node that
/// scrolls out of view and back again comes back with the user's edits intact.
pub fn build_canvas(count: usize) -> (CanvasLayer, SharedGraph) {
    build_canvas_with(count, false)
}

/// As [`build_canvas`], with control-on-hover materialisation optionally enabled.
///
/// Off by default: at `Full` the painted stand-in does not resemble Masonry's themed
/// slider and checkbox closely enough, so swapping them in on hover reads as the
/// interface changing under the cursor. The benchmark keeps measuring both so the
/// price of that choice stays visible.
pub fn build_canvas_with(count: usize, controls_on_hover: bool) -> (CanvasLayer, SharedGraph) {
    let graph = share(GraphModel::generated(count));
    let geometry = {
        let graph = graph.clone();
        move |i: usize| (graph.borrow().node(i).pos, NODE_SIZE)
    };
    let source = GraphSource::new(graph.clone());
    let canvas = CanvasLayer::new(count, geometry, source).with_controls_on_hover(controls_on_hover);
    (canvas, graph)
}

/// The argument following `flag`, if the flag is present and has one.
///
/// Shared by the window and the benchmark, which both take `--nodes`. Hand-rolled
/// rather than via a parser crate: four flags between the two of them, and the
/// benchmark's dependencies are part of what it measures the cost of building.
pub fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).map(String::as_str)
}

/// The value of `--nodes`, or [`DEFAULT_NODES`].
pub fn node_count(args: &[String]) -> usize {
    flag_value(args, "--nodes")
        .and_then(|n| n.parse().ok())
        .unwrap_or(DEFAULT_NODES)
}
