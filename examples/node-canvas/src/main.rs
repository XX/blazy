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

use blazy::canvas::{DEFAULT_WIDGET_BUDGET, DetailBudget};
use blazy::masonry::app::RenderRoot;
use blazy::masonry::core::NewWidget;
use blazy::shell::window::{ShellCtx, ShellDriver, WindowConfig, WindowKey, run};
use blazy::shell::{Backend, COMPILED};
use clap::Parser;
use node_canvas::{DEFAULT_NODES, build_canvas, property_set};

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

    let (canvas, graph) = build_canvas(args.nodes);
    let canvas = canvas.with_budget(DetailBudget {
        widgets: args.budget_widgets,
        ..Default::default()
    });
    // With the operator layer: click selects, left-drag on empty canvas boxes,
    // `G` grabs, `B` boxes, Escape cancels, Ctrl+Z undoes. The canvas's own
    // primary-button gestures step aside for the keymap; the middle button still pans
    // and the wheel still zooms.
    let editor = node_canvas::editor::with_ops(canvas, &graph);

    let config = WindowConfig::default()
        .with_title(format!("blazy - Phase 0 node canvas ({} nodes)", args.nodes))
        .with_size(1100.0, 750.0)
        .with_backend(backend);

    // The driver makes the editor the focus fallback, so the keymap hears keys.
    run(config, NewWidget::new(editor).erased(), property_set(), KeymapFocus).unwrap();
}

/// Makes the application's root widget the focus fallback, so the keymap hears keys.
struct KeymapFocus;

impl ShellDriver for KeymapFocus {
    fn started(&mut self, _cx: &mut ShellCtx, _window: WindowKey, root: &mut RenderRoot) {
        let id = root.get_layer_root(0).id();
        root.set_focus_fallback(Some(id));
    }
}
