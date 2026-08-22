//! The interactive window for the Phase 0.5 area screen.
//!
//! ```text
//! cargo make run-area-screen
//! cargo make run-area-screen --areas 16 --nodes 20000
//! ```
//!
//! Drag a splitter to move a boundary; each area pans and zooms independently over
//! the same graph, and each area's header is drawn at its own interface scale. There
//! is no counter overlay here, unlike the node-canvas window: the questions this
//! experiment asks are about how many areas and regions re-lay-out, which is a number
//! you read off the benchmark rather than off the screen. See [`area_screen`] for
//! what is under test.

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use area_screen::{DEFAULT_AREAS, build_screen_staggered};
use clap::Parser;
use masonry::core::NewWidget;
use masonry::dpi::LogicalSize;
use masonry::theme::default_property_set;
use masonry_winit::app::{AppDriver, DriverCtx, NewWindow, WindowId};
use masonry_winit::winit::window::Window;
use node_canvas::DEFAULT_NODES;

#[derive(Parser)]
#[command(
    name = "area-screen",
    about = "Phase 0.5 area screen — the interactive window.",
    after_help = "The measurements are a separate target:\n    cargo bench -p area-screen -- --help"
)]
struct Args {
    /// Number of areas the window is tiled into.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_AREAS)]
    areas: usize,

    /// Number of nodes in the graph every area shows.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_NODES)]
    nodes: usize,

    /// Interface scale for every region header.
    ///
    /// Omit it and each area gets a different one, which is the point of the window:
    /// per-region `ui_scale` is easier to see than to read about.
    #[arg(long, value_name = "F")]
    ui_scale: Option<f64>,
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
        // Controls inside nodes submit actions; this experiment is about areas and
        // has nothing to do with them.
    }
}

fn main() {
    let args = Args::parse();
    let (screen, _graph) = build_screen_staggered(args.areas.max(1), args.nodes, args.ui_scale);

    let attributes = Window::default_attributes()
        .with_title(format!(
            "blazy - Phase 0.5 area screen ({} areas, {} nodes)",
            args.areas, args.nodes
        ))
        .with_resizable(true)
        .with_min_inner_size(LogicalSize::new(640.0, 480.0))
        .with_inner_size(LogicalSize::new(1400.0, 900.0));

    masonry_winit::app::run(
        vec![NewWindow::new(attributes, NewWidget::new(screen).erased())],
        Driver,
        default_property_set(),
    )
    .unwrap();
}
