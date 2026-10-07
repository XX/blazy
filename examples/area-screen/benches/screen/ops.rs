//! What an operation on the areas costs, and what it must not do (§41.2).
//!
//! The four operations of §16 item 4 — split, join, swap, maximize/restore — plus the
//! round trip through a workspace file, on a real screen rather than on inert leaves:
//! eight areas, each holding a canvas over one graph. The unit tests in `blazy-areas`
//! check the same claims against a widget that does nothing, which is the right place
//! for the arithmetic; this is the place where "nothing was rebuilt" means "no node
//! editor lost its view".
//!
//! Every number here is a **counter**. The one claim that is about geometry —
//! restoring gives back the rectangles that were there — is counted from the failing
//! side, as the criteria always are: areas that came back the wrong size.

use std::time::{Duration, Instant};

use area_screen::{Screen, build_screen};
use blazy::areas::{AreaId, AreaScreen, SplitTree, Workspace};
use blazy::masonry::core::NewWidget;
use blazy::masonry::dpi::PhysicalSize;
use blazy::masonry::kurbo::{Axis, Size};
use blazy::masonry::testing::TestHarness;
use node_canvas::property_set;

use crate::bench::{Options, VIEWPORT};

/// One operation, measured.
pub(crate) struct OpsRow {
    pub(crate) what: &'static str,
    /// Areas on the screen after the operation.
    pub(crate) areas: usize,
    /// Area widgets built by the operation.
    ///
    /// The counter the whole set is judged on: an area that survived an operation must
    /// come out of it as the same widget, because what it holds — the view, the
    /// selection, the materialised nodes — does not live anywhere else (§30).
    pub(crate) builds: u64,
    /// Areas handed a size different from the one they had.
    pub(crate) resizes: u64,
    /// Areas whose rectangle differs from the one they are supposed to have come back
    /// to. Only meaningful for the operations that promise a return.
    pub(crate) mismatched: usize,
    pub(crate) ms: f64,
}

/// A screen of `areas` areas over one graph, settled.
fn harness(areas: usize, nodes: usize) -> TestHarness<Screen> {
    let (screen, _graph) = build_screen(areas, nodes, None);
    let mut harness = TestHarness::create_with_size(
        property_set(),
        NewWidget::new(screen),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    harness
}

/// The border-box size of every area that has a widget, by area id.
fn sizes(harness: &mut TestHarness<Screen>) -> Vec<(AreaId, Size)> {
    let ids: Vec<(AreaId, blazy::masonry::core::WidgetId)> = harness
        .root_widget()
        .tree()
        .areas()
        .filter_map(|area| harness.root_widget().area_widget(area).map(|id| (area, id)))
        .collect();
    ids.into_iter()
        .map(|(area, id)| (area, harness.get_widget_with_id(id).ctx().border_box().size()))
        .collect()
}

/// Runs one operation on a fresh screen and reports what it cost.
///
/// `expected` is what the sizes have to be afterwards, for an operation that promises to
/// return somewhere; `None` for one that does not.
fn row(
    what: &'static str,
    areas: usize,
    nodes: usize,
    act: impl FnOnce(&mut TestHarness<Screen>),
    expected: impl FnOnce(&[(AreaId, Size)]) -> Option<Vec<(AreaId, Size)>>,
) -> OpsRow {
    let mut harness = harness(areas, nodes);
    let before = sizes(&mut harness);
    let counters = harness.root_widget().stats().counters;

    let start = Instant::now();
    act(&mut harness);
    let _ = harness.redraw();
    let elapsed: Duration = start.elapsed();

    let after = harness.root_widget().stats().counters;
    let now = sizes(&mut harness);
    let mismatched = match expected(&before) {
        Some(wanted) => {
            let missing = wanted.iter().filter(|entry| !now.contains(entry)).count();
            // Both directions: an area that arrived where nobody asked it to is as wrong
            // as one that did not arrive at all.
            missing + now.iter().filter(|entry| !wanted.contains(entry)).count()
        },
        None => 0,
    };

    OpsRow {
        what,
        areas: harness.root_widget().tree().area_count(),
        builds: after.builds - counters.builds,
        resizes: after.area_resizes - counters.area_resizes,
        mismatched,
        ms: elapsed.as_secs_f64() * 1000.0,
    }
}

/// Tree → file → **a screen that has moved on** → tree.
///
/// The change in the middle is what makes this a round trip rather than a tautology: a
/// load that did nothing at all would pass a save-then-load comparison, and a file that
/// lost the ratios or renumbered the areas would not. So the screen is joined and
/// swapped first — the file has to carry a tree with a hole in its id space — then
/// written, then a splitter is dragged somewhere else, and only then is the file read
/// back. What has to come out is the rectangles as they were when it was written.
fn round_trip(areas: usize, nodes: usize) -> OpsRow {
    let mut harness = harness(areas, nodes);

    // A tree worth writing: an id that is no longer there, a pair that has moved, and a
    // splitter that is **not** where a balanced screen puts it. The last one is not
    // decoration — a tree straight out of `balanced` has 0.5 at every split, and a writer
    // that rounded its ratios to one decimal would round-trip it perfectly. Found by
    // breaking this criterion on purpose and watching it pass (§41.7).
    let sibling = harness.root_widget().tree().joinable(0);
    harness.edit_root_widget(|mut screen| {
        assert!(AreaScreen::join(&mut screen, 0, sibling.expect("area 0 has a sibling")));
        assert!(AreaScreen::swap(&mut screen, 2, 3));
    });
    let _ = harness.redraw();
    let awkward = harness.root_widget().bars()[0];
    let off_centre =
        blazy::masonry::kurbo::Point::new(awkward.rect.center().x - 137.0, awkward.rect.center().y - 137.0);
    harness.edit_root_widget(|mut screen| AreaScreen::drag_bar(&mut screen, awkward.split, off_centre));
    let _ = harness.redraw();

    let saved = sizes(&mut harness);
    let text = {
        let screen = harness.root_widget();
        let mut workspace = Workspace::new(screen.tree().clone());
        for area in screen.tree().areas() {
            workspace.set_content(area, "node-editor");
        }
        workspace.write()
    };

    // The screen moves on, so a load that does nothing cannot pass.
    let bar = harness.root_widget().bars()[0];
    let moved = blazy::masonry::kurbo::Point::new(bar.rect.center().x + 211.0, bar.rect.center().y + 211.0);
    harness.edit_root_widget(|mut screen| AreaScreen::drag_bar(&mut screen, bar.split, moved));
    let _ = harness.redraw();
    let drifted = sizes(&mut harness);
    assert_ne!(
        drifted, saved,
        "the drag has to have moved something, or nothing is proved"
    );

    let counters = harness.root_widget().stats().counters;
    let start = Instant::now();
    let read = Workspace::parse(&text).expect("what we wrote reads back");
    let tree: SplitTree = read.tree().clone();
    harness.edit_root_widget(|mut screen| AreaScreen::set_tree(&mut screen, tree));
    let _ = harness.redraw();
    let elapsed = start.elapsed();

    let now = sizes(&mut harness);
    let after = harness.root_widget().stats().counters;
    let missing = saved.iter().filter(|entry| !now.contains(entry)).count();
    OpsRow {
        what: "workspace round trip",
        areas: harness.root_widget().tree().area_count(),
        builds: after.builds - counters.builds,
        resizes: after.area_resizes - counters.area_resizes,
        mismatched: missing + now.iter().filter(|entry| !saved.contains(entry)).count(),
        ms: elapsed.as_secs_f64() * 1000.0,
    }
}

/// The table §41.2 is written from.
pub(crate) fn ops_table(opts: &Options, areas: usize, nodes: usize) -> Vec<OpsRow> {
    let areas = areas.max(2);
    // The graph is what makes an area expensive to rebuild, so the quick set keeps it
    // rather than shrinking it: the claim is about widgets surviving, not about frames.
    let nodes = if opts.quick { nodes.min(2_000) } else { nodes };

    let rows = vec![
        // Splitting is the one operation that builds, and it builds exactly one child.
        // It is in the table as the guard against the others being vacuous: a screen
        // where nothing is ever built would pass "nothing was rebuilt" trivially.
        row(
            "split",
            areas,
            nodes,
            |h| {
                h.edit_root_widget(|mut screen| {
                    AreaScreen::split(&mut screen, 0, Axis::Vertical, 0.5);
                });
            },
            |_| None,
        ),
        row(
            "join",
            areas,
            nodes,
            |h| {
                let pair = h.root_widget().tree().joinable(0);
                h.edit_root_widget(|mut screen| {
                    let sibling = pair.expect("area 0 has a sibling on a balanced screen");
                    assert!(AreaScreen::join(&mut screen, 0, sibling));
                });
            },
            |_| None,
        ),
        row(
            "swap",
            areas,
            nodes,
            |h| {
                let pair = h.root_widget().tree().joinable(0);
                h.edit_root_widget(|mut screen| {
                    let sibling = pair.expect("area 0 has a sibling");
                    assert!(AreaScreen::swap(&mut screen, 0, sibling));
                });
            },
            |_| None,
        ),
        row(
            "maximize",
            areas,
            nodes,
            |h| {
                h.edit_root_widget(|mut screen| assert!(AreaScreen::maximize(&mut screen, 0)));
            },
            |_| None,
        ),
        // The one that promises a return, and the promise is bit for bit.
        row(
            "maximize and restore",
            areas,
            nodes,
            |h| {
                h.edit_root_widget(|mut screen| assert!(AreaScreen::maximize(&mut screen, 0)));
                let _ = h.redraw();
                h.edit_root_widget(|mut screen| assert!(AreaScreen::restore(&mut screen)));
            },
            |before| Some(before.to_vec()),
        ),
        round_trip(areas, nodes),
    ];

    println!("\noperations on areas: what each one costs, on {areas} areas over one graph");
    println!(
        "  {:<22} {:>6} {:>8} {:>9} {:>12} {:>9}",
        "", "areas", "builds", "resizes", "misplaced", "ms"
    );
    for row in &rows {
        println!(
            "  {:<22} {:>6} {:>8} {:>9} {:>12} {:>9.3}",
            row.what, row.areas, row.builds, row.resizes, row.mismatched, row.ms
        );
    }
    rows
}
