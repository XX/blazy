//! What a structural edit costs, and what it is obliged to leave behind (§43).
//!
//! Its own file for the reason the operator table has one: this is a table over graph
//! sizes rather than a scenario, and what it measures is a *per-edit* quantity, not a
//! per-frame one. The interesting question is the same one §24.1 got wrong once by
//! stopping a sweep too early — does the cost follow the graph? — so the sweep reaches
//! 64 000 nodes, where a linear cost has nowhere left to hide.

use std::time::Instant;

use bench_utils::criteria::{Criterion, Kind};
use blazy::canvas::CanvasCounters;
use blazy::masonry::testing::TestHarness;
use blazy::ops::keymap::Props;
use node_canvas::editor::NodeEditor;
use node_canvas::model::SharedGraph;

use crate::bench::{Options, ScenarioRecord};
use crate::ops::ops_harness;

/// Graph sizes the table sweeps. The top end is where a linear cost would show (§24.1).
const EDIT_NODES: [usize; 3] = [5_000, 20_000, 64_000];
/// Add-and-delete cycles a row runs to see whether freed names come back.
const NAME_CYCLES: usize = 50;

/// One graph size, edited.
pub(crate) struct EditRow {
    pub(crate) nodes: usize,
    /// The first add-and-delete of the graph's life, which pays for the array it grows.
    first_add_ms: f64,
    /// Link names the adjacency walked to add one node and one link to it.
    add_scans: f64,
    /// Widgets built by one added node: it is on screen, so it should be exactly one.
    add_builds: f64,
    add_ms: f64,
    /// Link names walked to delete one node...
    delete_scans: f64,
    /// ...and the links that node actually had.
    degree: f64,
    delete_ms: f64,
    /// Names the graph uses after [`NAME_CYCLES`] add-and-delete cycles, above what it
    /// used before them. Zero if freed names are handed out again.
    names_grown: usize,
    /// Nodes that came back from undo under a different name or in a different place.
    restored_wrong: usize,
    /// Links that came back with fewer links than they left with.
    links_lost: usize,
    /// Structural edits the recorded sets did not notice (§28.4).
    unseen_edits: usize,
    /// Times the packed adjacency was rebuilt, over the whole row.
    compactions: u64,
}

/// Runs the edit table.
pub(crate) fn edit_table(opts: &Options) -> Vec<EditRow> {
    let sizes: &[usize] = if opts.quick { &EDIT_NODES[..1] } else { &EDIT_NODES };
    println!("\nedits: what one structural edit costs, and what it leaves behind (§43)");
    let rows: Vec<EditRow> = sizes.iter().map(|&nodes| edit_row(nodes)).collect();
    print_edits(&rows);
    rows
}

fn counters(harness: &TestHarness<NodeEditor>) -> CanvasCounters {
    harness.root_widget().stats().counters
}

fn edit_row(nodes: usize) -> EditRow {
    let (mut harness, graph) = ops_harness(nodes);

    // One cycle before anything is timed. A graph built by `collect` owns an array sized
    // exactly, so the *first* insertion reallocates it — 0.9 ms of `memcpy` on 64 000
    // nodes, once in the life of the graph and never again. Timing it would report the
    // allocator, and the row is about what an edit costs.
    let clock = Instant::now();
    cycle(&mut harness, &graph);
    let first_add_ms = clock.elapsed().as_secs_f64() * 1000.0;

    // --- one node added, where the view can see it
    let before = counters(&harness);
    let builds_before = before.builds;
    let clock = Instant::now();
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "node.add",
            &Props::new().with_float("x", 120.0).with_float("y", 120.0),
        );
    });
    let add_ms = clock.elapsed().as_secs_f64() * 1000.0;
    let _ = harness.redraw();
    let add_builds = (counters(&harness).builds - builds_before) as f64;
    let added = graph.borrow().names() - 1;

    // --- the link the new node is given, and whether the recorded set notices it
    let mut unseen_edits = 0;
    let recorded_before = harness.root_widget().stats().recorded_links;
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "link.add",
            &Props::new().with_int("from", 0).with_int("to", added as i64),
        );
    });
    let _ = harness.redraw();
    // The adjacency work of both halves of an addition: a node on its own touches no
    // links at all, so a counter that stopped before the link would be measuring zero
    // and claiming it meant something.
    let add_scans = (counters(&harness).edit_edge_scans - before.edit_edge_scans) as f64;
    if harness.root_widget().stats().recorded_links <= recorded_before {
        unseen_edits += 1;
    }

    // --- one node deleted, with its links
    let degree = graph
        .borrow()
        .links()
        .iter()
        .filter(|link| link.from == 0 || link.to == 0)
        .count() as f64;
    let before = counters(&harness);
    let recorded_before = harness.root_widget().stats().recorded_links;
    let place = graph.borrow().node(0).pos;
    let clock = Instant::now();
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.delete", &Props::new().with_int("index", 0));
    });
    let delete_ms = clock.elapsed().as_secs_f64() * 1000.0;
    let _ = harness.redraw();
    let delete_scans = (counters(&harness).edit_edge_scans - before.edit_edge_scans) as f64;
    if harness.root_widget().stats().recorded_links >= recorded_before {
        unseen_edits += 1;
    }

    // --- undo, and what came back
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "ed.undo", &Props::new());
    });
    let _ = harness.redraw();
    let restored_wrong = usize::from(graph.borrow().try_node(0).map(|node| node.pos) != Some(place));
    let back = graph
        .borrow()
        .links()
        .iter()
        .filter(|link| link.from == 0 || link.to == 0)
        .count() as f64;
    let links_lost = usize::from(back < degree);

    // --- names: add and delete in a loop, and see whether the array grows
    //
    // Measured after one cycle rather than from here: the free list is empty at this
    // point, so the first add legitimately takes a fresh name. What the claim is about
    // is every add after it.
    cycle(&mut harness, &graph);
    let names_before = graph.borrow().names();
    let before_compactions = counters(&harness).link_compactions;
    for _ in 0..NAME_CYCLES {
        cycle(&mut harness, &graph);
    }
    let names_grown = graph.borrow().names().saturating_sub(names_before);
    let compactions = counters(&harness).link_compactions - before_compactions;

    // --- the far field: below the box threshold the canvas paints the nodes itself,
    // from a scene recorded for a region. An edit does not move the view, so nothing
    // would re-record it unless the edit says so (§28.4).
    crate::bench::zoom_to(&mut harness, 0.02);
    let _ = harness.redraw();
    let far_before = harness.root_widget().stats().recorded_far;
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "node.add",
            &Props::new().with_float("x", 200.0).with_float("y", 200.0),
        );
    });
    let _ = harness.redraw();
    if harness.root_widget().stats().recorded_far <= far_before {
        unseen_edits += 1;
    }

    EditRow {
        nodes,
        first_add_ms,
        add_scans,
        add_builds,
        add_ms,
        delete_scans,
        degree,
        delete_ms,
        names_grown,
        restored_wrong,
        links_lost,
        unseen_edits,
        compactions,
    }
}

/// One add-and-delete of the same node: what a session does over and over.
fn cycle(harness: &mut TestHarness<NodeEditor>, graph: &SharedGraph) {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "node.add",
            &Props::new().with_float("x", 400.0).with_float("y", 400.0),
        );
    });
    let fresh = graph.borrow().names() - 1;
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "node.delete",
            &Props::new().with_int("index", fresh as i64),
        );
    });
}

fn print_edits(rows: &[EditRow]) {
    println!(
        "  {:>8}  {:>10}  {:>7}  {:>9}  {:>12}  {:>7}  {:>9}  {:>6}  {:>7}",
        "nodes", "add scans", "builds", "add ms", "delete scans", "degree", "delete ms", "names", "packs"
    );
    for row in rows {
        println!(
            "  {:>8}  {:>10.1}  {:>7.1}  {:>9.3}  {:>12.1}  {:>7.1}  {:>9.3}  {:>6}  {:>7}",
            row.nodes,
            row.add_scans,
            row.add_builds,
            row.add_ms,
            row.delete_scans,
            row.degree,
            row.delete_ms,
            row.names_grown,
            row.compactions,
        );
    }
}

impl EditRow {
    pub(crate) fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: "edits",
            frames: 1,
            mean_ms: self.add_ms,
            worst_ms: self.delete_ms,
            materialised: 0,
            detail: format!("one node added and one deleted in a graph of {}", self.nodes),
            child_layouts_per_frame: 0.0,
            builds_per_frame: self.add_builds,
            far_repaints_per_frame: 0.0,
            extra: vec![
                ("add_edge_scans", self.add_scans),
                ("delete_edge_scans", self.delete_scans),
                ("deleted_node_degree", self.degree),
                ("names_grown_per_cycles", self.names_grown as f64),
                ("restored_wrong", self.restored_wrong as f64),
                ("links_lost", self.links_lost as f64),
                ("unseen_edits", self.unseen_edits as f64),
                ("compactions", self.compactions as f64),
                ("first_add_ms", self.first_add_ms),
                ("delete_ms", self.delete_ms),
            ],
        }
    }
}

/// The criteria of §43, all on counters.
pub(crate) fn criteria(rows: &[EditRow]) -> Vec<Criterion> {
    if rows.is_empty() {
        return Vec::new();
    }
    let worst = |f: fn(&EditRow) -> f64| rows.iter().map(f).fold(0.0_f64, f64::max);

    vec![
        // An edit touches what it touches. If this followed the graph, the adjacency
        // would be being rebuilt on every edit — which is what the packed form used to
        // require, and the reason the overflow lists exist.
        Criterion {
            name: "an_edit_does_not_walk_the_graph",
            claim: "adding a node and a link costs the same at any graph size",
            kind: Kind::Counter,
            measured: worst(|row| row.add_scans),
            bound: 16.0,
            unit: "link names walked/edit",
        },
        // A deletion is the one edit that legitimately costs something: its own links.
        // The bound is a multiple of the degree rather than a constant, because the
        // graph is generated with about four links a node and a star would be different.
        Criterion {
            name: "a_removal_costs_its_own_links",
            claim: "deleting a node walks its own links, not the graph's",
            kind: Kind::Counter,
            measured: worst(|row| row.delete_scans / (row.degree + 1.0)),
            bound: 8.0,
            unit: "link names walked/link",
        },
        // The new node is inside the viewport, so it gets exactly one widget. More would
        // mean the edit invalidated widgets that had nothing to do with it.
        Criterion {
            name: "an_edit_builds_one_widget",
            claim: "adding a node builds a widget for it and for nothing else",
            kind: Kind::Counter,
            measured: worst(|row| row.add_builds),
            bound: 2.0,
            unit: "widgets built/edit",
        },
        // §41.2, counted: a node that comes back has to come back as itself.
        Criterion {
            name: "a_name_survives_delete_and_undo",
            claim: "undo restores a node under its own name, in its own place",
            kind: Kind::Counter,
            measured: worst(|row| row.restored_wrong as f64),
            bound: 1.0,
            unit: "nodes back in the wrong place",
        },
        Criterion {
            name: "links_come_back_with_their_node",
            claim: "undo restores the links the delete took",
            kind: Kind::Counter,
            measured: worst(|row| row.links_lost as f64),
            bound: 1.0,
            unit: "nodes back with fewer links",
        },
        // Without a free list the names grow with the number of edits a session makes
        // rather than with the number of nodes it holds, and every array keyed by name
        // grows with them.
        Criterion {
            name: "a_freed_name_is_handed_out_again",
            claim: "add-and-delete cycles do not grow the name space",
            kind: Kind::Counter,
            measured: worst(|row| row.names_grown as f64),
            bound: 1.0,
            unit: "names added by 50 cycles",
        },
        // The recorded sets are chosen for a region and re-chosen when the view leaves
        // it; an edit is not the view moving, so it has to say so itself (§28.4).
        Criterion {
            name: "an_edit_reaches_the_recorded_sets",
            claim: "a link added or removed changes what is drawn in the same frame",
            kind: Kind::Counter,
            measured: worst(|row| row.unseen_edits as f64),
            bound: 1.0,
            unit: "edits the drawn set did not notice",
        },
    ]
}
