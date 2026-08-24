//! The interactive window for the Phase 0 node canvas.
//!
//! ```text
//! cargo make run-node-canvas
//! cargo make run-node-canvas --nodes 20000
//! ```
//!
//! The measurements live in `benches/phase0/`; this binary exists so the claims they
//! make can be checked by eye as well as by counter. See [`node_canvas`] for what the
//! experiment is testing.

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use blazy_canvas::{DEFAULT_WIDGET_BUDGET, DetailBudget};
use blazy_shell::window::{WindowConfig, run};
use blazy_shell::{Backend, COMPILED};
use clap::Parser;
use masonry::core::NewWidget;
use masonry::theme::default_property_set;
use node_canvas::editor::NodeEditor;
use node_canvas::{DEFAULT_NODES, build_canvas};

#[derive(Parser)]
#[command(
    name = "node-canvas",
    about = "Phase 0 node canvas — the interactive window.",
    after_help = "The measurements are a separate target:\n    cargo bench -p node-canvas -- --help"
)]
struct Args {
    /// Number of nodes in the generated graph.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_NODES)]
    nodes: usize,

    #[arg(long, short = 'w', value_name = "W", default_value_t = DEFAULT_WIDGET_BUDGET)]
    budget_widgets: usize,

    /// Which rasteriser to draw with.
    ///
    /// The choice §16 item 1 asks for, and the reason this window goes through
    /// `blazy-shell` rather than through `masonry_winit`: upstream picks the backend
    /// with a cascade of `cfg` at compile time (§26.2).
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

    let (canvas, _graph) = build_canvas(args.nodes);
    let canvas = canvas.with_budget(DetailBudget {
        widgets: args.budget_widgets,
        ..Default::default()
    });
    let editor = NodeEditor::new(canvas);

    let config = WindowConfig::default()
        .with_title(format!("blazy - Phase 0 node canvas ({} nodes)", args.nodes))
        .with_size(1100.0, 750.0)
        .with_backend(backend);

    // Sliders and checkboxes inside nodes submit actions. This experiment does not
    // need to act on them — that they arrive at all is claim 3 holding.
    run(config, NewWidget::new(editor).erased(), default_property_set(), ()).unwrap();
}
