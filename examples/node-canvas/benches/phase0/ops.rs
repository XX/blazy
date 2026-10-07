//! What a gesture costs, and where the operator layer can sit (§38).
//!
//! Its own file because it is a table rather than a scenario: one graph, one gesture
//! per row, and the same counters under each of them. Three things are being compared
//! and they are not variations of one another —
//!
//! * the **seat**: the same grab driven from inside the widget tree and from in front of `RenderRoot`, which is the
//!   choice §11 got wrong and this answers with numbers;
//! * the **start**: a modal operator begun by a press holds Masonry's pointer capture, one begun by a key cannot, and
//!   the difference is a column;
//! * the **shape of undo**: a journal of what changed against a snapshot of the model.

use std::time::Instant;

use blazy::canvas::CanvasLayer;
use blazy::masonry::core::keyboard::{Code, Key, KeyState, KeyboardEvent, Modifiers, NamedKey};
use blazy::masonry::core::{NewWidget, TextEvent};
use blazy::masonry::dpi::PhysicalSize;
use blazy::masonry::kurbo::{Point, Vec2};
use blazy::masonry::testing::TestHarness;
use blazy::masonry::theme::default_property_set;
use blazy::masonry::ui_events::pointer::PointerButton;
use blazy::ops::event::{Device, Sample};
use blazy::ops::keymap::{Props, Scope};
use blazy::ops::runtime::Seat;
use node_canvas::CanvasSpec;
use node_canvas::editor::NodeEditor;
use node_canvas::model::{NODE_SIZE, SharedGraph};
use node_canvas::ops::{CANVAS_SCOPE, EditorWorld, UndoMode};

use crate::bench::{Options, ScenarioRecord, VIEWPORT, stats};

/// Pointer moves in one gesture.
///
/// Twenty rather than two, because the per-event numbers are what the table is read
/// for and a gesture of two events is mostly its start.
const MOVES: usize = 20;

/// Gestures per row.
const REPEATS: usize = 6;

/// Gestures per row in the quick set.
const QUICK_REPEATS: usize = 2;

/// Nodes the undo comparison is measured on.
///
/// Large on purpose: the two shapes of undo cost the same on a small graph, and the
/// whole question is what happens when the graph is the size a node editor is for.
const UNDO_NODES: usize = 20_000;

/// One gesture, measured.
pub(crate) struct OpsRow {
    pub(crate) gesture: &'static str,
    /// Where the driver sat: `tree`, `host`, or `canvas` for the canvas's own gesture.
    pub(crate) seat: &'static str,
    /// Events the gesture consists of.
    pub(crate) events: usize,
    /// Events delivered to a modal operator, per gesture.
    pub(crate) modal_events: f64,
    /// Events that reached the widget tree first while an operator was running.
    pub(crate) tree_first: f64,
    /// Events a pre-tree seat kept from the widget tree, per gesture.
    ///
    /// The mirror of [`tree_first`](Self::tree_first), and the column §39 added: what
    /// used to leak is now withheld, and the two numbers add up to the same gesture.
    pub(crate) withheld: f64,
    /// Nodes laid out, per gesture. The comparison with the canvas's own drag.
    pub(crate) child_layouts: f64,
    /// Layout passes over the canvas content, per gesture.
    ///
    /// The counter a gesture is judged on rather than [`child_layouts`](Self::child_layouts):
    /// a pass over clean children costs almost nothing and leaves that one at zero, so
    /// bounding it would let a gesture ask for layout on every event and still pass —
    /// the same trap `picking_does_not_relayout` was checked against (§25.4).
    pub(crate) content_layouts: f64,
    /// Picks answered, per gesture.
    pub(crate) picks: f64,
    /// Edits of the tree from outside it, per gesture — what the host seat pays.
    pub(crate) probes: f64,
    /// Polls that said no, per gesture.
    pub(crate) refused: f64,
    /// Operators that ran without being polled, over the whole row.
    ///
    /// A total rather than an average: one unpolled run in a table is a defect whether
    /// or not the row it happened in was repeated six times, and dividing it by the
    /// repeats is how it would come out under the bound.
    pub(crate) unpolled: f64,
    /// Operators cancelled, per gesture.
    pub(crate) cancels: f64,
    /// Operators run from the interface, per gesture.
    ///
    /// What says a gesture reached the keymap at all: the wheel used to be the canvas's
    /// own, and a zoom row whose operator never ran would cost exactly what the canvas's
    /// zoom costs — and pass any comparison with it.
    pub(crate) invoked: f64,
    /// Operators still running when the gesture ended. Zero, or the gesture hung.
    pub(crate) left_running: usize,
    /// Milliseconds per gesture, including a redraw per event.
    pub(crate) ms: f64,
}

/// One undo shape, measured.
pub(crate) struct UndoRow {
    pub(crate) mode: &'static str,
    pub(crate) nodes: usize,
    /// Nodes the step moved.
    pub(crate) moved: usize,
    /// What one step holds, by its own reckoning.
    pub(crate) bytes: usize,
    /// Milliseconds to record the step.
    pub(crate) push_ms: f64,
    /// Milliseconds to undo it.
    pub(crate) undo_ms: f64,
}

/// A harness whose editor drives operators, with the keymap hearing keys.
pub(crate) fn ops_harness(count: usize) -> (TestHarness<NodeEditor>, SharedGraph) {
    let (canvas, graph) = CanvasSpec::new(count).build();
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(node_canvas::editor::with_ops(canvas, &graph)),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    let root = harness.root_id();
    harness.set_focus_fallback(Some(root));
    (harness, graph)
}

/// A harness with no operator layer, for the two rows that are not about one.
fn plain_harness(count: usize, builtin_gestures: bool) -> (TestHarness<NodeEditor>, SharedGraph) {
    let (canvas, graph) = CanvasSpec::new(count).build();
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(node_canvas::editor::new(canvas.with_builtin_gestures(builtin_gestures))),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    (harness, graph)
}

/// Where a node's header is on screen. The view starts at identity.
fn grab_point(harness: &mut TestHarness<NodeEditor>, index: usize) -> Point {
    let pos = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::child_pos(&mut canvas, index).expect("the node exists")
        })
    });
    Point::new(pos.x + NODE_SIZE.width / 2.0, pos.y + 8.0)
}

/// A node the canvas has materialised.
fn visible_node(harness: &mut TestHarness<NodeEditor>) -> usize {
    let live = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::live_children(&mut canvas))
    });
    live[live.len() / 2].0
}

/// A point of the viewport with nothing under it.
fn empty_point(harness: &mut TestHarness<NodeEditor>) -> Point {
    for row in 0..12 {
        for column in 0..16 {
            let candidate = Point::new(20.0 + column as f64 * 67.0, 40.0 + row as f64 * 55.0);
            let hit = harness.edit_root_widget(|mut editor| {
                NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::hit_test(&mut canvas, candidate))
            });
            if hit.is_none() {
                return candidate;
            }
        }
    }
    panic!("no empty spot in the viewport");
}

fn press_key(harness: &mut TestHarness<NodeEditor>, key: Key, mods: Modifiers) {
    harness.process_text_event(TextEvent::Keyboard(KeyboardEvent {
        state: KeyState::Down,
        key,
        code: Code::Unidentified,
        modifiers: mods,
        ..KeyboardEvent::default()
    }));
}

/// Counters read from the canvas and from the operator layer, before and after.
struct Before {
    child_layouts: u64,
    content_layouts: u64,
    picks: u64,
    ops: blazy::ops::OpCounters,
}

fn before(harness: &mut TestHarness<NodeEditor>) -> Before {
    let stats = stats(harness);
    Before {
        child_layouts: stats.counters.child_layouts,
        content_layouts: stats.counters.content_layouts,
        picks: stats.counters.hit_queries,
        ops: harness.root_widget().op_counters(),
    }
}

/// Builds a row from what changed over `repeats` gestures.
#[expect(clippy::too_many_arguments, reason = "a row is its columns")]
fn row(
    gesture: &'static str,
    seat: &'static str,
    harness: &mut TestHarness<NodeEditor>,
    start: Before,
    repeats: usize,
    events: usize,
    probes: usize,
    ms: f64,
) -> OpsRow {
    let stats = stats(harness);
    let ops = harness.root_widget().op_counters();
    let per = |delta: u64| delta as f64 / repeats as f64;
    OpsRow {
        gesture,
        seat,
        events,
        modal_events: per(ops.modal_events - start.ops.modal_events),
        tree_first: per(ops.tree_first - start.ops.tree_first),
        withheld: per(ops.withheld - start.ops.withheld),
        child_layouts: per(stats.counters.child_layouts - start.child_layouts),
        content_layouts: per(stats.counters.content_layouts - start.content_layouts),
        picks: per(stats.counters.hit_queries - start.picks),
        probes: probes as f64 / repeats as f64,
        refused: per(ops.refused - start.ops.refused),
        unpolled: (ops.unpolled - start.ops.unpolled) as f64,
        cancels: per(ops.modal_cancels - start.ops.modal_cancels),
        invoked: per(ops.invoked - start.ops.invoked),
        left_running: harness.root_widget().modal_depth(),
        ms: ms / repeats as f64,
    }
}

/// Clicks a node: a press and a release, nothing modal.
///
/// Two nodes alternately, so that every gesture in the row really changes the
/// selection. Clicking the same node twice changes nothing, and a row where half the
/// gestures are no-ops would halve whatever it is measuring — which is exactly how a
/// criterion comes to pass while the thing it guards is broken.
fn click_row(count: usize, repeats: usize) -> OpsRow {
    let (mut harness, _graph) = ops_harness(count);
    let live = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::live_children(&mut canvas))
    });
    let first = grab_point(&mut harness, live[live.len() / 2].0);
    let second = grab_point(&mut harness, live[live.len() / 3].0);
    let start = before(&mut harness);
    let clock = Instant::now();
    for i in 0..repeats {
        harness.mouse_move(if i % 2 == 0 { first } else { second });
        harness.mouse_button_press(Some(PointerButton::Secondary));
        harness.mouse_button_release(Some(PointerButton::Secondary));
        let _ = harness.redraw();
    }
    let ms = clock.elapsed().as_secs_f64() * 1000.0;
    row("click select", "tree", &mut harness, start, repeats, 3, 0, ms)
}

/// A rubber band started by a press: the case pointer capture covers.
fn box_row(count: usize, repeats: usize) -> OpsRow {
    let (mut harness, _graph) = ops_harness(count);
    let from = empty_point(&mut harness);
    let start = before(&mut harness);
    let clock = Instant::now();
    for _ in 0..repeats {
        harness.mouse_move(from);
        harness.mouse_button_press(Some(PointerButton::Secondary));
        for step in 1..=MOVES {
            harness.mouse_move(from + Vec2::new(step as f64 * 12.0, step as f64 * 8.0));
            let _ = harness.redraw();
        }
        harness.mouse_button_release(Some(PointerButton::Secondary));
        let _ = harness.redraw();
    }
    let ms = clock.elapsed().as_secs_f64() * 1000.0;
    row("box select", "tree", &mut harness, start, repeats, MOVES + 3, 0, ms)
}

/// A grab started by `G`: the case pointer capture cannot cover.
///
/// `cancel` ends it with Escape instead of a confirming press, because a sweep that
/// only ever confirms tests half a switch (§28.4).
fn grab_row(gesture: &'static str, count: usize, repeats: usize, cancel: bool) -> OpsRow {
    let (mut harness, _graph) = ops_harness(count);
    let index = visible_node(&mut harness);
    let at = grab_point(&mut harness, index);
    harness.mouse_move(at);
    harness.mouse_button_press(Some(PointerButton::Secondary));
    harness.mouse_button_release(Some(PointerButton::Secondary));
    let _ = harness.redraw();

    let start = before(&mut harness);
    let clock = Instant::now();
    for _ in 0..repeats {
        harness.mouse_move(at);
        press_key(&mut harness, Key::Character("g".into()), Modifiers::empty());
        for step in 1..=MOVES {
            harness.mouse_move(at + Vec2::new(step as f64 * 3.0, 0.0));
            let _ = harness.redraw();
        }
        if cancel {
            press_key(&mut harness, Key::Named(NamedKey::Escape), Modifiers::empty());
        } else {
            harness.mouse_button_press(Some(PointerButton::Primary));
            harness.mouse_button_release(Some(PointerButton::Primary));
        }
        let _ = harness.redraw();
    }
    let ms = clock.elapsed().as_secs_f64() * 1000.0;
    row(gesture, "tree", &mut harness, start, repeats, MOVES + 2, 0, ms)
}

/// The same node dragged with the left button, through an operator.
///
/// The row the comparison with the canvas's own drag is actually made on: same button,
/// same start, same movement — the only difference is who is doing it. Started by a
/// press, so the driver takes pointer capture and the tree sees nothing after the first
/// event.
fn drag_row(count: usize, repeats: usize) -> OpsRow {
    let (mut harness, _graph) = ops_harness(count);
    let index = visible_node(&mut harness);
    let start = before(&mut harness);
    let clock = Instant::now();
    for _ in 0..repeats {
        let at = grab_point(&mut harness, index);
        harness.mouse_move(at);
        harness.mouse_button_press(Some(PointerButton::Primary));
        for step in 1..=MOVES {
            harness.mouse_move(at + Vec2::new(step as f64 * 3.0, 0.0));
            let _ = harness.redraw();
        }
        harness.mouse_button_release(Some(PointerButton::Primary));
        let _ = harness.redraw();
    }
    let ms = clock.elapsed().as_secs_f64() * 1000.0;
    row("drag (LMB)", "tree", &mut harness, start, repeats, MOVES + 2, 0, ms)
}

/// Panning with the left button, which is an operator like everything else.
///
/// Next to the middle-button row, which is the canvas's own pan, so the price of
/// routing a view change through the operator layer is a difference between two lines
/// rather than an opinion.
fn pan_op_row(count: usize, repeats: usize) -> OpsRow {
    let (mut harness, _graph) = ops_harness(count);
    // Found once, before the counters are read: searching for it asks the canvas what
    // is under a point, and those picks are not the gesture's.
    let from = empty_point(&mut harness);
    let start = before(&mut harness);
    let mut ms = 0.0;
    for _ in 0..repeats {
        let clock = Instant::now();
        harness.mouse_move(from);
        harness.mouse_button_press(Some(PointerButton::Primary));
        for step in 1..=MOVES {
            harness.mouse_move(from + Vec2::new(step as f64 * 2.0, 0.0));
            let _ = harness.redraw();
        }
        harness.mouse_button_release(Some(PointerButton::Primary));
        let _ = harness.redraw();
        ms += clock.elapsed().as_secs_f64() * 1000.0;
        reset_view(&mut harness, Vec2::new(-(MOVES as f64) * 2.0, 0.0));
    }
    row("pan (LMB)", "tree", &mut harness, start, repeats, MOVES + 2, 0, ms)
}

/// Puts the view back between gestures, outside the clock.
///
/// A pan leaves the viewport somewhere else, and the next gesture would start over a
/// different part of the graph — with a node under the point that was empty, so the
/// press would start a drag instead. Repeats of a row have to be repeats.
fn reset_view(harness: &mut TestHarness<NodeEditor>, delta: Vec2) {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::pan(&mut canvas, delta));
    });
    let _ = harness.redraw();
}

/// The canvas's own drag, for comparison: the same node moved the same way, by the
/// code the operator layer replaced (§20.5, §30.4).
fn canvas_drag_row(count: usize, repeats: usize) -> OpsRow {
    let (mut harness, _graph) = plain_harness(count, true);
    let index = visible_node(&mut harness);
    let start = before(&mut harness);
    let clock = Instant::now();
    for _ in 0..repeats {
        // Where the node is *now*, not where it started. The drag leaves it twenty
        // steps to the right, and pressing at the old point would land on empty canvas
        // and pan instead — a row that measured a drag once and a pan five times, and
        // then invited a comparison with the operator grab.
        let at = grab_point(&mut harness, index);
        harness.mouse_move(at);
        harness.mouse_button_press(Some(PointerButton::Primary));
        for step in 1..=MOVES {
            harness.mouse_move(at + Vec2::new(step as f64 * 3.0, 0.0));
            let _ = harness.redraw();
        }
        harness.mouse_button_release(Some(PointerButton::Primary));
        let _ = harness.redraw();
    }
    let ms = clock.elapsed().as_secs_f64() * 1000.0;
    row("canvas drag", "canvas", &mut harness, start, repeats, MOVES + 2, 0, ms)
}

/// The same grab, driven from the seat in front of `RenderRoot` (§38.2).
///
/// This is what a host hook can and cannot do, priced. It can withhold any event from
/// the tree, which is the one thing the in-tree seat cannot — and it knows nothing
/// about what the event is over, so every one of them costs a probe:
/// `RenderRoot::edit_widget`, and that runs the whole rewrite battery. The editor here
/// has no operator layer of its own; the runtime lives out here with the driver.
fn host_seat_row(count: usize, repeats: usize) -> OpsRow {
    let (mut harness, graph) = plain_harness(count, false);
    let index = visible_node(&mut harness);
    let at = grab_point(&mut harness, index);

    let mut runtime = node_canvas::ops::runtime();
    let mut world = EditorWorld::new(&graph);
    let mut probes = 0usize;

    // Select the node first, so the grab has something to move. Through the same
    // runtime: the host seat is a driver like any other.
    world.hover = probe(&mut harness, at, &mut probes);
    world.pointer = at;
    runtime.exec(&mut world, "node.select", &Props::new().with_int("index", index as i64));
    apply(&mut harness, &mut world, &mut probes);

    let start = before(&mut harness);
    let clock = Instant::now();
    for _ in 0..repeats {
        // The key that starts the grab. A host seat has no focus to consult, which is
        // its other half: it decides for itself that this key is the keymap's.
        world.pointer = at;
        world.hover = probe(&mut harness, at, &mut probes);
        let key_event = blazy::ops::event::OpEvent::Key {
            key: Key::Character("g".into()),
            mods: Modifiers::empty(),
            down: true,
        };
        runtime.feed(
            &mut world,
            &key_event,
            Sample::default(),
            Scope(&CANVAS_SCOPE),
            Seat::Host,
        );
        apply(&mut harness, &mut world, &mut probes);

        for step in 1..=MOVES {
            let pos = at + Vec2::new(step as f64 * 3.0, 0.0);
            // Every event: probe for the context, then decide. What the tree never
            // gets is the point — a consumed event is not forwarded at all.
            world.hover = probe(&mut harness, pos, &mut probes);
            world.pointer = pos;
            let moved = blazy::ops::event::OpEvent::Move {
                pos,
                mods: Modifiers::empty(),
            };
            let fed = runtime.feed(
                &mut world,
                &moved,
                Sample {
                    time_ns: step as u64 * 8_000_000,
                    device: Device::Mouse,
                    screen: pos,
                },
                Scope(&CANVAS_SCOPE),
                Seat::Host,
            );
            apply(&mut harness, &mut world, &mut probes);
            if !fed.is_consumed() {
                harness.mouse_move(pos);
            }
            let _ = harness.redraw();
        }

        let confirm = blazy::ops::event::OpEvent::Press {
            button: PointerButton::Primary,
            pos: at,
            mods: Modifiers::empty(),
        };
        runtime.feed(
            &mut world,
            &confirm,
            Sample {
                time_ns: 200_000_000,
                device: Device::Mouse,
                screen: at,
            },
            Scope(&CANVAS_SCOPE),
            Seat::Host,
        );
        apply(&mut harness, &mut world, &mut probes);
        let _ = harness.redraw();
    }
    let ms = clock.elapsed().as_secs_f64() * 1000.0;

    let stats = stats(&mut harness);
    let counters = runtime.counters();
    let per = |delta: u64| delta as f64 / repeats as f64;
    OpsRow {
        gesture: "grab (key G)",
        seat: "host",
        events: MOVES + 2,
        modal_events: per(counters.modal_events),
        // Nothing reached the tree first, because nothing reached the tree at all:
        // the host seat forwards only what no operator wanted.
        tree_first: per(counters.tree_first),
        withheld: per(counters.withheld),
        child_layouts: (stats.counters.child_layouts - start.child_layouts) as f64 / repeats as f64,
        content_layouts: (stats.counters.content_layouts - start.content_layouts) as f64 / repeats as f64,
        picks: (stats.counters.hit_queries - start.picks) as f64 / repeats as f64,
        probes: probes as f64 / repeats as f64,
        refused: per(counters.refused),
        unpolled: counters.unpolled as f64,
        cancels: per(counters.modal_cancels),
        invoked: per(counters.invoked),
        left_running: runtime.modal_depth(),
        ms: ms / repeats as f64,
    }
}

/// Asks the tree what is under a point, from outside it.
///
/// One `RenderRoot::edit_widget`, which runs the whole rewrite battery afterwards —
/// the price of the host seat, and the reason it is a column.
fn probe(harness: &mut TestHarness<NodeEditor>, pos: Point, probes: &mut usize) -> Option<blazy::canvas::CanvasHit> {
    *probes += 1;
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::hit_test(&mut canvas, pos))
    })
}

/// Carries what the operators changed into the tree, from outside it.
fn apply(harness: &mut TestHarness<NodeEditor>, world: &mut EditorWorld, probes: &mut usize) {
    if world.moved.is_empty() {
        world.dirty = false;
        return;
    }
    let mut moved = std::mem::take(&mut world.moved);
    moved.sort_unstable();
    moved.dedup();
    let positions: Vec<(usize, Point)> = {
        let graph = world.graph.borrow();
        moved.iter().map(|&index| (index, graph.node(index).pos)).collect()
    };
    *probes += 1;
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            for &(index, pos) in &positions {
                CanvasLayer::move_child(&mut canvas, index, pos);
            }
        });
    });
}

/// The canvas's middle-button pan, which the operator layer does not take over.
///
/// In the table for two reasons. It is the gesture that *does* lay out — a view change
/// re-runs the cull — so it is what makes "selecting lays out nothing" a claim about
/// selecting rather than about the counter being asleep. And it is the evidence that
/// turning the primary button over to the keymap left the rest of the canvas alone.
fn pan_row(count: usize, repeats: usize) -> OpsRow {
    let (mut harness, _graph) = ops_harness(count);
    // The same starting point as the operator pan, chosen the same way: the two rows
    // are only comparable if they pan over the same part of the graph.
    let from = empty_point(&mut harness);
    let start = before(&mut harness);
    let mut ms = 0.0;
    for _ in 0..repeats {
        let clock = Instant::now();
        harness.mouse_move(from);
        harness.mouse_button_press(Some(PointerButton::Auxiliary));
        for step in 1..=MOVES {
            harness.mouse_move(from + Vec2::new(step as f64 * 2.0, 0.0));
            let _ = harness.redraw();
        }
        harness.mouse_button_release(Some(PointerButton::Auxiliary));
        let _ = harness.redraw();
        ms += clock.elapsed().as_secs_f64() * 1000.0;
        reset_view(&mut harness, Vec2::new(-(MOVES as f64) * 2.0, 0.0));
    }
    row("pan (middle)", "canvas", &mut harness, start, repeats, MOVES + 2, 0, ms)
}

/// Wheel notches in one zoom gesture.
pub(crate) const NOTCHES: usize = 6;

/// Zooms in by a few wheel notches, through the keymap or through the canvas's own wheel.
///
/// The pair §38.7 left open: the wheel was the canvas's, and `view.zoom` took it over
/// (`issues/keymap file and zoom operator.md`). The claim is that it costs nothing for
/// having moved — the same passes, no node laid out — and the operator row has to show
/// its operator actually ran, or it is the canvas row twice.
fn zoom_row(count: usize, repeats: usize, through_keymap: bool) -> OpsRow {
    let (mut harness, _graph) = if through_keymap {
        ops_harness(count)
    } else {
        plain_harness(count, true)
    };
    let at = empty_point(&mut harness);
    harness.mouse_move(at);
    let _ = harness.redraw();
    let start = before(&mut harness);
    let mut ms = 0.0;
    for _ in 0..repeats {
        let clock = Instant::now();
        for _ in 0..NOTCHES {
            harness.mouse_wheel(Vec2::new(0.0, -120.0));
            let _ = harness.redraw();
        }
        ms += clock.elapsed().as_secs_f64() * 1000.0;
        // Back to where it was, outside the clock, so every repeat zooms over the same
        // part of the graph.
        harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| {
                CanvasLayer::set_view(&mut canvas, blazy::masonry::kurbo::Affine::IDENTITY);
            });
        });
        let _ = harness.redraw();
    }
    let (gesture, seat) = if through_keymap {
        ("zoom (wheel)", "tree")
    } else {
        ("zoom (wheel)", "canvas")
    };
    row(gesture, seat, &mut harness, start, repeats, NOTCHES, 0, ms)
}

/// The gesture table.
pub(crate) fn ops_table(opts: &Options, count: usize) -> Vec<OpsRow> {
    let repeats = if opts.quick { QUICK_REPEATS } else { REPEATS };
    println!("\noperators: what a gesture costs, and where the layer sits ({count} nodes)");
    let rows = vec![
        click_row(count, repeats),
        drag_row(count, repeats),
        canvas_drag_row(count, repeats),
        box_row(count, repeats),
        grab_row("grab (key G)", count, repeats, false),
        grab_row("grab, cancelled", count, repeats, true),
        pan_op_row(count, repeats),
        pan_row(count, repeats),
        zoom_row(count, repeats, true),
        zoom_row(count, repeats, false),
        host_seat_row(count, repeats),
    ];
    print_ops(&rows);
    rows
}

/// What one undo step holds, in each of the two shapes.
pub(crate) fn undo_table(opts: &Options) -> Vec<UndoRow> {
    let nodes = if opts.quick { UNDO_NODES / 4 } else { UNDO_NODES };
    println!("\nundo: a journal of what changed against a snapshot of the model ({nodes} nodes)");
    let rows = vec![
        undo_case(UndoMode::Journal, "journal", nodes, 1),
        undo_case(UndoMode::Snapshot, "snapshot", nodes, 1),
        undo_case(UndoMode::Journal, "journal", nodes, 200),
        undo_case(UndoMode::Snapshot, "snapshot", nodes, 200),
    ];
    print_undo(&rows);
    rows
}

/// Moves `moved` nodes with one operator and prices the step it leaves behind.
fn undo_case(mode: UndoMode, name: &'static str, nodes: usize, moved: usize) -> UndoRow {
    let (mut harness, _graph) = ops_harness(nodes);
    harness.edit_root_widget(|mut editor| node_canvas::ops::set_undo_mode(&mut editor, mode));
    // Selected through the operator, so the two rows differ in one thing only.
    harness.edit_root_widget(|mut editor| {
        for index in 0..moved {
            NodeEditor::exec(
                &mut editor,
                "node.select",
                &Props::new().with_int("index", index as i64).with_bool("extend", true),
            );
        }
    });

    let props = Props::new().with_float("dx", 12.0).with_float("dy", -7.0);
    let before_bytes = harness.root_widget().history_bytes();
    let clock = Instant::now();
    harness.edit_root_widget(|mut editor| NodeEditor::exec(&mut editor, "node.move", &props));
    let push_ms = clock.elapsed().as_secs_f64() * 1000.0;
    let bytes = harness.root_widget().history_bytes() - before_bytes;

    let clock = Instant::now();
    harness.edit_root_widget(|mut editor| NodeEditor::exec(&mut editor, "ed.undo", &Props::new()));
    let undo_ms = clock.elapsed().as_secs_f64() * 1000.0;

    UndoRow {
        mode: name,
        nodes,
        moved,
        bytes,
        push_ms,
        undo_ms,
    }
}

impl OpsRow {
    pub(crate) fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: "operators",
            frames: self.events,
            mean_ms: self.ms,
            worst_ms: 0.0,
            materialised: 0,
            detail: format!("{} from the {} seat", self.gesture, self.seat),
            child_layouts_per_frame: self.child_layouts,
            builds_per_frame: 0.0,
            far_repaints_per_frame: 0.0,
            extra: vec![
                ("content_layouts_per_gesture", self.content_layouts),
                ("modal_events_per_gesture", self.modal_events),
                ("tree_first_per_gesture", self.tree_first),
                ("withheld_per_gesture", self.withheld),
                ("picks_per_gesture", self.picks),
                ("probes_per_gesture", self.probes),
                ("refused_per_gesture", self.refused),
                ("unpolled_per_gesture", self.unpolled),
                ("cancels_per_gesture", self.cancels),
                ("left_running", self.left_running as f64),
            ],
        }
    }
}

impl UndoRow {
    pub(crate) fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: "undo",
            frames: 1,
            mean_ms: self.push_ms,
            worst_ms: self.undo_ms,
            materialised: 0,
            detail: format!("{} of {} nodes, {} moved", self.mode, self.nodes, self.moved),
            child_layouts_per_frame: 0.0,
            builds_per_frame: 0.0,
            far_repaints_per_frame: 0.0,
            extra: vec![
                ("step_bytes", self.bytes as f64),
                ("moved_nodes", self.moved as f64),
                ("undo_ms", self.undo_ms),
            ],
        }
    }
}

fn print_ops(rows: &[OpsRow]) {
    println!(
        "  {:<17} {:<7} {:>7} {:>8} {:>10} {:>7} {:>9} {:>7} {:>8} {:>8} {:>9}",
        "gesture",
        "seat",
        "events",
        "modal/g",
        "tree-1st/g",
        "held/g",
        "layouts/g",
        "picks/g",
        "probes/g",
        "refused",
        "ms/g"
    );
    for row in rows {
        println!(
            "  {:<17} {:<7} {:>7} {:>8.1} {:>10.1} {:>7.1} {:>9.2} {:>7.1} {:>8.1} {:>8.1} {:>9.3}",
            row.gesture,
            row.seat,
            row.events,
            row.modal_events,
            row.tree_first,
            row.withheld,
            row.content_layouts,
            row.picks,
            row.probes,
            row.refused,
            row.ms,
        );
    }
}

fn print_undo(rows: &[UndoRow]) {
    println!(
        "  {:<10} {:>8} {:>8} {:>12} {:>10} {:>10}",
        "shape", "nodes", "moved", "step bytes", "push ms", "undo ms"
    );
    for row in rows {
        println!(
            "  {:<10} {:>8} {:>8} {:>12} {:>10.3} {:>10.3}",
            row.mode, row.nodes, row.moved, row.bytes, row.push_ms, row.undo_ms,
        );
    }
}
