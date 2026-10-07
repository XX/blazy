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
//! The window is `blazy::app::EditorApp` over this example's graph and areas: what an
//! application writes is the graph, the nodes and what fills an area, and this file is
//! the last of those plus its flags.
//!
//! # Operations on areas (§41)
//!
//! All of them act on the area **under the pointer**, which the screen publishes
//! because a driver above it cannot work it out (§38.3, and the same reason). They are
//! `ScreenKeys::default()`, which holds a modifier on every one of them: the screen hears
//! a key before the editor does, and a plain letter here is a letter the editor never
//! gets.
//!
//! | key | what it does |
//! |---|---|
//! | `Alt+X` | split the area left/right |
//! | `Alt+Y` | split it top/bottom |
//! | `Alt+J` | join it with its sibling — it survives, the sibling goes |
//! | `Alt+W` | swap it with its sibling |
//! | `Ctrl+Space` | show it alone, or bring the screen back |
//! | `Alt+N` | open another window over the same graph (§44) |
//! | `Alt+D` | move the area into a window of its own |
//! | `Ctrl+S` | write the workspace to `--workspace` |
//! | `Ctrl+O` | read it back |
//!
//! Every one of them, and every key of the editor, can be rebound with `--keymap FILE`;
//! `--write-keymap FILE` writes the defaults as a starting point.
//!
//! Every area carries the operator layer, so the graph itself is edited here the way it
//! is in the node-canvas window — left-drag a node to move it, `G`, `X`, `F`, `Shift+A`,
//! `Ctrl+Z` (§38.3).
//!
//! `Alt+J` is where the shape of the answer shows: it offers a join only with the area's
//! **sibling**, so two areas that plainly share a border refuse to merge whenever they
//! are cousins. That is not a bug in the key, it is what a binary tree can express, and
//! §41.1 counts what it costs.

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::path::PathBuf;

use area_screen::{DEFAULT_AREAS, HeaderScale, ScreenSpec};
use blazy::app::EditorApp;
use blazy::areas::SplitTree;
use blazy::shell::window::WindowConfig;
use blazy::shell::{Backend, COMPILED};
use clap::Parser;
use node_canvas::DEFAULT_NODES;
use node_canvas::model::{GraphModel, share};

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
    /// The same path `Alt+N` takes, at startup: a second screen over the same models
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

    /// A file of keymap overrides, laid over the defaults (`bind` and `unbind` lines).
    ///
    /// One file for the editors and the screen: its `canvas` and `window` contexts are
    /// the editor's, its `screen` context the splits, joins and windows above.
    #[arg(long, value_name = "PATH")]
    keymap: Option<PathBuf>,

    /// Write the default keymap to PATH and exit: the starting point for `--keymap`.
    #[arg(long, value_name = "PATH")]
    write_keymap: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();
    if let Some(path) = &args.write_keymap {
        std::fs::write(path, blazy::app::default_keymap().write()).expect("the keymap could not be written");
        return;
    }
    let backend = args.backend.as_deref().map(|name| {
        Backend::from_name(name).unwrap_or_else(|| {
            let names: Vec<_> = COMPILED.iter().map(|backend| backend.name()).collect();
            panic!("unknown backend {name:?}; this build has {}", names.join(", "))
        })
    });

    let layers = !args.no_layer_cache;
    let areas = args.areas.max(1);
    // With the operator layer, because without it an area holds a bare canvas and the
    // graph cannot be edited at all: `node.move` is bound on the primary drag and lives
    // in the editor (§38.3). The window was built without it, and that is how a drag in
    // one window came to change nothing anywhere (§44.3).
    let spec = ScreenSpec::new(areas, args.nodes)
        .with_budget(args.budget_widgets)
        .with_header_scale(args.ui_scale.map_or(HeaderScale::Staggered, HeaderScale::Forced))
        .with_isolated_layers(layers)
        .with_ops(true);
    let graph = share(GraphModel::generated(args.nodes));

    let config = WindowConfig::default()
        .with_title(format!(
            "blazy - Phase 0.5 area screen ({} areas, {} nodes)",
            args.areas, args.nodes
        ))
        .with_size(1400.0, 900.0)
        .with_backend(backend);

    let building = graph.clone();
    let app = EditorApp::new(&graph, move |area, session| spec.area(&building, area, session));
    // A bad overrides file is reported and the defaults stay, rather than a window that
    // refuses to open over a typo.
    let app = match &args.keymap {
        Some(path) => match app.with_keymap_overrides(path) {
            Ok(app) => app,
            Err(error) => {
                eprintln!("{}: {error}; the default keymap is in force", path.display());
                std::process::exit(2);
            },
        },
        None => app,
    };
    app.with_properties(node_canvas::property_set())
        .with_workspace(args.workspace)
        .with_layer_cache(layers)
        .with_windows(args.windows)
        .run(config, SplitTree::balanced(areas))
        .unwrap();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use area_screen::{Screen, sync_window};
    use blazy::areas::AreaContent;
    use blazy::masonry::app::{RenderRoot, RenderRootOptions, WindowSizePolicy};
    use blazy::masonry::core::{NewWidget, WidgetId};
    use blazy::masonry::dpi::PhysicalSize;
    use blazy::masonry::kurbo::Point;
    use blazy::node_editor::Change;
    use blazy::ops::keymap::Props;
    use blazy::shell::window::{ShellCtx, ShellDriver};
    use node_canvas::editor::NodeEditor;
    use node_canvas::model::SharedGraph;
    use node_canvas::property_set;

    use super::*;

    /// The application this window runs, over `graph`, with nothing opened yet.
    fn app_over(graph: &SharedGraph, spec: ScreenSpec) -> EditorApp<GraphModel> {
        let building = graph.clone();
        EditorApp::new(graph, move |area, session| spec.area(&building, area, session)).with_layer_cache(false)
    }

    /// Two windows over one graph, driven the way the shell drives them, through the
    /// application the window runs.
    ///
    /// Not a harness: the application's path goes through `RenderRoot`, and a harness does
    /// not hand one out (§39.5). This is what the shell does — deliver the change, call
    /// `settled`, draw the other window, which calls `frame` — and it is the test that was
    /// missing when a change made with the mouse failed to cross.
    #[test]
    fn a_change_in_one_window_reaches_the_other_through_the_driver() {
        let graph = share(GraphModel::generated(60));
        let spec = ScreenSpec::new(2, 60).with_ops(true);
        let mut app = app_over(&graph, spec);
        let mut first = root_of(&app);
        let mut second = root_of(&app);
        let mut cx = ShellCtx::new();
        let (key_first, key_second) = (cx.name_window(), cx.name_window());
        app.started(&mut cx, key_first, &mut first);
        app.started(&mut cx, key_second, &mut second);
        let _ = cx.drain();

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
        app.settled(&mut cx, key_second, &mut second);
        assert!(!cx.drain().is_empty(), "the driver asked for the other window to draw");

        // And the other window draws, which is where it collects the change.
        assert_ne!(canvas_pos(&mut first, 0), Some(truth), "before its frame it is behind");
        app.frame(key_first, &mut first);
        let _ = first.redraw();
        assert_eq!(
            canvas_pos(&mut first, 0),
            Some(truth),
            "after its frame it shows what the model says"
        );
    }

    /// An area with no operator layer collects its changes too.
    ///
    /// The defect this pins down: the pull looked only for a `NodeEditor`, and an area
    /// without one holds the canvas itself, so the pull ran on every frame and applied
    /// nothing — silently, because there is nothing there to fail (§44.3). The pull asks
    /// the graph's views now, and a view is a canvas whatever wraps it; this shape stays
    /// to keep it that way.
    #[test]
    fn an_area_without_the_operator_layer_collects_its_changes() {
        let graph = share(GraphModel::generated(60));
        let app = app_over(&graph, ScreenSpec::new(2, 60));
        let mut root = root_of(&app);

        // Straight into the model and the record, because there are no operators here to
        // do it — which is the point: the pull is what carries a change into a view,
        // whatever made it.
        let moved = Point::new(40.0, 40.0);
        graph.borrow_mut().set_pos(0, moved);
        graph.borrow().views().note(Change::Moved { index: 0, pos: moved });
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
    fn root_of(app: &EditorApp<GraphModel>) -> RenderRoot {
        let screen = app.screen(SplitTree::balanced(2));
        let mut root = RenderRoot::new(NewWidget::new(screen).erased(), |_signal| {}, RenderRootOptions {
            default_properties: Arc::new(property_set()),
            use_system_fonts: false,
            size_policy: WindowSizePolicy::User,
            size: PhysicalSize::new(1400, 900),
            scale_factor: 1.0,
            test_font: None,
        });
        let _ = root.redraw();
        root
    }

    /// The main region of one area of a window: an editor, or a bare canvas.
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
