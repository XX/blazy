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
use blazy_shell::window::{WindowConfig, run};
use blazy_shell::{Backend, COMPILED};
use clap::Parser;
use masonry::core::NewWidget;
use masonry::theme::default_property_set;
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

    #[arg(long, short = 'w', value_name = "W")]
    budget_widgets: Option<usize>,

    /// Which rasteriser to draw with (§26.2).
    #[arg(long, value_name = "NAME")]
    backend: Option<String>,
}

fn main() {
    let args = Args::parse();
    let backend = args.backend.as_deref().map(|name| {
        Backend::from_name(name).unwrap_or_else(|| {
            let names: Vec<_> = COMPILED.iter().map(|backend| backend.name()).collect();
            panic!("unknown backend {name:?}; this build has {}", names.join(", "))
        })
    });

    let (screen, _graph) = build_screen_staggered(args.areas.max(1), args.nodes, args.budget_widgets, args.ui_scale);

    let config = WindowConfig::default()
        .with_title(format!(
            "blazy - Phase 0.5 area screen ({} areas, {} nodes)",
            args.areas, args.nodes
        ))
        .with_size(1400.0, 900.0)
        .with_backend(backend);

    // Controls inside nodes submit actions; this experiment is about areas and has
    // nothing to do with them.
    run(config, NewWidget::new(screen).erased(), default_property_set(), ()).unwrap();
}
