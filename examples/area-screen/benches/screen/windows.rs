//! Two windows over one graph: what they cost, and what one can do to the other.
//!
//! A window cannot be opened from a benchmark — there is no event loop — so a window
//! here is what a window is as far as the frame is concerned: its own `RenderRoot` with
//! its own screen over the shared models. That is the same approximation §39.4 used for
//! the host seat, and it is exact for everything this table claims: frame cost is the
//! cost of walking *a* tree (§20.2), and the two trees are real.

use std::time::Instant;

use area_screen::{Screen, ScreenSpec, detach_area, sync_editor};
use bench_utils::criteria::{Criterion, Kind, ScenarioRecord};
use blazy::masonry::core::keyboard::{Key, Modifiers};
use blazy::masonry::core::{NewWidget, WidgetId};
use blazy::masonry::dpi::PhysicalSize;
use blazy::masonry::kurbo::{Point, Vec2};
use blazy::masonry::testing::TestHarness;
use blazy::masonry::theme::default_property_set;
use blazy::node_editor::ops::CANVAS_SCOPE;
use blazy::ops::event::OpEvent;
use blazy::ops::keymap::{Props, Scope};
use blazy::ops::runtime::Seat;
use node_canvas::editor::NodeEditor;
use node_canvas::model::SharedGraph;

use crate::bench::{Options, VIEWPORT};

/// One row: a window, idle or panned, with the other window's counters beside it.
pub(crate) struct WindowRow {
    pub(crate) what: &'static str,
    windows: usize,
    /// Layout passes per frame in the window the gesture happened in.
    here: f64,
    /// Layout passes per frame in the other window, which nothing touched.
    there: f64,
    /// Areas the other window resized, which nothing should have.
    there_resizes: f64,
    ms: f64,
}

/// A window: its own root, its own screen, the same graph.
fn window(areas: usize, nodes: usize, graph: Option<&SharedGraph>) -> (TestHarness<Screen>, SharedGraph) {
    let spec = ScreenSpec::new(areas, nodes).with_ops(true);
    let (screen, graph) = match graph {
        Some(graph) => (spec.over(graph), graph.clone()),
        None => spec.build(),
    };
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(screen),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    (harness, graph)
}

/// Layout passes and area resizes a window has done so far.
///
/// Layout *passes* rather than child layouts, and that is the whole difference between a
/// counter that sees an idle window working and one that does not: moving a node
/// re-places a child without re-laying it out, which is what §20's drag criterion
/// measured as 0.0. A window made to work by another window would move nodes, not lay
/// out children, and `child_layouts` would report nothing.
///
/// Summed over the areas, because what a pass walks is the window's whole tree (§20.2)
/// — an area holding nothing still costs its own visit.
fn counters(harness: &TestHarness<Screen>) -> (u64, u64) {
    let areas = harness.root_widget().area_ids().len();
    let mut child_layouts = 0;
    for area in 0..areas {
        // The canvas's own counters, not the editor's copy of them: the editor caches
        // them during *its* layout, so a frame that laid out a node without laying out
        // the editor would be invisible — which is exactly the frame this table is
        // looking for.
        let canvas = canvas_id(harness, area);
        child_layouts += harness
            .get_widget_with_id(canvas)
            .downcast::<blazy::canvas::CanvasLayer>()
            .expect("an editor holds a canvas")
            .stats()
            .counters
            .content_layouts;
    }
    (child_layouts, harness.root_widget().stats().counters.area_resizes)
}

/// The canvas inside one area's editor.
fn canvas_id(harness: &TestHarness<Screen>, area: usize) -> WidgetId {
    harness
        .get_widget_with_id(editor_id(harness, area))
        .downcast::<NodeEditor>()
        .expect("an area of this screen holds an editor")
        .canvas_id()
}

/// The editor of one area, for the operator that writes the model.
fn editor_id(harness: &TestHarness<Screen>, area: usize) -> WidgetId {
    let area_id = harness.root_widget().area_ids()[area];
    *harness
        .get_widget_with_id(area_id)
        .downcast::<blazy::areas::AreaContent>()
        .expect("every area holds a region stack")
        .region_ids()
        .last()
        .expect("an area has regions")
}

/// Pans one area of a window.
///
/// Its own rather than the table's, because these screens carry the operator layer and
/// the main region of such an area is the editor, not the canvas underneath it.
fn pan_area(harness: &mut TestHarness<Screen>, area: usize, delta: Vec2) {
    let id = editor_id(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            blazy::canvas::CanvasLayer::pan(&mut canvas, delta);
        });
    });
}

/// Brings a window up to date with the graph, and counts what that took.
fn sync(harness: &mut TestHarness<Screen>, graph: &SharedGraph) -> usize {
    let areas = harness.root_widget().area_ids().len();
    let mut applied = 0;
    for area in 0..areas {
        let id = editor_id(harness, area);
        applied += harness.edit_widget_with_id(id, |mut widget| {
            let mut editor = widget.downcast::<NodeEditor>();
            sync_editor(&mut editor, graph)
        });
    }
    applied
}

/// What two windows cost, and what one does to the other.
pub(crate) fn window_table(opts: &Options, areas: usize, nodes: usize) -> Vec<WindowRow> {
    let frames = opts.frames();
    println!("\nwindows: two screens over one graph, each in its own root");

    let mut rows = Vec::new();

    // One window, idle: the baseline every "two windows" number is read against.
    let (mut alone, alone_graph) = window(areas, nodes, None);
    rows.push(idle_row("one window, idle", &mut alone, None, &alone_graph, frames, 1));

    // Two windows, both idle.
    let (mut first, graph) = window(areas, nodes, None);
    let (mut second, _) = window(areas, nodes, Some(&graph));
    rows.push(idle_row(
        "two windows, idle",
        &mut first,
        Some(&mut second),
        &graph,
        frames,
        2,
    ));

    // A pan in one of them. The other is not touched and must not notice — and it is
    // catching up with the model on every frame, exactly as the shell's `frame` hook
    // makes it, so "not touched" is a claim about a window that is actually running.
    let before_there = counters(&second);
    let clock = Instant::now();
    for _ in 0..frames {
        pan_area(&mut first, 0, Vec2::new(-6.0, -3.0));
        sync(&mut first, &graph);
        sync(&mut second, &graph);
        let _ = first.redraw();
        let _ = second.redraw();
    }
    let ms = clock.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    let here = counters(&first);
    let there = counters(&second);
    rows.push(WindowRow {
        what: "pan in one window",
        windows: 2,
        here: here.0 as f64 / frames as f64,
        there: (there.0 - before_there.0) as f64 / frames as f64,
        there_resizes: (there.1 - before_there.1) as f64 / frames as f64,
        ms,
    });

    print_windows(&rows);
    rows
}

fn idle_row(
    what: &'static str,
    harness: &mut TestHarness<Screen>,
    other: Option<&mut TestHarness<Screen>>,
    graph: &SharedGraph,
    frames: usize,
    windows: usize,
) -> WindowRow {
    let before = counters(harness);
    let mut other = other;
    let clock = Instant::now();
    for _ in 0..frames {
        // Idle means idle *including* the catch-up: a window that pulls nothing must
        // cost nothing, or the pull would be a per-frame tax on every window.
        sync(harness, graph);
        let _ = harness.redraw();
        if let Some(other) = other.as_mut() {
            sync(other, graph);
            let _ = other.redraw();
        }
    }
    let ms = clock.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    let after = counters(harness);
    WindowRow {
        what,
        windows,
        here: (after.0 - before.0) as f64 / frames as f64,
        there: 0.0,
        there_resizes: 0.0,
        ms,
    }
}

/// What a change in one window costs the other to follow (§30 across the boundary).
pub(crate) struct CrossRow {
    /// Nodes showing a different place in the second window before it caught up.
    pub(crate) adrift_before: usize,
    /// And after.
    pub(crate) adrift_after: usize,
    /// Changes the second window applied to catch up.
    pub(crate) applied: usize,
    /// Nodes in the graph, so "applied" can be read against something.
    pub(crate) nodes: usize,
    /// Links whose presence in an area of the other window disagrees with the model,
    /// after its pull.
    ///
    /// Three links added and one of them removed in the first window, so both directions
    /// are asked. The record the other window collects from used to have no links in it
    /// at all, and a node-only criterion passed over that for as long as it existed.
    pub(crate) links_adrift: usize,
    /// Changes still owed after every window collected its share, plus views registered
    /// for canvases no window holds — after a join, a detach and an edit.
    ///
    /// A view that left the tree used to stay registered and owed every later change, so
    /// the windows were woken after every event from the first join on (§36). Measured
    /// on the size of what is held rather than on work per frame, which is the only
    /// place that defect lived (§28.4).
    pub(crate) owed_after: usize,
    /// Views the graph held at the end, and canvases the three windows hold.
    pub(crate) views: usize,
    pub(crate) canvases: usize,
}

/// Moves one node in one window and asks the other where it thinks that node is.
pub(crate) fn cross_window(areas: usize, nodes: usize) -> CrossRow {
    let (mut first, graph) = window(areas, nodes, None);
    let (mut second, _) = window(areas, nodes, Some(&graph));

    let id = editor_id(&first, 0);
    first.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 0));
        NodeEditor::exec(&mut editor, "node.move", &Props::new().with_float("dx", 40.0));
    });
    let _ = first.redraw();
    let _ = second.redraw();

    let truth = graph.borrow().node(0).pos;
    let adrift_before = usize::from(canvas_pos(&mut second, 0) != Some(truth));
    let applied = sync(&mut second, &graph);
    let _ = second.redraw();
    let adrift_after = usize::from(canvas_pos(&mut second, 0) != Some(truth));

    // Links, both ways: three added in the first window, one of them removed again.
    let added = [(0, 7), (1, 8), (2, 9)];
    let first_editor = editor_id(&first, 0);
    first.edit_widget_with_id(first_editor, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        for (from, to) in added {
            let ends = Props::new().with_int("from", from).with_int("to", to);
            NodeEditor::exec(&mut editor, "link.add", &ends);
        }
        let ends = Props::new().with_int("from", 1).with_int("to", 8);
        NodeEditor::exec(&mut editor, "link.delete", &ends);
    });
    let _ = first.redraw();
    let _ = sync(&mut second, &graph);
    let _ = second.redraw();
    let links_adrift = links_adrift(&mut second, &graph, &added);

    // Views that leave: a join in the second window, a detach out of the first into a
    // third, and then an edit, so that there is something for a departed view to be owed.
    second.edit_root_widget(|mut screen| {
        let tree = screen.widget.tree().clone();
        if let Some((keep, gone)) = tree
            .areas()
            .find_map(|area| tree.joinable(area).map(|sibling| (area, sibling)))
        {
            Screen::join(&mut screen, keep, gone);
        }
    });
    let _ = second.redraw();
    let session = first.edit_root_widget(|mut screen| {
        let last = screen.widget.area_ids().len().saturating_sub(1);
        detach_area(&mut screen, last)
    });
    let _ = first.redraw();
    let mut third = session.map(|session| {
        let screen = ScreenSpec::new(1, nodes)
            .with_ops(true)
            .over_with(&graph, Some(session));
        harness(screen)
    });
    let first_editor = editor_id(&first, 0);
    first.edit_widget_with_id(first_editor, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 3));
        NodeEditor::exec(&mut editor, "node.move", &Props::new().with_float("dx", 25.0));
    });
    let _ = first.redraw();
    let mut canvases = 0;
    for window in [Some(&mut first), Some(&mut second), third.as_mut()]
        .into_iter()
        .flatten()
    {
        let _ = sync(window, &graph);
        let _ = window.redraw();
        canvases += window.root_widget().area_ids().len();
    }
    let views = graph.borrow().views().len();
    let owed_after = graph.borrow().views().owed() + views.abs_diff(canvases);

    CrossRow {
        adrift_before,
        adrift_after,
        applied,
        nodes,
        links_adrift,
        owed_after,
        views,
        canvases,
    }
}

/// Links whose presence in some area of a window disagrees with the model.
fn links_adrift(harness: &mut TestHarness<Screen>, graph: &SharedGraph, links: &[(i64, i64)]) -> usize {
    let mut adrift = 0;
    for area in 0..harness.root_widget().area_ids().len() {
        let id = editor_id(harness, area);
        for &(from, to) in links {
            let link = blazy::canvas::Link::new(from as usize, to as usize);
            let truth = graph.borrow().links().contains(&link);
            let shown = harness.edit_widget_with_id(id, |mut widget| {
                let mut editor = widget.downcast::<NodeEditor>();
                NodeEditor::with_canvas(&mut editor, |mut canvas| {
                    blazy::canvas::CanvasLayer::link_name(&mut canvas, link).is_some()
                })
            });
            adrift += usize::from(shown != truth);
        }
    }
    adrift
}

/// Where a window thinks node 0 is.
fn canvas_pos(harness: &mut TestHarness<Screen>, area: usize) -> Option<Point> {
    let id = editor_id(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            blazy::canvas::CanvasLayer::child_pos(&mut canvas, 0)
        })
    })
}

fn print_windows(rows: &[WindowRow]) {
    println!(
        "  {:<22} {:>8} {:>16} {:>18} {:>16} {:>10}",
        "what", "windows", "passes/f here", "passes/f there", "resizes there", "ms/frame"
    );
    for row in rows {
        println!(
            "  {:<22} {:>8} {:>16.2} {:>18.2} {:>16.2} {:>10.3}",
            row.what, row.windows, row.here, row.there, row.there_resizes, row.ms
        );
    }
}

impl WindowRow {
    pub(crate) fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: "windows",
            frames: 1,
            mean_ms: self.ms,
            worst_ms: self.ms,
            materialised: 0,
            detail: format!("{} ({} windows)", self.what, self.windows),
            child_layouts_per_frame: self.here,
            builds_per_frame: 0.0,
            far_repaints_per_frame: 0.0,
            extra: vec![
                ("layouts_in_the_other_window", self.there),
                ("resizes_in_the_other_window", self.there_resizes),
            ],
        }
    }
}

impl CrossRow {
    /// The finding this row exists for, archived: before the pull the second window is
    /// adrift, after it is not. A criterion can only gate the second half; the first is
    /// what says the pull is doing anything at all.
    pub(crate) fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: "windows",
            frames: 1,
            mean_ms: 0.0,
            worst_ms: 0.0,
            materialised: 0,
            detail: format!("one node moved in the other window, {} nodes", self.nodes),
            child_layouts_per_frame: 0.0,
            builds_per_frame: 0.0,
            far_repaints_per_frame: 0.0,
            extra: vec![
                ("nodes_adrift_before_the_pull", self.adrift_before as f64),
                ("nodes_adrift_after_the_pull", self.adrift_after as f64),
                ("changes_applied", self.applied as f64),
                ("links_adrift_after_the_pull", self.links_adrift as f64),
                ("owed_after_every_window_pulled", self.owed_after as f64),
                ("views_held", self.views as f64),
                ("canvases_in_windows", self.canvases as f64),
            ],
        }
    }
}

/// The criteria of the second window's first phase.
pub(crate) fn criteria(rows: &[WindowRow], cross: &CrossRow) -> Vec<Criterion> {
    let alone = rows.iter().find(|row| row.windows == 1).map_or(0.0, |row| row.here);
    let together = rows
        .iter()
        .find(|row| row.what == "two windows, idle")
        .map_or(0.0, |row| row.here);
    let panned = rows.iter().find(|row| row.what == "pan in one window");

    vec![
        // A frame is the cost of walking a tree, and the tree is per window (§20.2), so
        // two idle windows are two idles. Measured as "an idle window lays out nothing",
        // with and without a second one, rather than as the difference between the two:
        // a difference of zero is also what two windows that both do useless work would
        // report, and this way the catch-up pull cannot become a per-frame tax unnoticed.
        Criterion {
            name: "an_idle_window_lays_out_nothing",
            claim: "an idle window costs nothing, with or without a second one open",
            kind: Kind::Counter,
            measured: alone.max(together),
            bound: 1.0,
            unit: "layout passes/frame in an idle window",
        },
        // The other half: a gesture in one window is that window's work. Claim 2 of §21,
        // raised from an area to a window.
        Criterion {
            name: "a_frame_in_one_window_leaves_the_other_alone",
            claim: "panning one window lays out nothing in the other",
            kind: Kind::Counter,
            measured: panned.map_or(0.0, |row| row.there + row.there_resizes),
            bound: 1.0,
            unit: "layout passes and resizes/frame in the untouched window",
        },
        // §30 across the boundary, and the reason the pull exists: without it the second
        // window shows a node where it used to be, and nothing says so. Counted from the
        // failing side, so the criterion is about the mechanism working rather than about
        // the defect existing.
        Criterion {
            name: "a_change_reaches_the_other_window",
            claim: "a window catches up with what another one changed",
            kind: Kind::Counter,
            measured: cross.adrift_after as f64,
            bound: 1.0,
            unit: "nodes still adrift after the pull",
        },
        // And the cost of catching up is what changed, not what exists.
        Criterion {
            name: "catching_up_costs_what_changed",
            claim: "a window applies the changes it missed, not the graph",
            kind: Kind::Counter,
            measured: cross.applied as f64,
            bound: (cross.nodes / 4) as f64,
            unit: "changes applied for one moved node",
        },
        // Everything a view copies out of the model has to cross, and topology is part of
        // it (§43): the record a window collects from had nodes and no links, so a link
        // made in one window existed in the model and in that window only, for good.
        Criterion {
            name: "a_link_reaches_the_other_window",
            claim: "a link made or removed in one window is made or removed in the other",
            kind: Kind::Counter,
            measured: cross.links_adrift as f64,
            bound: 1.0,
            unit: "links adrift after the pull, summed over the areas",
        },
        // A view that left the tree is owed nothing. Without it the windows never idle
        // again after the first join: they are woken while anything is owed, and a view
        // that no window holds can never collect what it is owed (§36, §28.4).
        Criterion {
            name: "a_departed_view_is_owed_nothing",
            claim: "after a join, a detach and an edit, every window's pull leaves nothing owed",
            kind: Kind::Counter,
            measured: cross.owed_after as f64,
            bound: 1.0,
            unit: "changes owed plus views no window holds",
        },
    ]
}

/// What detach costs and what it loses (the detach task, phase 3).
pub(crate) struct DetachRow {
    /// Fields of the session that differ after the move: view, selection, history.
    pub(crate) fields_lost: usize,
    /// Areas the source screen rebuilt. Zero: nothing but the one that left is touched.
    pub(crate) rebuilt: usize,
    /// Nodes whose name means a different node in the new window.
    ///
    /// Measured on a graph that has been edited, so the names have holes in them: on
    /// dense names they would agree by accident (§43).
    pub(crate) names_adrift: usize,
    /// Operators left running in the session after the move.
    pub(crate) still_running: usize,
    /// Areas holding a session, before and after, across both screens.
    pub(crate) payloads_before: usize,
    pub(crate) payloads_after: usize,
    /// Whether the last area of a screen refused to go.
    pub(crate) last_area_refused: bool,
    ms: f64,
}

/// Detaches an area and asks what arrived.
pub(crate) fn detach_row(areas: usize, nodes: usize) -> DetachRow {
    let (mut screen, graph) = window(areas.max(2), nodes, None);

    // A graph with holes in its names, so "the names agree" means something (§43).
    let id = editor_id(&screen, 1);
    screen.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(&mut editor, "node.delete", &Props::new().with_int("index", 2));
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 5));
        NodeEditor::exec(&mut editor, "node.move", &Props::new().with_float("dx", 12.0));
    });
    let _ = screen.redraw();

    let before = {
        let editor = screen
            .get_widget_with_id(id)
            .downcast::<NodeEditor>()
            .expect("an area of this screen holds an editor");
        (editor.selection(), editor.history_depth(), editor.stats().zoom)
    };
    let payloads_before = payloads(&screen);
    let builds_before = screen.root_widget().stats().counters.builds;

    // A gesture in flight when the area is taken away. Started through the session's own
    // runtime, because that is where a modal operator lives (§38) and what detach has to
    // deal with (decision 6): without one running, the criterion below would ask nothing.
    let running = screen
        .root_widget()
        .payload(1)
        .cloned()
        .expect("every area carries its session");
    {
        let running = &mut *running.borrow_mut();
        running.runtime.dispatch(
            &mut running.world,
            &OpEvent::Key {
                key: Key::Character("g".into()),
                mods: Modifiers::empty(),
                down: true,
            },
            Scope(&CANVAS_SCOPE),
            Seat::Tree,
        );
        assert!(running.runtime.modal_depth() > 0, "a grab is running");
    }

    let clock = Instant::now();
    let session = screen
        .edit_root_widget(|mut screen| detach_area(&mut screen, 1))
        .expect("an area of several detaches");
    let ms = clock.elapsed().as_secs_f64() * 1000.0;
    let _ = screen.redraw();

    let still_running = session.borrow().runtime.modal_depth();
    let detached = ScreenSpec::new(1, nodes)
        .with_ops(true)
        .over_with(&graph, Some(session));
    let mut moved_to = harness(detached);
    let arrived_id = editor_id(&moved_to, 0);
    let arrived = moved_to
        .get_widget_with_id(arrived_id)
        .downcast::<NodeEditor>()
        .expect("the detached area holds an editor");

    let fields_lost = usize::from(arrived.selection() != before.0)
        + usize::from(arrived.history_depth() != before.1)
        + usize::from(arrived.stats().zoom != before.2);

    // Every live name means the same node in both places. The old window no longer holds
    // this area, so the comparison is against the model, which is the truth (§30).
    let names_adrift = (0..graph.borrow().names())
        .filter(|&index| {
            let truth = graph.borrow().try_node(index).map(|node| node.pos);
            let shown = canvas_pos_of(&mut moved_to, 0, index);
            truth.is_some() && shown != truth
        })
        .count();

    // And the last area of a screen refuses to go (decision 5).
    let (mut single, _) = window(1, nodes, None);
    let last_area_refused = single
        .edit_root_widget(|mut screen| detach_area(&mut screen, 0))
        .is_none();

    DetachRow {
        fields_lost,
        rebuilt: (screen.root_widget().stats().counters.builds - builds_before) as usize,
        names_adrift,
        still_running,
        payloads_before,
        payloads_after: payloads(&screen) + payloads(&moved_to),
        last_area_refused,
        ms,
    }
}

/// Areas of a screen that are carrying a session.
fn payloads(harness: &TestHarness<Screen>) -> usize {
    let screen = harness.root_widget();
    screen
        .tree()
        .areas()
        .filter(|&area| screen.payload(area).is_some())
        .count()
}

/// Where a window thinks a node is.
fn canvas_pos_of(harness: &mut TestHarness<Screen>, area: usize, index: usize) -> Option<Point> {
    let id = editor_id(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            blazy::canvas::CanvasLayer::child_pos(&mut canvas, index)
        })
    })
}

/// A harness over a screen built elsewhere.
fn harness(screen: Screen) -> TestHarness<Screen> {
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(screen),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    harness
}

impl DetachRow {
    pub(crate) fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: "detach",
            frames: 1,
            mean_ms: self.ms,
            worst_ms: self.ms,
            materialised: 0,
            detail: "one area into a window of its own".to_string(),
            child_layouts_per_frame: 0.0,
            builds_per_frame: self.rebuilt as f64,
            far_repaints_per_frame: 0.0,
            extra: vec![
                ("session_fields_lost", self.fields_lost as f64),
                ("areas_rebuilt", self.rebuilt as f64),
                ("names_adrift", self.names_adrift as f64),
                ("operators_still_running", self.still_running as f64),
                ("payloads_before", self.payloads_before as f64),
                ("payloads_after", self.payloads_after as f64),
            ],
        }
    }
}

/// The criteria of phase 3.
pub(crate) fn detach_criteria(row: &DetachRow) -> Vec<Criterion> {
    vec![
        // Decision 1, counted: the area is rebuilt and nothing the user would miss is.
        Criterion {
            name: "detach_loses_nothing",
            claim: "a detached area arrives with its view, its selection and its history",
            kind: Kind::Counter,
            measured: row.fields_lost as f64,
            bound: 1.0,
            unit: "session fields different after the move",
        },
        // §41.2's rule, at the window boundary.
        Criterion {
            name: "detach_rebuilds_nothing_that_stayed",
            claim: "detaching one area rebuilds no other",
            kind: Kind::Counter,
            measured: row.rebuilt as f64,
            bound: 1.0,
            unit: "area widgets built in the source screen",
        },
        // §43's names, across the boundary: checked on a graph that has holes, because
        // dense names would agree by accident.
        Criterion {
            name: "names_travel_with_the_area",
            claim: "a name means the same node in the window the area moved to",
            kind: Kind::Counter,
            measured: row.names_adrift as f64,
            bound: 1.0,
            unit: "nodes shown in the wrong place",
        },
        // Decision 6: a gesture belongs to the window it was made in.
        Criterion {
            name: "detach_leaves_no_operator_running",
            claim: "detaching cancels whatever was modal in the area's session",
            kind: Kind::Counter,
            measured: row.still_running as f64,
            bound: 1.0,
            unit: "operators still on the modal stack",
        },
        // Decision 1a: the payload moved, it was not copied and not lost.
        Criterion {
            name: "detach_moves_the_session",
            claim: "the sessions of both screens together are the sessions there were",
            kind: Kind::Counter,
            measured: (row.payloads_after as f64 - row.payloads_before as f64).abs(),
            bound: 1.0,
            unit: "sessions gained or lost by the move",
        },
        // Decision 5: the last area of a screen does not go, because a screen with no
        // area is not expressible.
        Criterion {
            name: "the_last_area_does_not_detach",
            claim: "a screen never ends up with no areas",
            kind: Kind::Counter,
            measured: f64::from(!row.last_area_refused),
            bound: 1.0,
            unit: "screens left empty",
        },
    ]
}
