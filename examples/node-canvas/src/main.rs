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
//! Two modes:
//!
//! ```text
//! cargo run -p node-canvas --release              # interactive window
//! cargo run -p node-canvas --release -- --bench   # headless measurements
//! ```
//!
//! The benchmark is the deliverable. The window is there so the claims can be
//! checked by eye as well as by counter.

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod bench;
mod editor;
mod model;
mod node;
#[cfg(test)]
mod tests;

use blazy_canvas::CanvasLayer;
use masonry::core::NewWidget;
use masonry::dpi::LogicalSize;
use masonry::theme::default_property_set;
use masonry_winit::app::{AppDriver, DriverCtx, NewWindow, WindowId};
use masonry_winit::winit::window::Window;

use crate::editor::NodeEditor;
use crate::model::{GraphModel, NODE_SIZE, SharedGraph, share};
use crate::node::GraphNode;

/// Default graph size. The figure comes straight from the Phase 0 brief.
pub const DEFAULT_NODES: usize = 5000;

/// Builds a virtualised canvas over a generated graph.
///
/// Only geometry is handed to the canvas up front. Widgets are built on demand by
/// the closure, which reads current state from the shared model — so a node that
/// scrolls out of view and back again comes back with the user's edits intact.
pub fn build_canvas(count: usize) -> (CanvasLayer, SharedGraph) {
    let graph = share(GraphModel::generated(count));
    let geometry = {
        let graph = graph.clone();
        move |i: usize| (graph.borrow().node(i).pos, NODE_SIZE)
    };
    let source = {
        let graph = graph.clone();
        move |i: usize| GraphNode::build(&graph, i)
    };
    (CanvasLayer::new(count, geometry, source), graph)
}

struct Driver;

impl AppDriver for Driver {
    fn on_action(
        &mut self,
        _window_id: WindowId,
        _ctx: &mut DriverCtx<'_, '_>,
        _widget_id: masonry::core::WidgetId,
        _action: masonry::core::ErasedAction,
    ) {
        // Sliders and checkboxes inside nodes submit actions. This experiment does
        // not need to act on them — that they arrive at all is claim 3 holding.
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--bench") {
        let count = parse_count(&args).unwrap_or(DEFAULT_NODES);
        bench::run(count);
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "Phase 0 node canvas\n\n\
             USAGE:\n    \
             node-canvas [--bench] [--nodes N]\n\n\
             OPTIONS:\n    \
             --bench       run headless measurements instead of opening a window\n    \
             --nodes N     number of nodes (default {DEFAULT_NODES})\n"
        );
        return;
    }

    let count = parse_count(&args).unwrap_or(DEFAULT_NODES);
    let (canvas, _graph) = build_canvas(count);
    let editor = NodeEditor::new(canvas);

    let window_size = LogicalSize::new(1100.0, 750.0);
    let attributes = Window::default_attributes()
        .with_title(format!("blazy - Phase 0 node canvas ({count} nodes)"))
        .with_resizable(true)
        .with_min_inner_size(LogicalSize::new(480.0, 320.0))
        .with_inner_size(window_size);

    masonry_winit::app::run(
        vec![NewWindow::new(attributes, NewWidget::new(editor).erased())],
        Driver,
        default_property_set(),
    )
    .unwrap();
}

fn parse_count(args: &[String]) -> Option<usize> {
    let idx = args.iter().position(|a| a == "--nodes")?;
    args.get(idx + 1)?.parse().ok()
}
