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
//! | `N` | open another window over the same graph (§44) |
//! | `D` | move the area into a window of its own |
//! | `Ctrl+S` | write the workspace to `--workspace` |
//! | `Ctrl+O` | read it back |
//!
//! Every area carries the operator layer, so the graph itself is edited here the way it
//! is in the node-canvas window — left-drag a node to move it, `G`, `X`, `Shift+A`,
//! `Ctrl+Z` (§38.3). Without it an area holds a bare canvas and the graph cannot be
//! edited at all, which is what made the two windows look independent (§44.6).
//!
//! `J` is where the shape of the answer shows: it offers a join only with the area's
//! **sibling**, so two areas that plainly share a border refuse to merge whenever they
//! are cousins. That is not a bug in the key, it is what a binary tree can express, and
//! §41.1 counts what it costs.

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::path::PathBuf;

use area_screen::{AreaSession, DEFAULT_AREAS, HeaderScale, Screen, ScreenSpec, detach_area, sync_window};
use blazy::areas::{AreaId, AreaScreen, Workspace};
use blazy::masonry::app::RenderRoot;
use blazy::masonry::core::keyboard::{Key, KeyState, Modifiers};
use blazy::masonry::core::{Handled, NewWidget, TextEvent, WidgetId};
use blazy::masonry::kurbo::Axis;
use blazy::masonry::theme::default_property_set;
use blazy::shell::window::{ShellCtx, ShellDriver, WindowConfig, WindowKey, run};
use blazy::shell::{Backend, COMPILED};
use clap::Parser;
use node_canvas::DEFAULT_NODES;
use node_canvas::model::SharedGraph;

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

    /// How many windows to open over the same graph.
    ///
    /// The same path the `N` key takes, at startup: a second screen over the same models
    /// (§30). Here because a window cannot be opened from a test — there is no event loop
    /// — so this is how two windows are seen by eye and by log.
    #[arg(long, value_name = "N", default_value_t = 1)]
    windows: usize,

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
    /// The models every window shows. A second window is another screen over these, not
    /// another application (§30, and the detach task's phase 1).
    graph: SharedGraph,
    /// How a screen is built, so the second window is built like the first.
    spec: ScreenSpec,
    /// Windows still to open at startup (`--windows`).
    remaining: usize,
    /// Every window this driver knows about.
    ///
    /// One driver per process, so this is where "the other windows" is answered — and
    /// answering it is what a change made in one window needs (§30 across the boundary).
    windows: Vec<WindowKey>,
}

impl Areas {
    /// Runs `act` on the screen and on the area under the pointer.
    ///
    /// Every operation here needs both, and the screen is the window's root, so this is
    /// the whole of reaching them.
    fn on_hovered_area(
        root: &mut RenderRoot,
        act: impl FnOnce(&mut blazy::masonry::core::WidgetMut<'_, Screen>, AreaId),
    ) {
        root.edit_base_layer(|mut widget| {
            let mut screen = widget.downcast::<Screen>();
            let Some(area) = screen.widget.hovered_area() else {
                return;
            };
            act(&mut screen, area);
        });
    }

    /// The windows that have a change of somebody else's to collect.
    ///
    /// Split out from [`settled`](ShellDriver::settled) so that it can be tested: the hook
    /// is handed a `RenderRoot`, which a test cannot make, and the decision is the part
    /// that was wrong — the wake-up used to live in the key handler, so a change made with
    /// the mouse never woke anyone (§44.3).
    fn windows_to_wake(&self) -> Vec<WindowKey> {
        if !self.graph.borrow().has_pending() {
            return Vec::new();
        }
        self.windows.clone()
    }

    /// Asks for another window over the same models.
    ///
    /// The screen is built here and handed over as a fresh tree, because a widget tree
    /// cannot move between windows: `RenderRoot` owns its arena and upstream has no
    /// reparenting. What the windows share is the graph, which is where the truth is
    /// (§30).
    fn open_window(&self, cx: &mut ShellCtx) {
        self.open_window_with(cx, None);
    }

    /// The same, with the first area taking over a session that already exists.
    fn open_window_with(&self, cx: &mut ShellCtx, carried: Option<AreaSession>) {
        let detached = carried.is_some();
        let spec = if detached {
            // One area: what was detached, and nothing else.
            ScreenSpec::new(1, self.spec.nodes).with_ops(true)
        } else {
            self.spec.clone()
        };
        let screen = spec.over_with(&self.graph, carried);
        let config = WindowConfig::default()
            .with_title(if detached {
                "blazy - detached area"
            } else {
                "blazy - another window, same graph"
            })
            .with_size(1000.0, 700.0);
        let window = cx.open_window(config, NewWidget::new(screen).erased());
        tracing::info!(?window, "window asked for");
    }

    fn save(&self, root: &mut RenderRoot) {
        let text = root.edit_base_layer(|mut widget| {
            let screen = widget.downcast::<Screen>();
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
                    let mut screen = widget.downcast::<Screen>();
                    AreaScreen::set_tree(&mut screen, workspace.tree().clone());
                });
                tracing::info!(path = %self.workspace.display(), "workspace read");
            },
            Err(error) => tracing::warn!(path = %self.workspace.display(), "workspace not read: {error}"),
        }
    }
}

impl ShellDriver for Areas {
    /// Opens the windows `--windows` asked for, one per window that starts.
    ///
    /// Each new window comes back here, so the count walks down to zero; the path is the
    /// one the `N` key takes, which is the point of doing it this way rather than in a
    /// loop of our own.
    fn started(&mut self, cx: &mut ShellCtx, window: WindowKey, _root: &mut RenderRoot) {
        self.windows.push(window);
        if self.remaining == 0 {
            return;
        }
        self.remaining -= 1;
        self.open_window(cx);
    }

    /// Catches this window up with what another one changed.
    ///
    /// The pull half of §30: the push reaches the areas of one window and is dropped
    /// across the boundary of two, so the model keeps what each view still owes and the
    /// window collects it when it draws.
    fn frame(&mut self, _window: WindowKey, root: &mut RenderRoot) {
        if self.graph.borrow().has_pending() {
            sync_window(root, &self.graph);
        }
    }

    fn layers(&mut self, _window: WindowKey, root: &mut RenderRoot) -> Vec<WidgetId> {
        if !self.enabled {
            return Vec::new();
        }
        // The base layer is the window's own root, which is the screen.
        root.edit_base_layer(|mut widget| widget.downcast::<Screen>().widget.area_ids())
    }

    /// The area operations, from the seat in front of the tree.
    ///
    /// Here rather than in a widget because they are the *screen's* operations and the
    /// screen is the root: there is nothing above it to bubble to. A real application
    /// would bind these in a keymap (§11) and this would be its driver; the window is a
    /// spike, so the bindings are written out.
    fn text_event(
        &mut self,
        cx: &mut ShellCtx,
        _window: WindowKey,
        root: &mut RenderRoot,
        event: &TextEvent,
    ) -> Handled {
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

        self.on_key(cx, root, name.as_str(), ctrl)
    }

    /// Wakes the windows that have a change of somebody else's to collect.
    ///
    /// Here rather than in the key or pointer seats, and that is the whole point of
    /// `settled`: a seat is offered an event before the tree acts on it, so a change made
    /// with the mouse — a drag, a box select — would be noticed one gesture late. This
    /// runs after the event, whatever the event was.
    ///
    /// Only when something is actually owed, because waking an idle window on every mouse
    /// move would undo §36.
    fn settled(&mut self, cx: &mut ShellCtx, _window: WindowKey, _root: &mut RenderRoot) {
        for window in self.windows_to_wake() {
            cx.request_redraw(window);
        }
    }
}

impl Areas {
    /// One key, and what it does to the screen.
    fn on_key(&mut self, cx: &mut ShellCtx, root: &mut RenderRoot, name: &str, ctrl: bool) -> Handled {
        match (name, ctrl) {
            // A second window over the same models. The screen is built here and handed
            // to the shell as a fresh tree, because a widget tree cannot move between
            // windows: `RenderRoot` owns its arena and upstream has no reparenting.
            ("n", false) => self.open_window(cx),
            // Detach: the area under the pointer moves into a window of its own. The
            // widget is rebuilt there — a tree cannot cross a `RenderRoot` — and the
            // session goes as it is, so the new window opens where the old area was
            // looking, with the same selection and the same history (decision 1).
            ("d", false) => {
                let taken = root.edit_base_layer(|mut widget| {
                    let mut screen = widget.downcast::<Screen>();
                    let area = screen.widget.hovered_area()?;
                    detach_area(&mut screen, area)
                });
                match taken {
                    Some(session) => self.open_window_with(cx, Some(session)),
                    // The last area of a window does not detach (decision 5), exactly as
                    // an area with no sibling does not join (§41.1).
                    None => tracing::info!("nothing to detach here; see decision 5"),
                }
            },
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
                let mut screen = widget.downcast::<Screen>();
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
    // With the operator layer, because without it an area holds a bare canvas and the
    // graph cannot be edited at all: `node.move` is bound on the primary drag and lives
    // in the editor (§38.3). The window was built without it, and that is how a drag in
    // one window came to change nothing anywhere — the report read as "the two windows
    // are independent", and neither one was following the model (§44.3).
    let spec = ScreenSpec::new(args.areas.max(1), args.nodes)
        .with_budget(args.budget_widgets)
        .with_header_scale(args.ui_scale.map_or(HeaderScale::Staggered, HeaderScale::Forced))
        .with_isolated_layers(layers)
        .with_ops(true);
    let (screen, graph) = spec.clone().build();

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
        graph,
        spec,
        remaining: args.windows.saturating_sub(1),
        windows: Vec::new(),
    })
    .unwrap();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use blazy::areas::AreaContent;
    use blazy::masonry::app::{RenderRootOptions, WindowSizePolicy};
    use blazy::masonry::core::WidgetId;
    use blazy::masonry::dpi::PhysicalSize;
    use blazy::masonry::kurbo::Point;
    use blazy::ops::keymap::Props;
    use node_canvas::editor::NodeEditor;
    use node_canvas::model::{GraphModel, share};

    use super::*;

    /// The driver, with two windows known and nothing else set up.
    fn driver(graph: &SharedGraph) -> Areas {
        // Named through a context, because that is the only thing that hands out window
        // keys — and two windows have to be two different ones.
        let mut cx = ShellCtx::new();
        let windows = vec![cx.name_window(), cx.name_window()];
        Areas {
            enabled: false,
            workspace: PathBuf::from("target/test-workspace.blazy"),
            graph: graph.clone(),
            spec: ScreenSpec::new(2, 8),
            remaining: 0,
            windows,
        }
    }

    /// A change anywhere wakes every window, and a quiet model wakes none.
    ///
    /// The decision the mouse path used to miss: an idle window collects a change on its
    /// next frame, and without this there is no next frame (§36, §44.3).
    #[test]
    fn a_pending_change_wakes_the_windows_and_nothing_else_does() {
        let graph = share(GraphModel::generated(8));
        let driver = driver(&graph);
        // A real id, because `WidgetId` cannot be made up: a view is a canvas, and this
        // is the cheapest widget the example has.
        let view = area_screen::area_header(0).to_pod().id();
        graph.borrow_mut().register_view(view);
        assert!(
            driver.windows_to_wake().is_empty(),
            "a view that has seen everything asks for nothing"
        );

        graph.borrow_mut().set_pos(0, Point::new(5.0, 5.0));
        assert_eq!(driver.windows_to_wake().len(), 2, "both windows have to follow it");

        // Once the view has collected it, the windows go quiet again.
        let _ = graph.borrow_mut().take_pending(view);
        assert!(driver.windows_to_wake().is_empty());
    }
    /// Two windows over one graph, driven the way the shell drives them.
    ///
    /// Not a harness: the application's path goes through `RenderRoot`, and a harness does
    /// not hand one out (§39.5). This is what the shell does — deliver the change, call
    /// `settled`, draw the other window, which calls `frame` — and it is the test that was
    /// missing when a change made with the mouse failed to cross.
    #[test]
    fn a_change_in_one_window_reaches_the_other_through_the_driver() {
        let graph = share(GraphModel::generated(60));
        let mut first = root_over(&graph);
        let mut second = root_over(&graph);
        let mut cx = ShellCtx::new();
        let (key_first, key_second) = (cx.name_window(), cx.name_window());
        let mut areas = Areas {
            enabled: false,
            workspace: PathBuf::from("target/test-workspace.blazy"),
            graph: graph.clone(),
            spec: ScreenSpec::new(2, 60),
            remaining: 0,
            windows: vec![key_first, key_second],
        };

        // A change in the second window, through the operators, as a gesture would.
        let editor = editor_of(&mut second, 0);
        second.edit_widget(editor, |mut widget| {
            let mut editor = widget.downcast::<NodeEditor>();
            NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 0));
            NodeEditor::exec(&mut editor, "node.move", &Props::new().with_float("dx", 40.0));
        });
        let _ = second.redraw();
        let truth = graph.borrow().node(0).pos;

        // The shell asks the driver what to do once the event is over.
        areas.settled(&mut cx, key_second, &mut second);
        let woken: Vec<_> = cx.drain();
        assert!(!woken.is_empty(), "the driver asked for the other window to draw");

        // And the other window draws, which is where it collects the change.
        assert_ne!(canvas_pos(&mut first, 0), Some(truth), "before its frame it is behind");
        areas.frame(key_first, &mut first);
        let _ = first.redraw();
        assert_eq!(
            canvas_pos(&mut first, 0),
            Some(truth),
            "after its frame it shows what the model says"
        );
    }

    /// An area with no operator layer collects its changes too.
    ///
    /// The defect this pins down: [`sync_window`] looked only for a `NodeEditor`, and an
    /// area without one holds the canvas itself, so the pull ran on every frame and
    /// applied nothing — silently, because there is nothing there to fail. The window was
    /// built that way, which is what "the two windows are independent" was (§44.3). The
    /// window has the operator layer now, and this shape stays: `ScreenSpec` builds it and
    /// the area benchmarks measure it.
    #[test]
    fn an_area_without_the_operator_layer_collects_its_changes() {
        let graph = share(GraphModel::generated(60));
        let mut root = root_of(ScreenSpec::new(2, 60), &graph);

        // Straight into the model, because there are no operators here to do it — which
        // is the point: the pull is what carries a change into a view, whatever made it.
        let moved = Point::new(40.0, 40.0);
        graph.borrow_mut().set_pos(0, moved);
        assert_ne!(
            bare_canvas_pos(&mut root, 0),
            Some(moved),
            "before the pull it is behind"
        );

        let applied = sync_window(&mut root, &graph);
        assert_eq!(applied, 2, "both areas of this window are views of the graph");
        assert_eq!(bare_canvas_pos(&mut root, 0), Some(moved));
        assert_eq!(bare_canvas_pos(&mut root, 1), Some(moved), "and so is the other area");

        // And nothing is owed twice: a second pull has nothing to carry.
        assert_eq!(sync_window(&mut root, &graph), 0);
    }

    /// Where one area of a window thinks node 0 is, with no editor in the way.
    fn bare_canvas_pos(root: &mut RenderRoot, area: usize) -> Option<Point> {
        let canvas = editor_of(root, area);
        root.edit_widget(canvas, |mut widget| {
            let mut canvas = widget.downcast::<blazy::canvas::CanvasLayer>();
            blazy::canvas::CanvasLayer::child_pos(&mut canvas, 0)
        })
    }

    /// A `RenderRoot` over a screen of two areas: what a window is, minus the window.
    fn root_over(graph: &SharedGraph) -> RenderRoot {
        root_of(ScreenSpec::new(2, 60).with_ops(true), graph)
    }

    /// The same, over a screen built to the spec given.
    ///
    /// Taken apart from [`root_over`] for one reason: the pull has to work for an area
    /// built *without* the operator layer as well, and that is the shape the window used
    /// to build.
    fn root_of(spec: ScreenSpec, graph: &SharedGraph) -> RenderRoot {
        let screen = spec.over(graph);
        let mut root = RenderRoot::new(NewWidget::new(screen).erased(), |_signal| {}, RenderRootOptions {
            default_properties: Arc::new(default_property_set()),
            use_system_fonts: false,
            size_policy: WindowSizePolicy::User,
            size: PhysicalSize::new(1400, 900),
            scale_factor: 1.0,
            test_font: None,
        });
        let _ = root.redraw();
        root
    }

    /// The editor of one area of a window.
    fn editor_of(root: &mut RenderRoot, area: usize) -> WidgetId {
        let areas: Vec<WidgetId> = root.edit_base_layer(|mut widget| widget.downcast::<Screen>().widget.area_ids());
        let content = root
            .get_widget(areas[area])
            .and_then(|widget| widget.downcast::<AreaContent>())
            .expect("an area holds a region stack");
        *content.region_ids().last().expect("an area has regions")
    }

    /// Where one window thinks node 0 is.
    fn canvas_pos(root: &mut RenderRoot, area: usize) -> Option<Point> {
        let editor = editor_of(root, area);
        root.edit_widget(editor, |mut widget| {
            let mut editor = widget.downcast::<NodeEditor>();
            NodeEditor::with_canvas(&mut editor, |mut canvas| {
                blazy::canvas::CanvasLayer::child_pos(&mut canvas, 0)
            })
        })
    }
}
