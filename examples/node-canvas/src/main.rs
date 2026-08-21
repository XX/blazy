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
//! cargo make run-node-canvas                     # interactive window
//! cargo make bench                               # headless measurements
//! cargo make bench-ci                            # what CI runs, plus a JSON report
//! ```
//!
//! The benchmark is the deliverable. The window is there so the claims can be
//! checked by eye as well as by counter.
//!
//! `--bench` exits non-zero if any criterion fails, so the claims above are a CI
//! gate rather than a paragraph someone has to remember to re-read. What is gated
//! and what is merely reported is argued in [`criteria`].

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod bench;
mod criteria;
mod editor;
mod model;
mod node;
#[cfg(test)]
mod tests;

use std::fs;
use std::process::ExitCode;

use blazy_canvas::CanvasLayer;
use masonry::core::NewWidget;
use masonry::dpi::LogicalSize;
use masonry::theme::default_property_set;
use masonry_winit::app::{AppDriver, DriverCtx, NewWindow, WindowId};
use masonry_winit::winit::window::Window;

use crate::editor::NodeEditor;
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "Phase 0 node canvas\n\n\
             USAGE:\n    \
             node-canvas [--bench [--quick] [--report FILE]] [--nodes N]\n\n\
             OPTIONS:\n    \
             --bench          run headless measurements instead of opening a window;\n                     \
             exits non-zero if a Phase 0 criterion fails\n    \
             --quick          with --bench, run only the scenarios the criteria are\n                     \
             decided on; same verdict, fewer numbers\n    \
             --report FILE    with --bench, also write the run as JSON to FILE\n    \
             --nodes N        number of nodes (default {DEFAULT_NODES})\n"
        );
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--bench") {
        return run_bench(&args);
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

    ExitCode::SUCCESS
}

/// Runs the measurements and turns the verdict into an exit code.
fn run_bench(args: &[String]) -> ExitCode {
    let opts = bench::Options {
        count: parse_count(args).unwrap_or(DEFAULT_NODES),
        quick: args.iter().any(|a| a == "--quick"),
    };

    let outcome = bench::run(&opts);

    // Written after the report is printed, so a failure to write cannot cost us the
    // numbers — the console output is the primary record and the file is the archive.
    if let Some(path) = parse_flag(args, "--report") {
        match fs::write(path, outcome.to_json()) {
            Ok(()) => println!("report written to {path}"),
            // Worth failing the run over: a CI job asked for an artifact and did not
            // get one, and a silently missing report is discovered weeks later when
            // someone goes looking for the history.
            Err(err) => {
                eprintln!("could not write report to {path}: {err}");
                return ExitCode::FAILURE;
            },
        }
    }

    if outcome.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn parse_count(args: &[String]) -> Option<usize> {
    parse_flag(args, "--nodes")?.parse().ok()
}

/// The argument following `flag`, if the flag is present and has one.
fn parse_flag<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).map(String::as_str)
}
