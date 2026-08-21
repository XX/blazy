//! The interactive window for the Phase 0 node canvas.
//!
//! ```text
//! cargo make run-node-canvas
//! cargo make run-node-canvas -- --nodes 20000
//! ```
//!
//! The measurements live in `benches/phase0.rs`; this binary exists so the claims
//! they make can be checked by eye as well as by counter. See [`node_canvas`] for
//! what the experiment is testing.

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use masonry::core::NewWidget;
use masonry::dpi::LogicalSize;
use masonry::theme::default_property_set;
use masonry_winit::app::{AppDriver, DriverCtx, NewWindow, WindowId};
use masonry_winit::winit::window::Window;
use node_canvas::editor::NodeEditor;
use node_canvas::{DEFAULT_NODES, build_canvas, node_count};

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

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "Phase 0 node canvas\n\n\
             USAGE:\n    \
             node-canvas [--nodes N]\n\n\
             OPTIONS:\n    \
             --nodes N    number of nodes (default {DEFAULT_NODES})\n\n\
             The measurements are a separate target:\n    \
             cargo bench -p node-canvas -- --help\n"
        );
        return;
    }

    let count = node_count(&args);
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
