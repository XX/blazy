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
//!
//! # Operations on areas (§41)
//!
//! All of them act on the area **under the pointer**, which the screen publishes
//! because a driver above it cannot work it out (§38.3, and the same reason).
//!
//! | key | what it does |
//! |---|---|
//! | `X` | split the area left/right |
//! | `Y` | split it top/bottom |
//! | `J` | join it with its sibling — it survives, the sibling goes |
//! | `W` | swap it with its sibling |
//! | `M` | show it alone, or bring the screen back |
//! | `Ctrl+S` | write the workspace to `--workspace` |
//! | `Ctrl+O` | read it back |
//!
//! `J` is where the shape of the answer shows: it offers a join only with the area's
//! **sibling**, so two areas that plainly share a border refuse to merge whenever they
//! are cousins. That is not a bug in the key, it is what a binary tree can express, and
//! §41.1 counts what it costs.

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::path::PathBuf;

use area_screen::{DEFAULT_AREAS, HeaderScale, ScreenSpec};
use blazy_areas::{AreaId, AreaScreen, Workspace};
use blazy_shell::window::{ShellDriver, WindowConfig, run};
use blazy_shell::{Backend, COMPILED};
use clap::Parser;
use masonry::app::RenderRoot;
use masonry::core::keyboard::{Key, KeyState, Modifiers};
use masonry::core::{Handled, NewWidget, TextEvent, WidgetId};
use masonry::kurbo::Axis;
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

    /// Where `Ctrl+S` writes the workspace and `Ctrl+O` reads it back.
    #[arg(long, value_name = "PATH", default_value = "target/workspace.blazy")]
    workspace: PathBuf,

    /// Draw every frame from scratch instead of keeping idle areas' pixels (§36).
    ///
    /// The cache is on by default because it is what makes eight areas over one graph
    /// affordable: an area nobody is touching costs a texture copy instead of a
    /// rasterisation. Turning it off is how the difference is seen by eye.
    #[arg(long)]
    no_layer_cache: bool,
}

/// Tells the shell which subtrees are layers: every area (§36).
///
/// Two things at once, and deliberately so — the shell asks these widgets to repaint,
/// without which their layers vanish (§26.1), and tells the presenter it may keep their
/// pixels. Asking the tree every frame rather than remembering ids is what keeps this
/// correct across a split or a join.
struct Areas {
    enabled: bool,
    /// Where the workspace is written and read.
    workspace: PathBuf,
}

impl Areas {
    /// Runs `act` on the screen and on the area under the pointer.
    ///
    /// Every operation here needs both, and the screen is the window's root, so this is
    /// the whole of reaching them.
    fn on_hovered_area(root: &mut RenderRoot, act: impl FnOnce(&mut masonry::core::WidgetMut<'_, AreaScreen>, AreaId)) {
        root.edit_base_layer(|mut widget| {
            let mut screen = widget.downcast::<AreaScreen>();
            let Some(area) = screen.widget.hovered_area() else {
                return;
            };
            act(&mut screen, area);
        });
    }

    fn save(&self, root: &mut RenderRoot) {
        let text = root.edit_base_layer(|mut widget| {
            let screen = widget.downcast::<AreaScreen>();
            // The tree is the geometry; what fills each area is this application's to
            // say, and here every area is the same kind of editor (§41.5).
            let mut workspace = Workspace::new(screen.widget.tree().clone());
            for area in screen.widget.tree().areas() {
                workspace.set_content(area, "node-editor");
            }
            workspace.write()
        });
        match std::fs::write(&self.workspace, text) {
            Ok(()) => tracing::info!(path = %self.workspace.display(), "workspace written"),
            Err(error) => tracing::warn!(path = %self.workspace.display(), "workspace not written: {error}"),
        }
    }

    fn load(&self, root: &mut RenderRoot) {
        let text = match std::fs::read_to_string(&self.workspace) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(path = %self.workspace.display(), "workspace not read: {error}");
                return;
            },
        };
        match Workspace::parse(&text) {
            Ok(workspace) => {
                root.edit_base_layer(|mut widget| {
                    let mut screen = widget.downcast::<AreaScreen>();
                    AreaScreen::set_tree(&mut screen, workspace.tree().clone());
                });
                tracing::info!(path = %self.workspace.display(), "workspace read");
            },
            Err(error) => tracing::warn!(path = %self.workspace.display(), "workspace not read: {error}"),
        }
    }
}

impl ShellDriver for Areas {
    fn layers(&mut self, root: &mut RenderRoot) -> Vec<WidgetId> {
        if !self.enabled {
            return Vec::new();
        }
        // The base layer is the window's own root, which is the screen.
        root.edit_base_layer(|mut widget| widget.downcast::<AreaScreen>().widget.area_ids())
    }

    /// The area operations, from the seat in front of the tree.
    ///
    /// Here rather than in a widget because they are the *screen's* operations and the
    /// screen is the root: there is nothing above it to bubble to. A real application
    /// would bind these in a keymap (§11) and this would be its driver; the window is a
    /// spike, so the bindings are written out.
    fn text_event(&mut self, root: &mut RenderRoot, event: &TextEvent) -> Handled {
        let TextEvent::Keyboard(key) = event else {
            return Handled::No;
        };
        if key.state != KeyState::Down {
            return Handled::No;
        }
        let ctrl = key.modifiers.contains(Modifiers::CONTROL);
        let Key::Character(name) = &key.key else {
            return Handled::No;
        };

        match (name.as_str(), ctrl) {
            ("s", true) => self.save(root),
            ("o", true) => self.load(root),
            ("x", false) => Self::on_hovered_area(root, |screen, area| {
                AreaScreen::split(screen, area, Axis::Horizontal, 0.5);
            }),
            ("y", false) => Self::on_hovered_area(root, |screen, area| {
                AreaScreen::split(screen, area, Axis::Vertical, 0.5);
            }),
            ("j", false) => Self::on_hovered_area(root, |screen, area| {
                // The survivor is the one under the pointer, as in Blender. If it has no
                // sibling — because its sibling is a split rather than a leaf — nothing
                // happens, and that is the finding rather than a failure (§41.1).
                match screen.widget.tree().joinable(area) {
                    Some(sibling) => {
                        AreaScreen::join(screen, area, sibling);
                    },
                    None => tracing::info!(area, "no sibling to join with; see §41.1"),
                }
            }),
            ("w", false) => Self::on_hovered_area(root, |screen, area| match screen.widget.tree().joinable(area) {
                Some(sibling) => {
                    AreaScreen::swap(screen, area, sibling);
                },
                None => tracing::info!(area, "no sibling to swap with"),
            }),
            ("m", false) => root.edit_base_layer(|mut widget| {
                let mut screen = widget.downcast::<AreaScreen>();
                match screen.widget.tree().maximized() {
                    Some(_) => {
                        AreaScreen::restore(&mut screen);
                    },
                    None => {
                        if let Some(area) = screen.widget.hovered_area() {
                            AreaScreen::maximize(&mut screen, area);
                        }
                    },
                }
            }),
            _ => return Handled::No,
        }
        Handled::Yes
    }
}

fn main() {
    let args = Args::parse();
    let backend = args.backend.as_deref().map(|name| {
        Backend::from_name(name).unwrap_or_else(|| {
            let names: Vec<_> = COMPILED.iter().map(|backend| backend.name()).collect();
            panic!("unknown backend {name:?}; this build has {}", names.join(", "))
        })
    });

    let layers = !args.no_layer_cache;
    let (screen, _graph) = ScreenSpec::new(args.areas.max(1), args.nodes)
        .with_budget(args.budget_widgets)
        .with_header_scale(args.ui_scale.map_or(HeaderScale::Staggered, HeaderScale::Forced))
        .with_isolated_layers(layers)
        .build();

    let config = WindowConfig::default()
        .with_title(format!(
            "blazy - Phase 0.5 area screen ({} areas, {} nodes)",
            args.areas, args.nodes
        ))
        .with_size(1400.0, 900.0)
        .with_backend(backend);

    // Controls inside nodes submit actions; this experiment is about areas and has
    // nothing to do with them. What the driver is here for is layers (§36).
    run(config, NewWidget::new(screen).erased(), default_property_set(), Areas {
        enabled: layers,
        workspace: args.workspace,
    })
    .unwrap();
}
