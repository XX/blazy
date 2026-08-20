// Copyright 2026 the blazy Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

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
mod node;

use blazy_canvas::{CanvasItem, CanvasLayer};
use masonry::core::NewWidget;
use masonry::dpi::LogicalSize;
use masonry::kurbo::{Point, Size};
use masonry::peniko::Color;
use masonry::theme::default_property_set;
use masonry_winit::app::{AppDriver, DriverCtx, NewWindow, WindowId};
use masonry_winit::winit::window::Window;

use crate::editor::NodeEditor;
use crate::node::GraphNode;

/// Default graph size. The figure comes straight from the Phase 0 brief.
pub const DEFAULT_NODES: usize = 5000;

/// Node footprint in canvas units.
pub const NODE_SIZE: Size = Size::new(160.0, 96.0);

/// Spacing between nodes in the generated grid.
const GRID_STEP: f64 = 220.0;
/// Nodes per row in the generated grid.
const GRID_COLS: usize = 80;

/// Builds a deterministic grid of nodes.
///
/// Deterministic on purpose: two benchmark runs must be comparable, so there is no
/// randomness anywhere. The jitter is a cheap hash of the index, not an RNG.
pub fn build_canvas(count: usize) -> CanvasLayer {
    let items = (0..count).map(|i| {
        let col = i % GRID_COLS;
        let row = i / GRID_COLS;

        // A reproducible pseudo-random offset, so the grid does not look like graph
        // paper while staying identical between runs.
        let h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let jitter_x = ((h >> 33) % 61) as f64 - 30.0;
        let jitter_y = ((h >> 17) % 41) as f64 - 20.0;

        let pos = Point::new(col as f64 * GRID_STEP + jitter_x, row as f64 * GRID_STEP + jitter_y);

        let hue = (i % 6) as u8;
        let tint = match hue {
            0 => Color::from_rgb8(0x6b, 0x4b, 0x8a),
            1 => Color::from_rgb8(0x3c, 0x6e, 0x71),
            2 => Color::from_rgb8(0x8a, 0x5a, 0x3c),
            3 => Color::from_rgb8(0x44, 0x6b, 0x3c),
            4 => Color::from_rgb8(0x8a, 0x3c, 0x51),
            _ => Color::from_rgb8(0x3c, 0x4e, 0x8a),
        };

        let value = ((h >> 5) % 100) as f64 / 100.0;
        let checked = h & 1 == 0;

        CanvasItem::new(NewWidget::new(GraphNode::new(tint, value, checked)), pos, NODE_SIZE)
    });

    CanvasLayer::new(items)
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
    let editor = NodeEditor::new(build_canvas(count));

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
