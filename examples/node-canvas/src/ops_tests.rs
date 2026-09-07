//! What the operator layer has to get right (§38).
//!
//! The benchmark says what a gesture costs; these say that the gesture is the same
//! gesture however it was started. The two claims §11 rests on are here as tests
//! rather than as intentions: a key and a script produce the same model state, and
//! undo puts the model back exactly — compared as state, not as a picture.

use std::collections::BTreeSet;

use blazy_canvas::CanvasLayer;
use blazy_ops::OpResult;
use blazy_ops::keymap::Props;
use masonry::core::keyboard::{Code, Key, KeyState, KeyboardEvent, Modifiers, NamedKey};
use masonry::core::{NewWidget, TextEvent};
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Point, Vec2};
use masonry::testing::{TestHarness, TestHarnessParams};
use masonry::theme::default_property_set;
use masonry::ui_events::pointer::PointerButton;

use crate::CanvasSpec;
use crate::editor::NodeEditor;
use crate::model::{NODE_SIZE, NodeState, SharedGraph};

/// A harness whose editor drives operators, with the keymap listening for keys.
///
/// The focus fallback is what the host does at startup (`ShellDriver::started`): a
/// keymap has to hear the keys that no focused widget claimed, and Masonry sends a
/// key to the focused widget or to the fallback and nowhere else.
fn ops_harness(count: usize) -> (TestHarness<NodeEditor>, SharedGraph) {
    let (canvas, graph) = CanvasSpec::new(count).build();
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::with_ops(canvas, &graph)),
        PhysicalSize::new(1100, 750),
    );
    let _ = harness.redraw();
    let root = harness.root_id();
    harness.set_focus_fallback(Some(root));
    (harness, graph)
}

/// Where a node's header sits on screen. The view starts at identity, so canvas
/// coordinates are window coordinates until something zooms.
fn node_grab(harness: &mut TestHarness<NodeEditor>, index: usize) -> Point {
    let pos = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::child_pos(&mut canvas, index).expect("the node exists")
        })
    });
    Point::new(pos.x + NODE_SIZE.width / 2.0, pos.y + 8.0)
}

/// A point of the viewport with no node under it.
///
/// Searched rather than written down: the generated grid is jittered, so where the
/// gaps are depends on the node count, and a hard-coded point is a test that starts
/// failing for a reason that has nothing to do with what it checks.
fn empty_spot(harness: &mut TestHarness<NodeEditor>) -> Point {
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

fn key(harness: &mut TestHarness<NodeEditor>, key: Key, mods: Modifiers) {
    harness.process_text_event(TextEvent::Keyboard(KeyboardEvent {
        state: KeyState::Down,
        key,
        code: Code::Unidentified,
        modifiers: mods,
        ..KeyboardEvent::default()
    }));
}

fn character(harness: &mut TestHarness<NodeEditor>, name: &str) {
    key(harness, Key::Character(name.into()), Modifiers::empty());
}

fn selection(harness: &TestHarness<NodeEditor>) -> BTreeSet<usize> {
    harness.root_widget().selection()
}

fn snapshot(graph: &SharedGraph) -> Vec<NodeState> {
    graph.borrow().snapshot()
}

/// Selects `index` the way a user does: a right-click on it.
fn click_node(harness: &mut TestHarness<NodeEditor>, index: usize) {
    let at = node_grab(harness, index);
    harness.mouse_move(at);
    harness.mouse_button_press(Some(PointerButton::Secondary));
    harness.mouse_button_release(Some(PointerButton::Secondary));
    let _ = harness.redraw();
}

/// A node the canvas has materialised, so that a click on it lands on a real widget.
fn visible_node(harness: &mut TestHarness<NodeEditor>) -> usize {
    let live = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::live_children(&mut canvas))
    });
    assert!(!live.is_empty(), "the canvas shows something");
    live[live.len() / 2].0
}

#[test]
fn a_right_click_on_a_node_selects_it() {
    let (mut harness, _graph) = ops_harness(500);
    let index = visible_node(&mut harness);
    click_node(&mut harness, index);
    assert_eq!(selection(&harness), BTreeSet::from([index]));
    assert_eq!(harness.root_widget().modal_depth(), 0, "a click is not modal");
}

/// The left button drags the node under it — and takes it into the selection on the
/// way, because a drag that moved something the user had not selected would be worse
/// than one that refused.
#[test]
fn a_left_drag_moves_the_node_under_the_pointer() {
    let (mut harness, graph) = ops_harness(500);
    let index = visible_node(&mut harness);
    let before = graph.borrow().node(index).pos;

    let at = node_grab(&mut harness, index);
    harness.mouse_move(at);
    harness.mouse_button_press(Some(PointerButton::Primary));
    // The press starts nothing now: it is held until the gesture says what it is
    // (§39.3). Under §38 this assertion read `modal_depth() == 1`.
    assert_eq!(
        harness.root_widget().modal_depth(),
        0,
        "a press on its own is not a drag"
    );
    assert!(harness.root_widget().is_holding(), "it is being held");
    harness.mouse_move(at + Vec2::new(35.0, 20.0));
    assert_eq!(harness.root_widget().modal_depth(), 1, "the move made it a drag");
    harness.mouse_button_release(Some(PointerButton::Primary));
    let _ = harness.redraw();

    assert_eq!(harness.root_widget().modal_depth(), 0, "the release confirmed it");
    assert_eq!(graph.borrow().node(index).pos, before + Vec2::new(35.0, 20.0));
    assert_eq!(selection(&harness), BTreeSet::from([index]), "and it is selected");
    // Nothing reached the tree first: a press-started operator holds the capture.
    assert_eq!(harness.root_widget().op_counters().tree_first, 0);
}

/// Where a node sits **on screen**, which is not where it sits on the canvas once the
/// view has moved. [`node_grab`] answers the canvas question; a test that drives the
/// pointer needs this one.
fn node_grab_screen(harness: &mut TestHarness<NodeEditor>, index: usize) -> Point {
    let canvas_pos = node_grab(harness, index);
    let view =
        harness.edit_root_widget(|mut editor| NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.view()));
    view * canvas_pos
}

/// The same drag, on a view that is not the identity.
///
/// The regression that made this a test: a gesture's anchor and the pointer it is
/// compared against have to be in the same space, and there are two spaces here —
/// canvas and window. With the view at the identity they coincide, so a test that only
/// ever drags an unpanned canvas cannot tell them apart, and the node jumps by the pan
/// on the first event of every drag in the real window.
#[test]
fn a_drag_is_anchored_where_the_press_was_on_a_panned_view() {
    let (mut harness, graph) = ops_harness(500);
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::pan(&mut canvas, Vec2::new(120.0, -80.0));
        });
    });
    let _ = harness.redraw();

    let index = visible_node(&mut harness);
    let before = graph.borrow().node(index).pos;
    let at = node_grab_screen(&mut harness, index);
    harness.mouse_move(at);
    harness.mouse_button_press(Some(PointerButton::Primary));
    harness.mouse_move(at + Vec2::new(35.0, 20.0));
    harness.mouse_button_release(Some(PointerButton::Primary));
    let _ = harness.redraw();

    assert_eq!(
        graph.borrow().node(index).pos,
        before + Vec2::new(35.0, 20.0),
        "the node follows the pointer, and does not jump by the pan first"
    );
}

/// The rubber band, on the same panned view: it begins at the press.
#[test]
fn a_band_starts_where_the_press_was_on_a_panned_view() {
    let (mut harness, graph) = ops_harness(500);
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::pan(&mut canvas, Vec2::new(120.0, -80.0));
        });
    });
    let _ = harness.redraw();

    // A band drawn tightly around one node takes that node and nothing else. Drawn from
    // an anchor that is off by the pan, it takes whatever happens to be over there.
    let index = visible_node(&mut harness);
    let at = node_grab_screen(&mut harness, index);
    let from = at + Vec2::new(-30.0, -20.0);
    harness.mouse_move(from);
    harness.mouse_button_press(Some(PointerButton::Secondary));
    harness.mouse_move(at + Vec2::new(30.0, 30.0));
    harness.mouse_button_release(Some(PointerButton::Secondary));
    let _ = harness.redraw();

    let _ = graph;
    // Exactly that node: a band anchored a pan away from the press is not empty, it is
    // *huge* — it stretches from wherever the anchor landed to the pointer and sweeps up
    // everything in between, which is why "did it select the node" is not the question.
    assert_eq!(
        selection(&harness),
        BTreeSet::from([index]),
        "a tight band takes one node"
    );
}

/// Panning on a HiDPI display moves the view by what the pointer did, and not by more.
///
/// The other half of the same regression: a gesture's anchor came from the event's
/// *physical* position while the operator compared it against a logical one, so the
/// first event of a pan jumped by roughly the click position times the scale factor
/// minus one. At scale 1 the two spaces coincide and nothing is visible, which is why
/// this test sets one.
#[test]
fn a_pan_on_a_scaled_display_moves_by_what_the_pointer_did() {
    let (canvas, graph) = CanvasSpec::new(500).build();
    let mut harness = TestHarness::create_with(
        default_property_set(),
        NewWidget::new(NodeEditor::with_ops(canvas, &graph)),
        {
            let mut params = TestHarnessParams::default();
            params.window_size = PhysicalSize::new(1100, 750);
            params.scale_factor = 2.0;
            params
        },
    );
    let _ = harness.redraw();
    let root = harness.root_id();
    harness.set_focus_fallback(Some(root));

    // The harness takes *physical* positions, and at scale 2 they are twice the logical
    // ones the canvas answers hit tests in — so the gesture is driven in physical
    // coordinates and expected to move the view by half of what it travelled.
    let from = empty_spot(&mut harness);
    let physical = |p: Point| Point::new(p.x * 2.0, p.y * 2.0);
    harness.mouse_move(physical(from));
    harness.mouse_button_press(Some(PointerButton::Primary));
    harness.mouse_move(physical(from) + Vec2::new(80.0, 60.0));
    harness.mouse_button_release(Some(PointerButton::Primary));
    let _ = harness.redraw();

    let view =
        harness.edit_root_widget(|mut editor| NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.view()));
    assert_eq!(
        view.translation(),
        Vec2::new(40.0, 30.0),
        "the view moved by the pointer's travel, not by the scale factor"
    );
}

/// The status line reports what the runtime decided, and a double click is in it.
///
/// The one path from a real pointer to `count >= 2` that a test can walk: the harness
/// fills `PointerState::time` the way a backend does, the runtime resolves, and the HUD
/// prints. What no test can check is whether *this platform's* timestamps are usable —
/// that is what the line is in the window for (§39.2).
#[test]
fn the_status_line_reports_a_double_click() {
    let (mut harness, _graph) = ops_harness(500);
    let empty = empty_spot(&mut harness);

    harness.mouse_move(empty);
    for _ in 0..2 {
        harness.mouse_button_press(Some(PointerButton::Primary));
        harness.mouse_button_release(Some(PointerButton::Primary));
    }
    let _ = harness.redraw();

    let counters = harness.root_widget().op_counters();
    assert_eq!(counters.clicks, 2, "two clicks");
    assert_eq!(counters.double_clicks, 1, "the second one was a double");
    assert!(
        harness.root_widget().hud().contains("clicks 2 (double 1)"),
        "the status line says so, got {:?}",
        harness.root_widget().hud()
    );
}

/// A left click on empty canvas means "nothing selected".
///
/// Decided by the operator that holds the gesture rather than by the keymap, and that
/// is forced rather than chosen: capture is granted during the press (§38.1), so an
/// operator that may have to hold the pointer must start before anyone can know whether
/// the press will turn into a drag.
#[test]
fn a_left_click_on_empty_canvas_clears_the_selection() {
    let (mut harness, _graph) = ops_harness(500);
    let index = visible_node(&mut harness);
    click_node(&mut harness, index);
    assert_eq!(selection(&harness), BTreeSet::from([index]));

    let empty = empty_spot(&mut harness);
    harness.mouse_move(empty);
    harness.mouse_button_press(Some(PointerButton::Primary));
    harness.mouse_button_release(Some(PointerButton::Primary));
    let _ = harness.redraw();

    assert!(selection(&harness).is_empty(), "a click on nothing selects nothing");
    assert_eq!(harness.root_widget().modal_depth(), 0);
    // …and it is a step of its own, so the selection comes back.
    key(&mut harness, Key::Character("z".into()), Modifiers::CONTROL);
    let _ = harness.redraw();
    assert_eq!(selection(&harness), BTreeSet::from([index]), "undo brings it back");
}

/// A left *drag* on empty canvas pans instead, and leaves the selection alone.
///
/// The other half of the switch (§28.4): an operator that cleared the selection at the
/// end of every gesture would pass the test above and lose the selection on every pan.
#[test]
fn a_left_drag_on_empty_canvas_keeps_the_selection() {
    let (mut harness, _graph) = ops_harness(500);
    let index = visible_node(&mut harness);
    click_node(&mut harness, index);

    let from = empty_spot(&mut harness);
    harness.mouse_move(from);
    harness.mouse_button_press(Some(PointerButton::Primary));
    harness.mouse_move(from + Vec2::new(30.0, 20.0));
    harness.mouse_button_release(Some(PointerButton::Primary));
    let _ = harness.redraw();

    assert_eq!(
        selection(&harness),
        BTreeSet::from([index]),
        "panning is not a selection gesture"
    );
}

/// The left button on empty canvas drags the view, through an operator like everything
/// else — the view is the widget's, so the operator asks and the driver carries it in.
#[test]
fn a_left_drag_on_empty_canvas_pans_the_view() {
    let (mut harness, graph) = ops_harness(500);
    let index = visible_node(&mut harness);
    let before_pos = graph.borrow().node(index).pos;
    let before_screen = node_grab(&mut harness, index);

    let from = empty_spot(&mut harness);
    harness.mouse_move(from);
    harness.mouse_button_press(Some(PointerButton::Primary));
    harness.mouse_move(from + Vec2::new(40.0, 30.0));
    harness.mouse_button_release(Some(PointerButton::Primary));
    let _ = harness.redraw();

    assert_eq!(harness.root_widget().modal_depth(), 0);
    assert_eq!(
        graph.borrow().node(index).pos,
        before_pos,
        "panning moves the view, not the graph"
    );
    let after_screen = node_grab(&mut harness, index);
    assert_eq!(after_screen, before_screen, "and not the canvas-space geometry either");
    let zoom =
        harness.edit_root_widget(|mut editor| NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.view()));
    assert_eq!(
        zoom.translation(),
        Vec2::new(40.0, 30.0),
        "the view moved by what the pointer did"
    );
}

/// Two bindings share the primary button and the poll tells them apart: over a node
/// the select operator takes it, over empty canvas it refuses and the box select gets
/// its turn. That is the mechanism §11 describes, and it is worth a test because the
/// alternative — one operator with an `if` in it — passes every other assertion here.
#[test]
fn poll_decides_which_of_two_bindings_runs() {
    let (mut harness, _graph) = ops_harness(500);
    let index = visible_node(&mut harness);
    click_node(&mut harness, index);
    assert_eq!(
        harness.root_widget().op_counters().refused,
        0,
        "over a node, the first binding on the right button ran"
    );

    let empty = empty_spot(&mut harness);
    let before = harness.root_widget().op_counters().refused;
    harness.mouse_move(empty);
    harness.mouse_button_press(Some(PointerButton::Secondary));
    harness.mouse_move(empty + Vec2::new(20.0, 10.0));
    assert!(
        harness.root_widget().op_counters().refused > before,
        "over empty canvas the select operator refused"
    );
    assert_eq!(harness.root_widget().modal_depth(), 1, "and the box select started");
    harness.mouse_button_release(Some(PointerButton::Secondary));
    let _ = harness.redraw();

    // The same on the left button, where the two bindings are move and pan.
    let before = harness.root_widget().op_counters().refused;
    harness.mouse_move(empty);
    harness.mouse_button_press(Some(PointerButton::Primary));
    harness.mouse_move(empty + Vec2::new(20.0, 10.0));
    assert!(
        harness.root_widget().op_counters().refused > before,
        "over empty canvas the move operator refused"
    );
    assert_eq!(harness.root_widget().modal_depth(), 1, "and the pan started");
    harness.mouse_button_release(Some(PointerButton::Primary));
}

/// A band drawn over nodes selects them, and the gesture ends.
#[test]
fn a_box_select_takes_what_it_covers() {
    let (mut harness, graph) = ops_harness(500);
    let start = empty_spot(&mut harness);
    let corner = {
        // A rectangle from the empty spot up over the middle of the viewport, so it
        // certainly covers nodes.
        Point::new(600.0, 200.0)
    };
    harness.mouse_move(start);
    harness.mouse_button_press(Some(PointerButton::Secondary));
    harness.mouse_move(corner);
    harness.mouse_button_release(Some(PointerButton::Secondary));
    let _ = harness.redraw();

    let selected = selection(&harness);
    assert!(!selected.is_empty(), "the band covered nodes");
    assert_eq!(harness.root_widget().modal_depth(), 0, "the gesture ended");

    // Every selected node really is inside the band, checked against the model rather
    // than against whatever the canvas had materialised.
    let band = masonry::kurbo::Rect::from_points(start, corner);
    for index in selected {
        let rect = masonry::kurbo::Rect::from_origin_size(graph.borrow().node(index).pos, NODE_SIZE);
        let overlap = rect.intersect(band);
        assert!(
            overlap.width() > 0.0 && overlap.height() > 0.0,
            "node {index} at {rect:?} is not inside {band:?}"
        );
    }
}

/// The claim §11 is built on, at the level of a real gesture: a grab driven by the
/// keyboard and the pointer leaves the model exactly where the scripted call does.
#[test]
fn a_grab_and_a_scripted_move_agree() {
    let delta = Vec2::new(30.0, -18.0);

    let (mut interactive, graph_a) = ops_harness(500);
    let index = visible_node(&mut interactive);
    click_node(&mut interactive, index);
    let start = node_grab(&mut interactive, index);
    interactive.mouse_move(start);
    character(&mut interactive, "g");
    interactive.mouse_move(start + delta);
    interactive.mouse_button_press(Some(PointerButton::Primary));
    let _ = interactive.redraw();
    assert_eq!(interactive.root_widget().modal_depth(), 0, "the grab was confirmed");

    let (mut scripted, graph_b) = ops_harness(500);
    click_node(&mut scripted, index);
    let result = scripted.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "node.move",
            &Props::new().with_float("dx", delta.x).with_float("dy", delta.y),
        )
    });
    assert_eq!(result, OpResult::Finished);
    let _ = scripted.redraw();

    assert_eq!(
        snapshot(&graph_a),
        snapshot(&graph_b),
        "the key and the script must leave the same model"
    );
    // …and the view must have followed, not just the model.
    let viewed = scripted.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::child_pos(&mut canvas, index))
    });
    assert_eq!(viewed, Some(graph_b.borrow().node(index).pos));
}

/// Undo has to restore the model, not something that looks like it. Compared as
/// state — every node, every field — because a picture would agree with a move that
/// was undone to the wrong place by a fraction of a pixel.
#[test]
fn undo_and_redo_return_the_model_exactly() {
    let (mut harness, graph) = ops_harness(500);
    let index = visible_node(&mut harness);
    let before_everything = snapshot(&graph);

    click_node(&mut harness, index);
    let at = node_grab(&mut harness, index);
    harness.mouse_move(at);
    character(&mut harness, "g");
    harness.mouse_move(at + Vec2::new(40.0, 25.0));
    harness.mouse_button_press(Some(PointerButton::Primary));
    let _ = harness.redraw();
    let after_move = snapshot(&graph);
    assert_ne!(before_everything, after_move, "the grab moved something");

    // Two steps in the history: the selection and the move.
    assert_eq!(harness.root_widget().history_depth(), 2);

    key(&mut harness, Key::Character("z".into()), Modifiers::CONTROL);
    let _ = harness.redraw();
    assert_eq!(snapshot(&graph), before_everything, "undo restores the model exactly");
    assert!(
        selection(&harness).contains(&index),
        "the move was undone, not the click"
    );

    key(&mut harness, Key::Character("z".into()), Modifiers::CONTROL);
    let _ = harness.redraw();
    assert!(selection(&harness).is_empty(), "and then the selection");

    key(
        &mut harness,
        Key::Character("z".into()),
        Modifiers::CONTROL | Modifiers::SHIFT,
    );
    key(
        &mut harness,
        Key::Character("z".into()),
        Modifiers::CONTROL | Modifiers::SHIFT,
    );
    let _ = harness.redraw();
    assert_eq!(snapshot(&graph), after_move, "redo puts it back exactly");
    assert_eq!(selection(&harness), BTreeSet::from([index]));

    // The view followed the undo as well as the model: a node whose widget is on
    // screen has to be moved back, not left where the drag put it.
    let viewed = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::child_pos(&mut canvas, index))
    });
    assert_eq!(viewed, Some(graph.borrow().node(index).pos));
}

/// Cancelling is as important as finishing (§28.4): a sweep that only ever confirms
/// tests half a switch.
#[test]
fn escape_cancels_a_grab_and_leaves_nothing_running() {
    let (mut harness, graph) = ops_harness(500);
    let index = visible_node(&mut harness);
    click_node(&mut harness, index);
    let before = snapshot(&graph);

    let at = node_grab(&mut harness, index);
    harness.mouse_move(at);
    character(&mut harness, "g");
    harness.mouse_move(at + Vec2::new(55.0, 55.0));
    assert_ne!(snapshot(&graph), before, "the grab is following the pointer");

    key(&mut harness, Key::Named(NamedKey::Escape), Modifiers::empty());
    let _ = harness.redraw();

    assert_eq!(snapshot(&graph), before, "cancelling puts the model back");
    assert_eq!(harness.root_widget().modal_depth(), 0);
    assert_eq!(harness.root_widget().op_counters().modal_cancels, 1);
    assert_eq!(
        harness.root_widget().history_depth(),
        1,
        "a cancelled grab leaves nothing to undo — only the click before it"
    );
}

/// The finding of §38.1, pinned so it cannot change quietly.
///
/// A modal operator started from a *press* holds Masonry's pointer capture, and every
/// following event is delivered straight to the driver: the tree sees none of them.
/// One started from a *key* cannot take capture — Masonry offers it during a press and
/// at no other time — so every event of that gesture reaches the tree first.
#[test]
fn a_gesture_the_runtime_owns_costs_the_tree_nothing() {
    let (mut harness, _graph) = ops_harness(500);
    let start = empty_spot(&mut harness);
    harness.mouse_move(start);
    harness.mouse_button_press(Some(PointerButton::Secondary));
    for step in 1..=4 {
        harness.mouse_move(start + Vec2::new(step as f64 * 20.0, -step as f64 * 15.0));
    }
    harness.mouse_button_release(Some(PointerButton::Secondary));
    let pressed = harness.root_widget().op_counters();
    assert!(pressed.modal_events >= 5, "the operator saw the whole gesture");
    assert_eq!(pressed.tree_first, 0, "and the tree saw none of it");

    let (mut harness, _graph) = ops_harness(500);
    let index = visible_node(&mut harness);
    click_node(&mut harness, index);
    let at = node_grab(&mut harness, index);
    harness.mouse_move(at);
    character(&mut harness, "g");
    for step in 1..=4 {
        harness.mouse_move(at + Vec2::new(step as f64 * 10.0, 0.0));
    }
    harness.mouse_button_press(Some(PointerButton::Primary));
    let keyed = harness.root_widget().op_counters();
    // Under §38 this assertion read `tree_first >= 4`: a key-started operator could not
    // take pointer capture, so every move of it reached the tree first. The layer seat
    // can withhold now, and the two starts cost the same (§39.4).
    assert_eq!(
        keyed.tree_first, 0,
        "a key-started operator leaks nothing now that its seat can withhold"
    );
    assert!(
        keyed.withheld >= 4,
        "and what it does not leak, it withheld: {}",
        keyed.withheld
    );
}

/// The pre-tree seat exists, and it is the layer root's.
///
/// `Layer::capture_pointer_event` is called for every pointer event before the target
/// is computed. It cannot stop one, so the runtime only counts — and the count is what
/// says the seat is real.
#[test]
fn the_pre_tree_seat_sees_every_pointer_event() {
    let (mut harness, _graph) = ops_harness(500);
    let before = harness.root_widget().op_counters();
    harness.mouse_move(Point::new(200.0, 200.0));
    harness.mouse_move(Point::new(240.0, 210.0));
    harness.mouse_button_press(Some(PointerButton::Primary));
    harness.mouse_button_release(Some(PointerButton::Primary));
    let after = harness.root_widget().op_counters();
    let seen = after.seen_first - before.seen_first;
    assert!(seen >= 4, "the layer hook saw the whole gesture, got {seen}");
    // And a click — a press and a release with nothing in between — is withheld from
    // nobody. That is the property that makes holding a press safe (§39.3): what the
    // runtime keeps back is the *moves* in the middle of a gesture, never the press or
    // the release, so a press held in error costs the tree a hover and not a click.
    assert_eq!(
        after.withheld, before.withheld,
        "a click takes nothing away from the tree"
    );
}

/// A grab with nothing selected must not start. The poll is the mechanism, and a
/// criterion counts what it refuses.
#[test]
fn a_grab_with_an_empty_selection_is_refused() {
    let (mut harness, graph) = ops_harness(500);
    let before = snapshot(&graph);
    harness.mouse_move(Point::new(400.0, 300.0));
    character(&mut harness, "g");
    harness.mouse_move(Point::new(460.0, 340.0));
    let _ = harness.redraw();

    assert_eq!(harness.root_widget().modal_depth(), 0, "nothing started");
    assert_eq!(snapshot(&graph), before, "and nothing moved");
    assert!(harness.root_widget().op_counters().refused > 0);
}
