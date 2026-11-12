//! The editor against a graph of its own, with no example behind it.
//!
//! The example's tests exercise the driver in depth against its own graph; these pin
//! down what the crate promises to any graph: the seam is enough to select, move and
//! undo, a script and a key end in the same state, and the move recorder is honoured.

use std::cell::RefCell;
use std::rc::Rc;

use blazy_canvas::CanvasLayer;
use blazy_ops::OpResult;
use blazy_ops::keymap::Props;
use blazy_ops::undo::Step;
use masonry::core::NewWidget;
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Point, Rect, Size};
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;
use masonry::widgets::Label;

use crate::{EditorWorld, Link, MoveRecord, NodeEditor, NodeGraph, SharedGraph, Views};

/// The smallest graph the seam allows: rectangles in a row, a number in each, its links,
/// and the views showing it.
///
/// Names are indices with holes, and the holes are handed out again before fresh names
/// — the free list of §41.2, which is what `insert_node` asks an application for. A
/// removed node's number is wiped with its rectangle, so whatever brings it back has to
/// have kept it.
#[derive(Default)]
struct Row {
    rects: Vec<Option<Rect>>,
    values: Vec<u32>,
    free: Vec<usize>,
    links: Vec<Link>,
    views: Views,
    /// A node no link may go into: the port type the test graph has.
    refuses: Option<u32>,
    /// How many links an input holds, when not the default one.
    holds: Option<usize>,
}

impl NodeGraph for Row {
    fn node_count(&self) -> usize {
        self.rects.iter().flatten().count()
    }

    fn node_rect(&self, index: usize) -> Rect {
        self.rects[index].unwrap_or_default()
    }

    fn set_node_pos(&mut self, index: usize, pos: Point) {
        if let Some(rect) = self.rects[index] {
            self.rects[index] = Some(Rect::from_origin_size(pos, rect.size()));
        }
    }

    fn views(&self) -> &Views {
        &self.views
    }

    fn insert_node(&mut self, rect: Rect) -> usize {
        self.insert_copy(rect, 0)
    }

    fn restore_node(&mut self, index: usize, rect: Rect, value: u32) {
        if self.rects.len() <= index {
            self.rects.resize(index + 1, None);
            self.values.resize(index + 1, 0);
        }
        if let Some(at) = self.free.iter().position(|&free| free == index) {
            self.free.swap_remove(at);
        }
        self.rects[index] = Some(rect);
        self.values[index] = value;
    }

    fn remove_node(&mut self, index: usize) -> Vec<Link> {
        if self.rects.get(index).copied().flatten().is_none() {
            return Vec::new();
        }
        self.rects[index] = None;
        self.values[index] = 0;
        self.free.push(index);
        let (gone, kept): (Vec<Link>, Vec<Link>) = self
            .links
            .iter()
            .partition(|link| link.from as usize == index || link.to as usize == index);
        self.links = kept;
        gone
    }

    fn insert_link(&mut self, link: Link) -> bool {
        let live = |i: u32| self.rects.get(i as usize).copied().flatten().is_some();
        if !live(link.from) || !live(link.to) || link.from == link.to {
            return false;
        }
        if self.links.iter().any(|&other| same_link(other, link)) {
            return false;
        }
        self.links.push(link);
        true
    }

    fn remove_link(&mut self, link: Link) {
        self.links.retain(|&other| !same_link(other, link));
    }

    fn accepts_link(&self, link: Link) -> bool {
        self.refuses != Some(link.to)
    }

    fn input_limit(&self, _index: usize, _port: u16) -> usize {
        self.holds.unwrap_or(1)
    }

    type Content = u32;

    fn content(&self, index: usize) -> u32 {
        self.values[index]
    }

    fn insert_copy(&mut self, rect: Rect, value: u32) -> usize {
        match self.free.pop() {
            Some(index) => {
                self.rects[index] = Some(rect);
                self.values[index] = value;
                index
            },
            None => {
                self.rects.push(Some(rect));
                self.values.push(value);
                self.rects.len() - 1
            },
        }
    }

    fn links_of(&self, index: usize) -> Vec<Link> {
        let ends_here = |link: &&Link| link.from as usize == index || link.to as usize == index;
        self.links.iter().filter(ends_here).copied().collect()
    }
}

/// Two links are the same link whichever way round they are written.
fn same_link(a: Link, b: Link) -> bool {
    a == b || a == b.reversed()
}

const SIZE: Size = Size::new(100.0, 60.0);

fn row(count: usize) -> SharedGraph<Row> {
    let rects = (0..count)
        .map(|i| Some(Rect::from_origin_size(Point::new(i as f64 * 150.0, 0.0), SIZE)))
        .collect();
    Rc::new(RefCell::new(Row {
        rects,
        values: (0..count as u32).map(|i| 100 + i).collect(),
        ..Row::default()
    }))
}

fn origin(graph: &SharedGraph<Row>, index: usize) -> Point {
    graph.borrow().node_rect(index).origin()
}

fn select(runtime: &mut blazy_ops::runtime::OpRuntime<EditorWorld<Row>>, world: &mut EditorWorld<Row>, index: i64) {
    let result = runtime.exec(world, "node.select", &Props::new().with_int("index", index));
    assert_eq!(result, OpResult::Finished);
}

/// Select, move, undo, redo — the whole cycle through the seam and nothing else.
#[test]
fn a_move_goes_through_the_graph_and_comes_back_on_undo() {
    let graph = row(3);
    let mut runtime = crate::ops::runtime();
    let mut world = EditorWorld::new(&graph);

    select(&mut runtime, &mut world, 1);
    let before = origin(&graph, 1);
    let moved = runtime.exec(
        &mut world,
        "node.move",
        &Props::new().with_float("dx", 30.0).with_float("dy", -5.0),
    );
    assert_eq!(moved, OpResult::Finished);
    assert_eq!(origin(&graph, 1), before + (30.0, -5.0));
    assert_eq!(origin(&graph, 0), Point::ORIGIN, "only the selection moved");
    assert!(world.moved.contains(&1), "the views are told which node to follow");

    assert_eq!(runtime.history_mut().undo(&mut world), Some("node.move"));
    assert_eq!(origin(&graph, 1), before);
    assert_eq!(runtime.history_mut().redo(&mut world), Some("node.move"));
    assert_eq!(origin(&graph, 1), before + (30.0, -5.0));
}

/// A box select finds nodes through the graph, including ones no view has built.
#[test]
fn a_box_selects_what_it_covers_in_the_graph() {
    let graph = row(5);
    let mut runtime = crate::ops::runtime();
    let mut world = EditorWorld::new(&graph);

    let band = Props::new()
        .with_float("x0", 120.0)
        .with_float("y0", 10.0)
        .with_float("x1", 320.0)
        .with_float("y1", 20.0);
    assert_eq!(runtime.exec(&mut world, "node.box_select", &band), OpResult::Finished);
    assert_eq!(world.selection().iter().copied().collect::<Vec<_>>(), [1, 2]);
}

/// The recorder is the application's to replace, and a finished move goes through it.
#[test]
fn a_finished_move_goes_through_the_recorder() {
    struct Tagged;
    impl Step<EditorWorld<Row>> for Tagged {
        fn name(&self) -> &'static str {
            "tagged.move"
        }
        fn undo(&mut self, _world: &mut EditorWorld<Row>) {}
        fn redo(&mut self, _world: &mut EditorWorld<Row>) {}
    }
    fn tagged(_world: &EditorWorld<Row>, record: MoveRecord) -> Box<dyn Step<EditorWorld<Row>>> {
        assert_eq!(record.nodes, [0]);
        assert_eq!(record.to[0], record.from[0] + (10.0, 0.0));
        Box::new(Tagged)
    }

    let graph = row(2);
    let mut runtime = crate::ops::runtime();
    let mut world = EditorWorld::new(&graph);
    world.record_move = tagged;

    select(&mut runtime, &mut world, 0);
    runtime.exec(&mut world, "node.move", &Props::new().with_float("dx", 10.0));
    assert_eq!(runtime.history_mut().undo(&mut world), Some("tagged.move"));
}

/// The widget: an operator run through it reaches the canvas, not only the graph.
///
/// The half of the driver a world-level test cannot see — carrying the moved node into
/// the canvas's own copy of the geometry (§30).
#[test]
fn an_operator_run_through_the_editor_moves_the_canvas_too() {
    let graph = row(3);
    let geometry = {
        let graph = graph.clone();
        move |index: usize| {
            let rect = graph.borrow().node_rect(index);
            Some((rect.origin(), rect.size()))
        }
    };
    let source = |index: usize, _detail| NewWidget::new(Label::new(format!("node {index}"))).erased();
    let canvas = CanvasLayer::new(3, geometry, source);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::with_ops(canvas, &graph)),
        PhysicalSize::new(800, 400),
    );
    let _ = harness.redraw();

    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 2));
        NodeEditor::exec(&mut editor, "node.move", &Props::new().with_float("dy", 40.0));
    });
    let _ = harness.redraw();

    let expected = Point::new(300.0, 40.0);
    assert_eq!(origin(&graph, 2), expected);
    let in_canvas = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::child_pos(&mut canvas, 2))
    });
    assert_eq!(in_canvas, Some(expected));
    assert_eq!(harness.root_widget().selection().into_iter().collect::<Vec<_>>(), [2]);
    assert!(
        harness.root_widget().hud().is_empty(),
        "no statistics overlay unless asked"
    );
}

// --- MARK: STRUCTURE (§43)

/// The rule of §41.2, through the whole stack: a delete renumbers nothing, and undo
/// brings the node back under its own name, with its links.
#[test]
fn a_deleted_node_comes_back_under_its_own_name_with_its_links() {
    let graph = row(3);
    let mut runtime = crate::ops::runtime();
    let mut world = EditorWorld::new(&graph);
    assert!(world.add_link(Link::new(0, 1)));
    assert!(world.add_link(Link::new(1, 2)));
    let before = origin(&graph, 2);

    select(&mut runtime, &mut world, 1);
    assert_eq!(
        runtime.exec(&mut world, "node.delete", &Props::new()),
        OpResult::Finished
    );
    assert_eq!(graph.borrow().node_count(), 2);
    assert!(graph.borrow().links.is_empty(), "both links went with it");
    assert_eq!(
        origin(&graph, 2),
        before,
        "the node that stayed kept its name and place"
    );

    assert_eq!(runtime.history_mut().undo(&mut world), Some("node.delete"));
    assert_eq!(graph.borrow().node_count(), 3);
    assert_eq!(graph.borrow().links.len(), 2, "and the links came back with it");
    assert_eq!(origin(&graph, 1), Point::new(150.0, 0.0), "under its own name");
}

/// A name a delete freed is handed out again, so the arrays a caller keys by name grow
/// with the graph rather than with the session.
#[test]
fn a_freed_name_is_handed_out_again() {
    let graph = row(3);
    let mut runtime = crate::ops::runtime();
    let mut world = EditorWorld::new(&graph);

    select(&mut runtime, &mut world, 1);
    runtime.exec(&mut world, "node.delete", &Props::new());
    let props = Props::new().with_float("x", 900.0).with_float("y", 5.0);
    assert_eq!(runtime.exec(&mut world, "node.add", &props), OpResult::Finished);

    assert_eq!(graph.borrow().node_count(), 3);
    assert_eq!(origin(&graph, 1), Point::new(900.0, 5.0), "the freed name was reused");
    assert_eq!(
        world.selection().iter().copied().collect::<Vec<_>>(),
        [1],
        "and selected"
    );
    assert_eq!(runtime.history_mut().undo(&mut world), Some("node.add"));
    assert_eq!(graph.borrow().node_count(), 2);
}

/// Links are the graph's, not a view's: added and removed through the model, and
/// refused when they make no sense.
#[test]
fn links_are_added_and_removed_through_the_graph() {
    let graph = row(3);
    let mut runtime = crate::ops::runtime();
    let mut world = EditorWorld::new(&graph);

    let pair = Props::new().with_int("from", 0).with_int("to", 2);
    assert_eq!(runtime.exec(&mut world, "link.add", &pair), OpResult::Finished);
    assert_eq!(graph.borrow().links.len(), 1);
    assert_eq!(
        runtime.exec(&mut world, "link.add", &pair),
        OpResult::Cancelled,
        "the same link twice is not two links"
    );

    assert_eq!(runtime.exec(&mut world, "link.delete", &pair), OpResult::Finished);
    assert!(graph.borrow().links.is_empty());
    assert_eq!(runtime.history_mut().undo(&mut world), Some("link.delete"));
    assert_eq!(graph.borrow().links.len(), 1, "undo puts it back");
}

/// The half a world-level test cannot see: an edit reaches the canvas, so the node is
/// there to be drawn and picked rather than only to be in the model.
#[test]
fn an_added_node_reaches_the_canvas() {
    let graph = row(3);
    let geometry = {
        let graph = graph.clone();
        move |index: usize| {
            let rect = graph.borrow().node_rect(index);
            Some((rect.origin(), rect.size()))
        }
    };
    let source = |index: usize, _detail| NewWidget::new(Label::new(format!("node {index}"))).erased();
    let canvas = CanvasLayer::new(3, geometry, source);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::with_ops(canvas, &graph)),
        PhysicalSize::new(800, 400),
    );
    let _ = harness.redraw();
    let before = harness.root_widget().stats().total;

    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "node.add",
            &Props::new().with_float("x", 20.0).with_float("y", 120.0),
        );
    });
    let _ = harness.redraw();
    assert_eq!(harness.root_widget().stats().total, before + 1, "the canvas has it");

    let added = graph.borrow().node_count() - 1;
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "node.delete",
            &Props::new().with_int("index", added as i64),
        );
    });
    let _ = harness.redraw();
    assert_eq!(harness.root_widget().stats().total, before, "and loses it again");
}

// --- MARK: SESSION (the detach task, phase 2)

/// The point of a session: a new widget over the old state is the old view.
///
/// The widget is thrown away and built again — which is what a window boundary forces,
/// because a tree cannot move between `RenderRoot`s — and the view, the selection and the
/// history come back because they were never in the widget.
#[test]
fn a_new_widget_over_the_same_session_keeps_the_view_the_selection_and_the_history() {
    use masonry::kurbo::{Affine, Vec2};

    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    let mut first = editor_harness(&graph, &session);

    first.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 1));
        NodeEditor::exec(&mut editor, "node.move", &Props::new().with_float("dx", 25.0));
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::pan(&mut canvas, Vec2::new(-40.0, -10.0));
        });
    });
    let _ = first.redraw();

    let view = first.root_widget().stats().zoom;
    let selection = first.root_widget().selection();
    let depth = first.root_widget().history_depth();
    assert!(!selection.is_empty() && depth > 0, "there is something to lose");
    assert_ne!(session.borrow().view, Affine::IDENTITY, "the session followed the view");

    // The window closes: this widget is dropped and another is built over the same
    // session, which is all detach can be.
    drop(first);
    let second = editor_harness(&graph, &session);

    assert_eq!(second.root_widget().selection(), selection, "the selection came along");
    assert_eq!(second.root_widget().history_depth(), depth, "and the history");
    assert_eq!(
        second.root_widget().stats().zoom,
        view,
        "and the new canvas opens where the old one was looking"
    );
}

/// The operators know where the canvas puts its ports: a link taken off an input is drawn
/// from an output the pointer is not on, and only the canvas's layout says where it is.
#[test]
fn the_editor_hands_the_canvas_port_layout_to_its_operators() {
    let graph = row(2);
    let session = crate::EditorSession::new(&graph).share();
    let ports = blazy_canvas::PortLayout {
        first: Some(12.0),
        step: 18.0,
        ..Default::default()
    };
    let geometry = |_index: usize| Some((Point::ORIGIN, SIZE));
    let source = |index: usize, _detail| NewWidget::new(Label::new(format!("node {index}"))).erased();
    let canvas = CanvasLayer::new(2, geometry, source).with_ports(ports);
    let _editor = NodeEditor::with_session(canvas, session.clone());
    assert_eq!(session.borrow().world.ports, ports);
}

/// A harness over a fresh canvas and an existing session.
fn editor_harness(graph: &SharedGraph<Row>, session: &crate::SessionHandle<Row>) -> TestHarness<NodeEditor<Row>> {
    editor_harness_sized(graph, session, PhysicalSize::new(800, 400))
}

fn editor_harness_sized(
    graph: &SharedGraph<Row>,
    session: &crate::SessionHandle<Row>,
    size: PhysicalSize<u32>,
) -> TestHarness<NodeEditor<Row>> {
    let geometry = {
        let graph = graph.clone();
        move |index: usize| {
            let rect = graph.borrow().node_rect(index);
            Some((rect.origin(), rect.size()))
        }
    };
    let source = |index: usize, _detail| NewWidget::new(Label::new(format!("node {index}"))).erased();
    let canvas = CanvasLayer::new(3, geometry, source);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::with_session(canvas, session.clone())),
        size,
    );
    let _ = harness.redraw();
    harness
}

/// A view is forgotten when its token goes, and with it everything it was owed.
///
/// The token's drop is the only notice a graph gets that a canvas left the tree: Masonry
/// has no removal event. Before it existed a view joined away stayed owed every later
/// change, and the windows were woken after every event for good.
#[test]
fn a_view_is_forgotten_with_its_token() {
    let views = Views::new();
    let canvas = NewWidget::new(Label::new("")).to_pod().id();
    let other = NewWidget::new(Label::new("")).to_pod().id();
    let token = views.attach(canvas);
    let _kept = views.attach(other);

    views.note_except(other, crate::Change::Contents { index: 0 });
    assert_eq!(views.owed(), 1, "owed to the view that did not make it");
    assert!(views.has_pending());

    drop(token);
    assert_eq!(views.len(), 1);
    assert_eq!(views.owed(), 0, "and what it was owed goes with it");
    assert!(!views.has_pending());
    assert_eq!(views.counters().detached, 1);
}

/// A view attached after a node was raised is owed the raise, and a name handed out
/// again starts at the bottom (§50.8).
///
/// A canvas is built from the model's geometry and nothing else, so a view made later — a
/// split, a detached window — would otherwise stack the graph by name and disagree with
/// every view that saw the raise.
#[test]
fn a_late_view_is_owed_the_stacking_order() {
    let views = Views::new();
    assert_eq!(views.raise(3), Some(1));
    assert_eq!(views.raise(3), None, "the node in front is not raised again");
    assert_eq!(views.raise(1), Some(2));

    let late = NewWidget::new(Label::new("")).to_pod().id();
    let _token = views.attach(late);
    assert_eq!(views.take(late), [
        crate::Change::Raised { index: 1, stamp: 2 },
        crate::Change::Raised { index: 3, stamp: 1 },
    ]);

    views.note(crate::Change::Structure(crate::Edit::NodeRemoved { index: 3 }));
    assert_eq!(views.stacking(3), 0, "a freed name forgets its place");
    assert_eq!(views.stacking(1), 2);
}

/// Collecting a link twice files it once.
///
/// Twice is the normal case: inside a window the push delivers an edit to the other views
/// in the same frame, and the pull delivers it again. A node re-inserted under its own
/// name replaces itself, but a canvas names its links itself, so a second insertion would
/// file the same link under a second name.
#[test]
fn a_link_collected_twice_is_filed_once() {
    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    let mut harness = editor_harness(&graph, &session);
    let canvas = harness.root_widget().canvas_id();
    let views = graph.borrow().views().clone();
    let _token = views.attach(canvas);
    let link = Link::new(0, 2);
    views.note(crate::Change::Structure(crate::Edit::LinkAdded(link)));
    views.note(crate::Change::Structure(crate::Edit::LinkAdded(link)));

    let applied = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| crate::sync_canvas(&mut canvas, &views))
    });
    assert_eq!(applied, 2);
    let names = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            let first = CanvasLayer::link_name(&mut canvas, link);
            if let Some(name) = first {
                CanvasLayer::remove_link(&mut canvas, name);
            }
            (first, CanvasLayer::link_name(&mut canvas, link))
        })
    });
    assert!(names.0.is_some(), "the link is there");
    assert_eq!(names.1, None, "once: removing it once leaves none");
}

/// The wheel zooms through the keymap exactly as the canvas zoomed on its own.
///
/// `view.zoom` took the wheel over from the canvas; rebinding nothing must change nothing
/// about how it feels — the same notch, the same point, the same view.
#[test]
fn the_wheel_zooms_through_the_keymap_as_the_canvas_did() {
    let graph = row(3);
    let at = Point::new(220.0, 140.0);
    let notch = masonry::kurbo::Vec2::new(0.0, -120.0);

    // The canvas alone, with its own wheel.
    let geometry = {
        let graph = graph.clone();
        move |index: usize| {
            let rect = graph.borrow().node_rect(index);
            Some((rect.origin(), rect.size()))
        }
    };
    let source = |index: usize, _detail| NewWidget::new(Label::new(format!("node {index}"))).erased();
    let mut bare = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(CanvasLayer::new(3, geometry, source)),
        PhysicalSize::new(800, 400),
    );
    let _ = bare.redraw();
    bare.mouse_move(at);
    bare.mouse_wheel(notch);
    let by_canvas = bare.root_widget().view();

    // The editor, whose canvas has given the wheel up to the keymap.
    let session = crate::EditorSession::new(&graph).share();
    let mut editor = editor_harness(&graph, &session);
    editor.mouse_move(at);
    editor.mouse_wheel(notch);
    let by_operator =
        editor.edit_root_widget(|mut editor| NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.view()));

    assert_ne!(by_canvas, masonry::kurbo::Affine::IDENTITY, "the notch zoomed at all");
    assert_eq!(
        by_operator, by_canvas,
        "the same notch at the same point gives the same view"
    );
    assert_eq!(
        session.borrow().runtime.history().depth(),
        0,
        "a view change is not an undo step"
    );
}

/// A key and a script zoom by the factor they are given, about the point they name.
#[test]
fn a_key_and_a_script_zoom_by_their_factor() {
    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    let mut harness = editor_harness(&graph, &session);
    let zoom = |harness: &mut TestHarness<NodeEditor<Row>>| {
        harness.edit_root_widget(|mut editor| NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.zoom()))
    };
    let before = zoom(&mut harness);
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "view.zoom",
            &Props::new()
                .with_float("factor", 2.0)
                .with_float("x", 0.0)
                .with_float("y", 0.0),
        );
    });
    let _ = harness.redraw();
    assert!((zoom(&mut harness) / before - 2.0).abs() < 1e-9);

    // Keys reach the focus fallback and nobody else (§38.3); a host names the editor.
    let editor = harness.root_widget().ctx().widget_id();
    harness.set_focus_fallback(Some(editor));
    harness.process_text_event(masonry::core::TextEvent::Keyboard(
        masonry::core::keyboard::KeyboardEvent {
            state: masonry::core::keyboard::KeyState::Down,
            key: masonry::core::keyboard::Key::Character("-".into()),
            code: masonry::core::keyboard::Code::Unidentified,
            modifiers: masonry::core::keyboard::Modifiers::empty(),
            ..Default::default()
        },
    ));
    let _ = harness.redraw();
    let expected = 2.0 / crate::ops::ZoomOp::STEP;
    assert!(
        (zoom(&mut harness) / before - expected).abs() < 1e-9,
        "`-` zooms out by one step — if the editor hears the key at all"
    );
}

/// The editor's default keymap survives its own file.
#[test]
fn the_default_keymap_reads_back_from_its_file() {
    let keymap = crate::ops::default_keymap();
    let text = keymap.write();
    assert_eq!(blazy_ops::keymap::Keymap::parse(&text), Ok(keymap), "{text}");
}

/// The colour a selected node's border takes, by the style below.
const PICKED: masonry::peniko::Color = masonry::peniko::Color::from_rgb8(0xff, 0xa5, 0x2c);

/// Properties where a label that wears [`SELECTED`](crate::SELECTED) has a coloured border.
///
/// The whole of what an application writes for its nodes to look selected: one layer of
/// its node type's property stack. Labels stand in for nodes here; an application styles
/// a type of its own, because this replaces the theme's stack for the type.
fn styled_properties() -> masonry::core::DefaultProperties {
    use masonry::core::{PropertyStack, Selector};
    use masonry::properties::BorderColor;
    let mut properties = default_property_set();
    let mut stack = PropertyStack::new();
    stack.push_layer(Selector::classes(&[crate::SELECTED]), BorderColor { color: PICKED });
    properties.insert_stack::<Label>(stack);
    properties
}

/// A harness over a fresh canvas and an existing session, with the selection styled.
fn styled_harness(graph: &SharedGraph<Row>, session: &crate::SessionHandle<Row>) -> TestHarness<NodeEditor<Row>> {
    let geometry = {
        let graph = graph.clone();
        move |index: usize| {
            let rect = graph.borrow().node_rect(index);
            Some((rect.origin(), rect.size()))
        }
    };
    let source = |index: usize, _detail| NewWidget::new(Label::new(format!("node {index}"))).erased();
    let canvas = CanvasLayer::new(3, geometry, source);
    let mut harness = TestHarness::create_with_size(
        styled_properties(),
        NewWidget::new(NodeEditor::with_session(canvas, session.clone())),
        PhysicalSize::new(800, 400),
    );
    let _ = harness.redraw();
    harness
}

/// The border colour each node's widget resolves to, by name.
fn borders(harness: &mut TestHarness<NodeEditor<Row>>) -> Vec<(usize, masonry::peniko::Color)> {
    let live = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::live_children(&mut canvas))
    });
    live.into_iter()
        .map(|(index, id)| {
            let color = harness
                .get_widget_with_id(id)
                .get_prop::<masonry::properties::BorderColor>()
                .color;
            (index, color)
        })
        .collect()
}

/// A selected node looks selected by its own style, and only the selected one does.
///
/// §38.7's customer: the node never hears of a selection — it wears a class, and its
/// type's property stack says what that class looks like.
#[test]
fn a_selected_node_looks_selected_by_its_own_style() {
    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    let mut harness = styled_harness(&graph, &session);
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 1));
    });
    let _ = harness.redraw();

    let borders = borders(&mut harness);
    assert_eq!(borders.len(), 3, "all three nodes are on screen");
    for (index, color) in borders {
        assert_eq!(color == PICKED, index == 1, "node {index}");
    }
}

/// A click changes the class of the nodes it changed, and no others.
#[test]
fn a_click_changes_the_class_of_two_nodes() {
    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    let mut harness = styled_harness(&graph, &session);
    let changes = |harness: &mut TestHarness<NodeEditor<Row>>| {
        harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.stats().counters.class_changes)
        })
    };
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 0));
    });
    let _ = harness.redraw();
    let before = changes(&mut harness);
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 2));
    });
    let _ = harness.redraw();
    assert_eq!(
        changes(&mut harness) - before,
        2,
        "one node lost the class, one gained it"
    );
}

/// The canvas's content widget, the parent of the node widgets, found by type.
fn canvas_content(widget: masonry::core::WidgetRef<'_, dyn masonry::core::Widget>) -> Option<masonry::core::WidgetId> {
    if widget.short_type_name() == "CanvasContent" {
        return Some(widget.id());
    }
    widget.children().into_iter().find_map(canvas_content)
}

/// Selecting a node brings it to the front: it is drawn over the node it overlaps, and a
/// press on the overlap — through the widget tree and through the canvas's own pick —
/// reaches it rather than the node underneath.
#[test]
fn a_selected_node_comes_to_the_front() {
    let graph = row(3);
    // Node 1 slides half over node 0; by name it stacks above it.
    graph.borrow_mut().set_node_pos(1, Point::new(50.0, 0.0));
    let session = crate::EditorSession::new(&graph).share();
    let mut harness = styled_harness(&graph, &session);
    let overlap = Point::new(75.0, 30.0);

    let on_top = |harness: &mut TestHarness<NodeEditor<Row>>| {
        let live = harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::live_children(&mut canvas))
        });
        // Masonry paints children in `children_ids` order and searches them in reverse, so
        // the later of the two in the canvas content's children is the one drawn on top.
        let content = canvas_content(harness.root_widget().as_dyn()).expect("the editor holds a canvas");
        let order: Vec<usize> = harness
            .get_widget_with_id(content)
            .children()
            .iter()
            .filter_map(|child| live.iter().find(|(_, id)| *id == child.id()).map(|(index, _)| *index))
            .filter(|index| *index < 2)
            .collect();
        let drawn = order.last().copied();
        let picked = harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::hit_test(&mut canvas, overlap))
        });
        let picked = match picked {
            Some(blazy_canvas::CanvasHit::Node { index, .. }) => Some(index),
            _ => None,
        };
        (drawn, picked)
    };
    assert_eq!(
        on_top(&mut harness),
        (Some(1), Some(1)),
        "by name, the later node is on top"
    );

    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 0));
    });
    let _ = harness.redraw();
    assert_eq!(
        on_top(&mut harness),
        (Some(0), Some(0)),
        "the selected node came to the front"
    );

    // And it stays there when the selection moves on: the order is not the selection.
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 2));
    });
    let _ = harness.redraw();
    assert_eq!(on_top(&mut harness), (Some(0), Some(0)));
}

/// A node on top hides the ports of the node under it from the pointer.
///
/// The port pick went through the node on top to a port nobody could see, so a press
/// there pulled a link out of the node underneath. (What the eye sees is the node's: it
/// draws its own dots, and `node-canvas` checks the pixels.)
#[test]
fn a_node_on_top_hides_the_ports_under_it() {
    let graph = row(3);
    // Node 1 slides half over node 0: node 0's output port, at (100, 30), is under it.
    graph.borrow_mut().set_node_pos(1, Point::new(50.0, 0.0));
    let session = crate::EditorSession::new(&graph).share();
    let geometry = {
        let graph = graph.clone();
        move |index: usize| {
            let rect = graph.borrow().node_rect(index);
            Some((rect.origin(), rect.size()))
        }
    };
    let source = |index: usize, _detail| NewWidget::new(Label::new(format!("node {index}"))).erased();
    let ports = blazy_canvas::PortLayout {
        dot_radius: 4.0,
        ..Default::default()
    };
    let canvas = CanvasLayer::new(3, geometry, source).with_ports(ports);
    let mut harness = TestHarness::create_with_size(
        styled_properties(),
        NewWidget::new(NodeEditor::with_session(canvas, session.clone())),
        PhysicalSize::new(800, 400),
    );
    let _ = harness.redraw();
    let _ = harness.redraw();

    let pick = |harness: &mut TestHarness<NodeEditor<Row>>, at: Point| {
        harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::hit_test(&mut canvas, at))
        })
    };
    let output_of_0 = Point::new(100.0, 30.0);
    let input_of_1 = Point::new(50.0, 30.0);
    assert!(
        matches!(
            pick(&mut harness, output_of_0),
            Some(blazy_canvas::CanvasHit::Node { index: 1, .. })
        ),
        "node 1 is on top of node 0's output port"
    );
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 0));
    });
    let _ = harness.redraw();
    assert!(matches!(
        pick(&mut harness, output_of_0),
        Some(blazy_canvas::CanvasHit::Port { index: 0, .. })
    ));
    assert!(
        matches!(
            pick(&mut harness, input_of_1),
            Some(blazy_canvas::CanvasHit::Node { index: 0, .. })
        ),
        "raised, node 0 hides node 1's input port in turn"
    );
}

/// A port under the corner a node above leaves unpainted is still picked; one under the
/// node's body is not.
///
/// What shows can be grabbed: the dot there is drawn, so the pick stops at the node's
/// *shape*, which only the application knows (`NodeSource::hit`), not at its box.
#[test]
fn a_port_under_a_rounded_corner_is_picked() {
    use blazy_canvas::{Detail, NodeSource};
    use masonry::core::Widget;
    use masonry::kurbo::{RoundedRect, Shape};

    /// Nodes that are labels in rounded rectangles.
    struct Rounded;

    impl NodeSource for Rounded {
        fn build(&mut self, index: usize, _detail: Detail) -> NewWidget<dyn Widget> {
            NewWidget::new(Label::new(format!("node {index}"))).erased()
        }

        fn hit(&mut self, _index: usize, rect: Rect, point: Point) -> bool {
            RoundedRect::from_rect(rect, 12.0).contains(point)
        }
    }

    let graph = row(3);
    // Node 1's top-left corner just inside node 0's output port, at (100, 30).
    graph.borrow_mut().set_node_pos(1, Point::new(96.0, 26.0));
    let geometry = {
        let graph = graph.clone();
        move |index: usize| {
            let rect = graph.borrow().node_rect(index);
            Some((rect.origin(), rect.size()))
        }
    };
    let ports = blazy_canvas::PortLayout {
        dot_radius: 4.0,
        ..Default::default()
    };
    let canvas = CanvasLayer::new(3, geometry, Rounded).with_ports(ports);
    let session = crate::EditorSession::new(&graph).share();
    let mut harness = TestHarness::create_with_size(
        styled_properties(),
        NewWidget::new(NodeEditor::with_session(canvas, session)),
        PhysicalSize::new(800, 400),
    );
    let _ = harness.redraw();
    let mut pick = |at: Point| {
        harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::hit_test(&mut canvas, at))
        })
    };
    assert!(
        matches!(
            pick(Point::new(97.0, 27.0)),
            Some(blazy_canvas::CanvasHit::Port { index: 0, .. })
        ),
        "outside node 1's rounding, node 0's port shows and is picked"
    );
    assert!(
        matches!(
            pick(Point::new(103.0, 33.0)),
            Some(blazy_canvas::CanvasHit::Node { index: 1, .. })
        ),
        "inside it, the press is node 1's"
    );
}

/// A node built over a selection that already exists wears it from the start.
///
/// The case the class has to be remembered for: a widget rebuilt in another window — the
/// nodes are new, the session is not.
#[test]
fn a_node_built_after_the_selection_wears_it() {
    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    {
        let mut first = styled_harness(&graph, &session);
        first.edit_root_widget(|mut editor| {
            NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 2));
        });
        let _ = first.redraw();
    }
    let mut second = styled_harness(&graph, &session);
    let _ = second.redraw();
    let picked: Vec<usize> = borders(&mut second)
        .into_iter()
        .filter(|(_, color)| *color == PICKED)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(picked, [2]);
}

/// A selected node that scrolls out of view and back comes back wearing the class.
///
/// Virtualisation drops the widget the moment the node leaves (§20.2); the class lives on
/// in the canvas's slot, and the next widget is built wearing it. Nothing tells the node
/// again — that is the point.
#[test]
fn a_selected_node_scrolled_back_into_view_still_looks_selected() {
    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    let mut harness = styled_harness(&graph, &session);
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 0));
    });
    let _ = harness.redraw();
    let pan = |harness: &mut TestHarness<NodeEditor<Row>>, dx: f64| {
        harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| {
                CanvasLayer::pan(&mut canvas, masonry::kurbo::Vec2::new(dx, 0.0));
            });
        });
        let _ = harness.redraw();
    };
    pan(&mut harness, -5000.0);
    assert!(
        borders(&mut harness).is_empty(),
        "every node left the view, and with it its widget"
    );
    pan(&mut harness, 5000.0);
    let picked: Vec<usize> = borders(&mut harness)
        .into_iter()
        .filter(|(_, color)| *color == PICKED)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(picked, [0]);
}

mod menus {
    use masonry::core::keyboard::{Code, Key, KeyState, KeyboardEvent, Modifiers, NamedKey};
    use masonry::core::{TextEvent, WidgetId};
    use masonry::kurbo::Point;
    use masonry::ui_events::pointer::PointerButton;

    use super::*;
    use crate::MenuLayer;

    fn key(harness: &mut TestHarness<NodeEditor<Row>>, key: Key) {
        harness.process_text_event(TextEvent::Keyboard(KeyboardEvent {
            state: KeyState::Down,
            key,
            code: Code::Unidentified,
            modifiers: Modifiers::empty(),
            ..KeyboardEvent::default()
        }));
        let _ = harness.redraw();
    }

    /// An editor with the keys, the pointer over empty canvas, and its menu open.
    fn opened() -> (TestHarness<NodeEditor<Row>>, SharedGraph<Row>, WidgetId) {
        let graph = row(3);
        let session = crate::EditorSession::new(&graph).share();
        let mut harness = editor_harness(&graph, &session);
        let editor = harness.root_widget().ctx().widget_id();
        harness.set_focus_fallback(Some(editor));
        harness.mouse_move(Point::new(500.0, 300.0));
        key(&mut harness, Key::Character("w".into()));
        let menu = harness.root_widget().open_menu().expect("W opens the menu");
        assert!(harness.try_get_widget(menu).is_some(), "and it is in the window");
        (harness, graph, menu)
    }

    /// The button of the entry that runs `op`.
    fn entry(harness: &TestHarness<NodeEditor<Row>>, menu: WidgetId, op: &str) -> WidgetId {
        let layer = harness.get_widget_with_id(menu);
        let layer = layer.downcast::<MenuLayer<Row>>().expect("the layer is a menu");
        layer
            .entries()
            .find(|(_, name)| *name == op)
            .map(|(id, _)| id)
            .expect("the entry is there")
    }

    /// Choosing an entry runs its operator the way a key does, and closes the menu.
    #[test]
    fn an_entry_runs_its_operator_and_closes_the_menu() {
        let (mut harness, graph, menu) = opened();
        let add = entry(&harness, menu, "node.add");
        // Unchecked: the harness looks for a widget under the pointer in the base layer
        // only, and a menu is a layer of its own.
        harness.mouse_move_to_unchecked(add);
        harness.mouse_button_press(Some(PointerButton::Primary));
        harness.mouse_button_release(Some(PointerButton::Primary));
        let _ = harness.redraw();
        assert_eq!(graph.borrow().node_count(), 4, "a node was added");
        assert!(harness.try_get_widget(menu).is_none(), "and the menu is gone");
        assert_eq!(harness.root_widget().open_menu(), None);
    }

    /// An entry whose operator would be refused is shown refused.
    #[test]
    fn an_entry_that_would_be_refused_is_disabled() {
        let (harness, _graph, menu) = opened();
        // Nothing is selected, so there is nothing to link.
        let link = entry(&harness, menu, "link.add");
        assert!(harness.get_widget_with_id(link).ctx().is_disabled());
        let add = entry(&harness, menu, "node.add");
        assert!(!harness.get_widget_with_id(add).ctx().is_disabled());
    }

    /// A click outside closes the menu and does nothing else.
    #[test]
    fn a_click_outside_closes_the_menu_and_nothing_else() {
        let (mut harness, graph, menu) = opened();
        let clicks = harness.root_widget().op_counters().clicks;
        harness.mouse_move(Point::new(5.0, 5.0));
        harness.mouse_button_press(Some(PointerButton::Primary));
        harness.mouse_button_release(Some(PointerButton::Primary));
        let _ = harness.redraw();
        assert!(harness.try_get_widget(menu).is_none(), "the menu is gone");
        assert_eq!(harness.root_widget().open_menu(), None, "and the editor knows");
        assert_eq!(graph.borrow().node_count(), 3);
        assert_eq!(
            harness.root_widget().op_counters().clicks,
            clicks,
            "the click never reached the keymap"
        );
    }

    /// Escape closes it, and the keys are the menu's while it is open.
    #[test]
    fn escape_closes_the_menu_and_keys_do_not_leak_past_it() {
        let (mut harness, graph, menu) = opened();
        // `Shift+A` would add a node; with the menu open it does not.
        harness.process_text_event(TextEvent::Keyboard(KeyboardEvent {
            state: KeyState::Down,
            key: Key::Character("a".into()),
            code: Code::Unidentified,
            modifiers: Modifiers::SHIFT,
            ..KeyboardEvent::default()
        }));
        let _ = harness.redraw();
        assert_eq!(graph.borrow().node_count(), 3, "the keymap did not hear it");
        key(&mut harness, Key::Named(NamedKey::Escape));
        assert!(harness.try_get_widget(menu).is_none());
        assert_eq!(harness.root_widget().open_menu(), None);
    }
}

/// A menu opened near the bottom-right corner is moved inside the window.
///
/// In a window the menu fits: moving it in is all the editor does, and a menu taller than
/// its window is cut off — the editor's own menu grew past 400 px with the copy entries
/// (§57.6).
#[test]
fn a_menu_opened_by_the_edge_stays_inside_the_window() {
    use masonry::core::TextEvent;
    use masonry::core::keyboard::{Code, Key, KeyState, KeyboardEvent, Modifiers};
    use masonry::kurbo::Point;
    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    let mut harness = editor_harness_sized(&graph, &session, PhysicalSize::new(800, 600));
    let editor = harness.root_widget().ctx().widget_id();
    harness.set_focus_fallback(Some(editor));
    harness.mouse_move(Point::new(790.0, 590.0));
    harness.process_text_event(TextEvent::Keyboard(KeyboardEvent {
        state: KeyState::Down,
        key: Key::Character("w".into()),
        code: Code::Unidentified,
        modifiers: Modifiers::empty(),
        ..KeyboardEvent::default()
    }));
    let _ = harness.redraw();
    let menu = harness.root_widget().open_menu().expect("the menu opened");
    let widget = harness.get_widget_with_id(menu);
    let origin = widget.ctx().to_window(Point::ORIGIN);
    let size = widget.ctx().border_box().size();
    assert!(origin.x >= 0.0 && origin.y >= 0.0, "{origin:?}");
    assert!(origin.x + size.width <= 800.0 + 1e-9, "{origin:?} {size:?}");
    assert!(origin.y + size.height <= 600.0 + 1e-9, "{origin:?} {size:?}");
}

mod link_drag {
    use masonry::core::TextEvent;
    use masonry::core::keyboard::{Code, Key, KeyState, KeyboardEvent, Modifiers, NamedKey};
    use masonry::kurbo::Point;
    use masonry::ui_events::pointer::PointerButton;

    use super::*;

    /// Drags from `from` to `to` with the primary button, in steps.
    fn drag(harness: &mut TestHarness<NodeEditor<Row>>, from: Point, to: Point) {
        harness.mouse_move(from);
        harness.mouse_button_press(Some(PointerButton::Primary));
        for step in 1..=8 {
            harness.mouse_move(from + (to - from) * (f64::from(step) / 8.0));
        }
        let _ = harness.redraw();
    }

    fn release(harness: &mut TestHarness<NodeEditor<Row>>) {
        harness.mouse_button_release(Some(PointerButton::Primary));
        let _ = harness.redraw();
    }

    fn links(graph: &SharedGraph<Row>) -> Vec<Link> {
        graph.borrow().links.clone()
    }

    /// Out of node 0's output, into node 1's input: a link, which undo takes back.
    #[test]
    fn a_link_dragged_from_an_output_to_an_input_is_added_and_undone() {
        let graph = row(3);
        let session = crate::EditorSession::new(&graph).share();
        let mut harness = editor_harness(&graph, &session);
        drag(&mut harness, Point::new(100.0, 30.0), Point::new(150.0, 30.0));
        assert!(
            session.borrow().world.link_preview.is_some(),
            "the curve follows the pointer"
        );
        release(&mut harness);
        assert_eq!(links(&graph), [Link::between(0, 0, 1, 0)]);
        assert!(
            session.borrow().world.link_preview.is_none(),
            "and goes when the drag ends"
        );
        assert!(!session.borrow().world.track_hover, "and so does the pick per move");
        let in_canvas = harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| {
                CanvasLayer::link_name(&mut canvas, Link::between(0, 0, 1, 0)).is_some()
            })
        });
        assert!(in_canvas, "the canvas shows it");

        harness.edit_root_widget(|mut editor| {
            NodeEditor::exec(&mut editor, "ed.undo", &Props::new());
        });
        assert!(links(&graph).is_empty(), "undo takes it back");
    }

    /// Dragged backwards, from an input to an output, it is the same link.
    #[test]
    fn a_link_dragged_from_an_input_runs_out_of_the_output_anyway() {
        let graph = row(3);
        let session = crate::EditorSession::new(&graph).share();
        let mut harness = editor_harness(&graph, &session);
        drag(&mut harness, Point::new(150.0, 30.0), Point::new(100.0, 30.0));
        release(&mut harness);
        assert_eq!(links(&graph), [Link::between(0, 0, 1, 0)]);
    }

    /// Dropped on nothing, it is nothing.
    #[test]
    fn a_link_dropped_on_empty_canvas_adds_nothing() {
        let graph = row(3);
        let session = crate::EditorSession::new(&graph).share();
        let mut harness = editor_harness(&graph, &session);
        drag(&mut harness, Point::new(100.0, 30.0), Point::new(125.0, 200.0));
        release(&mut harness);
        assert!(links(&graph).is_empty());
        assert_eq!(session.borrow().runtime.history().depth(), 0, "and leaves no step");
    }

    /// Escape in the middle of a drag cancels it.
    #[test]
    fn escape_cancels_a_link_drag() {
        let graph = row(3);
        let session = crate::EditorSession::new(&graph).share();
        let mut harness = editor_harness(&graph, &session);
        let editor = harness.root_widget().ctx().widget_id();
        harness.set_focus_fallback(Some(editor));
        drag(&mut harness, Point::new(100.0, 30.0), Point::new(150.0, 30.0));
        harness.process_text_event(TextEvent::Keyboard(KeyboardEvent {
            state: KeyState::Down,
            key: Key::Named(NamedKey::Escape),
            code: Code::Unidentified,
            modifiers: Modifiers::empty(),
            ..KeyboardEvent::default()
        }));
        assert!(session.borrow().world.link_preview.is_none());
        release(&mut harness);
        assert!(links(&graph).is_empty(), "the release after a cancel adds nothing");
    }

    /// A drag that starts on an input with a link takes the link off and carries its input
    /// end, from the output it still leaves; let go on another input, the link is there —
    /// in the canvas too — and one undo puts it back where it was.
    #[test]
    fn a_link_dragged_off_an_input_comes_off_and_goes_where_it_is_let_go() {
        let graph = row(3);
        let session = crate::EditorSession::new(&graph).share();
        let mut harness = editor_harness(&graph, &session);
        harness.edit_root_widget(|mut editor| {
            NodeEditor::exec(
                &mut editor,
                "link.add",
                &Props::new().with_int("from", 0).with_int("to", 1),
            );
        });
        let depth = session.borrow().runtime.history().depth();

        // Node 1's input is at (150, 30), node 2's at (300, 30), node 0's output at (100, 30).
        drag(&mut harness, Point::new(150.0, 30.0), Point::new(300.0, 30.0));
        assert!(links(&graph).is_empty(), "the link came off as the drag began");
        let preview = session.borrow().world.link_preview.clone().expect("a curve");
        assert_eq!(
            preview.curves,
            [(Point::new(100.0, 30.0), Point::new(300.0, 30.0))],
            "from the output it leaves, to the input under the pointer"
        );
        release(&mut harness);
        assert_eq!(links(&graph), [Link::new(0, 2)]);
        let shown = |harness: &mut TestHarness<NodeEditor<Row>>, link| {
            harness.edit_root_widget(|mut editor| {
                NodeEditor::with_canvas(&mut editor, |mut canvas| {
                    CanvasLayer::link_name(&mut canvas, link).is_some()
                })
            })
        };
        assert!(shown(&mut harness, Link::new(0, 2)) && !shown(&mut harness, Link::new(0, 1)));
        assert_eq!(session.borrow().runtime.history().depth(), depth + 1, "one step");

        harness.edit_root_widget(|mut editor| {
            NodeEditor::exec(&mut editor, "ed.undo", &Props::new());
        });
        assert_eq!(links(&graph), [Link::new(0, 1)], "undo puts it back where it was");
    }
}

/// Every port a press and drag would start a link drag from is lit while the pointer is on
/// it — before the press, whether the drag would draw a new link or take one off — and put
/// out when the pointer leaves. A modifier the keymap asks for counts, pressed with the
/// pointer standing still. While a link is dragged, the port it is drawn out of and the
/// port it would be connected to are lit instead, the second in the colour of a refusal
/// where the graph refuses it. Lit in pixels, not only in a field: the dot in another
/// colour, at its own size.
#[test]
fn a_port_a_press_and_drag_would_start_from_is_lit() {
    use masonry::core::TextEvent;
    use masonry::core::keyboard::{Code, Key, KeyState, KeyboardEvent, Modifiers, NamedKey};
    use masonry::peniko::Color;
    use masonry::ui_events::pointer::PointerButton;

    use crate::OverlayStyle;

    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    let geometry = {
        let graph = graph.clone();
        move |index: usize| {
            let rect = graph.borrow().node_rect(index);
            Some((rect.origin(), rect.size()))
        }
    };
    let source = |index: usize, _detail| NewWidget::new(Label::new(format!("node {index}"))).erased();
    let dots = blazy_canvas::PortLayout {
        dot_radius: 4.0,
        ..Default::default()
    };
    let canvas = CanvasLayer::new(3, geometry, source).with_ports(dots);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::with_session(canvas, session.clone())),
        PhysicalSize::new(800, 400),
    );
    let _ = harness.redraw();
    let editor = harness.root_widget().ctx().widget_id();
    harness.set_focus_fallback(Some(editor));
    harness.edit_root_widget(|mut editor| {
        NodeEditor::exec(
            &mut editor,
            "link.add",
            &Props::new().with_int("from", 0).with_int("to", 1),
        );
    });
    let hint = |harness: &TestHarness<NodeEditor<Row>>| harness.root_widget().port_hint();
    let ctrl = |harness: &mut TestHarness<NodeEditor<Row>>, down: bool| {
        harness.process_text_event(TextEvent::Keyboard(KeyboardEvent {
            state: if down { KeyState::Down } else { KeyState::Up },
            key: Key::Named(NamedKey::Control),
            code: Code::ControlLeft,
            modifiers: Modifiers::empty(),
            ..KeyboardEvent::default()
        }));
    };
    let pixel = |harness: &mut TestHarness<NodeEditor<Row>>, at: Point| {
        let image = harness.render();
        let p = image.get_pixel(at.x as u32, at.y as u32).0;
        Color::from_rgba8(p[0], p[1], p[2], p[3])
    };
    let style = OverlayStyle::default();
    // Node 1's input, (150, 30), has the link; node 2's input, (300, 30), has none; node
    // 0's output, (100, 30), has it at its other end.
    let (input_1, input_2, output_0) = (
        Point::new(150.0, 30.0),
        Point::new(300.0, 30.0),
        Point::new(100.0, 30.0),
    );
    // Just outside the dot, whose radius is 4: above it, where the curve of a link dragged
    // to it, which arrives from the left, does not pass.
    let beside_input_1 = Point::new(150.0, 25.0);
    let unlit = pixel(&mut harness, beside_input_1);

    harness.mouse_move(input_1);
    assert_eq!(hint(&harness), Some((1, blazy_canvas::PortSide::Input, 0)));
    assert_eq!(pixel(&mut harness, input_1), style.port_hover, "and drawn");
    assert_eq!(pixel(&mut harness, beside_input_1), unlit, "at the dot's own size");

    harness.mouse_move(input_2);
    assert_eq!(
        hint(&harness),
        Some((2, blazy_canvas::PortSide::Input, 0)),
        "an input with no link is lit too: a drag from it draws a new one"
    );
    assert_eq!(pixel(&mut harness, input_2), style.port_hover);
    harness.mouse_move(Point::new(50.0, 200.0));
    assert_eq!(hint(&harness), None, "off every port, nothing is lit");
    harness.mouse_move(output_0);
    assert_eq!(
        hint(&harness),
        Some((0, blazy_canvas::PortSide::Output, 0)),
        "and so is an output, without Ctrl"
    );

    // A new link out of node 2's output, carried to node 1's input: the press hint is out
    // — the drag runs — and the port the link would go to is lit by the drag.
    harness.mouse_move(Point::new(400.0, 30.0));
    harness.mouse_button_press(Some(PointerButton::Primary));
    for x in [380.0, 300.0, 200.0, 150.0] {
        harness.mouse_move(Point::new(x, 30.0));
    }
    let target = session
        .borrow()
        .world
        .link_preview
        .as_ref()
        .and_then(|preview| preview.target);
    assert_eq!(
        target,
        Some((1, blazy_canvas::PortSide::Input, 0)),
        "the drag is on node 1's input"
    );
    assert_eq!(hint(&harness), None, "the press hint is out while it runs");
    let output_2 = Point::new(400.0, 30.0);
    let source = session
        .borrow()
        .world
        .link_preview
        .as_ref()
        .and_then(|preview| preview.source);
    assert_eq!(
        source,
        Some((2, blazy_canvas::PortSide::Output, 0)),
        "drawn out of node 2's output"
    );
    assert_eq!(
        pixel(&mut harness, output_2),
        style.port_hover,
        "which is lit while it is"
    );
    assert_eq!(
        pixel(&mut harness, input_1),
        style.port_hover,
        "the port it would go to is lit"
    );
    assert_eq!(pixel(&mut harness, beside_input_1), unlit, "at the dot's own size");
    // The graph stops taking links into node 1: on the next move the port says so.
    graph.borrow_mut().refuses = Some(1);
    harness.mouse_move(Point::new(151.0, 30.0));
    assert_eq!(
        pixel(&mut harness, input_1),
        style.refused,
        "in the colour of a refusal"
    );
    harness.mouse_move(Point::new(225.0, 200.0));
    let off = pixel(&mut harness, input_1);
    assert!(off != style.refused && off != style.port_hover, "off it, out: {off:?}");
    harness.mouse_button_release(Some(PointerButton::Primary));
    graph.borrow_mut().refuses = None;
    assert_ne!(
        pixel(&mut harness, output_2),
        style.port_hover,
        "and out once it is over"
    );

    // Taking node 1's link off: its fixed end is where the link already was, not a port a
    // new link is drawn out of, so nothing is lit there.
    harness.mouse_move(input_1);
    harness.mouse_button_press(Some(PointerButton::Primary));
    for y in [40.0, 80.0, 150.0] {
        harness.mouse_move(Point::new(150.0, y));
    }
    let source = session
        .borrow()
        .world
        .link_preview
        .as_ref()
        .and_then(|preview| preview.source);
    assert_eq!(source, None, "a link taken off is not drawn out of anything");
    assert_ne!(pixel(&mut harness, output_0), style.port_hover);
    let pointer_info = masonry::core::PointerInfo {
        pointer_id: None,
        persistent_device_id: None,
        pointer_type: masonry::core::PointerType::Mouse,
    };
    harness.process_pointer_event(masonry::core::PointerEvent::Cancel(pointer_info));
    assert_eq!(graph.borrow().links, [Link::new(0, 1)], "cancelled, it is back");

    // In a keymap whose only `link.drag` wants `Ctrl`, a port is lit only while it is held —
    // pressed and let go with the pointer standing still.
    session
        .borrow_mut()
        .runtime
        .set_keymap(
            blazy_ops::keymap::Keymap::new().with("canvas", vec![blazy_ops::keymap::Binding::new(
                blazy_ops::event::Pattern::drag(PointerButton::Primary).with_mods(Modifiers::CONTROL),
                "link.drag",
            )]),
        );
    harness.mouse_move(Point::new(50.0, 200.0));
    harness.mouse_move(output_0);
    assert_eq!(hint(&harness), None, "no drag starts here without Ctrl");
    ctrl(&mut harness, true);
    assert_eq!(
        hint(&harness),
        Some((0, blazy_canvas::PortSide::Output, 0)),
        "with it, one does"
    );
    ctrl(&mut harness, false);
    assert_eq!(hint(&harness), None, "and Ctrl coming up puts it out");
    session.borrow_mut().runtime.set_keymap(crate::ops::default_keymap());

    harness.mouse_move(input_1);
    assert!(hint(&harness).is_some());
    let pointer = masonry::core::PointerInfo {
        pointer_id: None,
        persistent_device_id: None,
        pointer_type: masonry::core::PointerType::Mouse,
    };
    harness.process_pointer_event(masonry::core::PointerEvent::Leave(pointer));
    assert_eq!(hint(&harness), None, "nothing is lit once the pointer has left");
    let _ = harness.redraw();
    assert_ne!(pixel(&mut harness, input_1), style.port_hover);
}

// --- MARK: RELINK (`issues/relink and port types.md`)

mod relink {
    use blazy_canvas::{CanvasHit, PortSide};
    use blazy_ops::event::OpEvent;
    use blazy_ops::keymap::Scope;
    use blazy_ops::runtime::{OpRuntime, Seat};
    use masonry::core::keyboard::{Key, Modifiers, NamedKey};
    use masonry::kurbo::Point;
    use masonry::ui_events::pointer::PointerButton;

    use super::copies::{linked, state};
    use super::{Row, SharedGraph};
    use crate::ops::CANVAS_SCOPE;
    use crate::{EditorWorld, Link};

    type Runtime = OpRuntime<EditorWorld<Row>>;

    fn send(runtime: &mut Runtime, world: &mut EditorWorld<Row>, event: &OpEvent) {
        let _ = runtime.dispatch(world, event, Scope(&CANVAS_SCOPE), Seat::Tree);
    }

    /// Puts the pointer on a port, as the canvas's pick would report it.
    fn over(world: &mut EditorWorld<Row>, index: usize, side: PortSide) {
        let pos = world.ports.position(world.node_rect(index), side, 0);
        world.pointer = pos;
        world.hover = Some(CanvasHit::Port {
            index,
            side,
            port: 0,
            pos,
        });
    }

    /// Puts the pointer on nothing at `pos`.
    fn nowhere(world: &mut EditorWorld<Row>, pos: Point) {
        world.pointer = pos;
        world.hover = None;
    }

    fn moved(runtime: &mut Runtime, world: &mut EditorWorld<Row>) {
        let event = OpEvent::Move {
            pos: world.pointer,
            mods: Modifiers::empty(),
        };
        send(runtime, world, &event);
    }

    /// Starts a drag where the pointer is, with `mods` held.
    fn grab(runtime: &mut Runtime, world: &mut EditorWorld<Row>, mods: Modifiers) {
        let event = OpEvent::Drag {
            button: PointerButton::Primary,
            pos: world.pointer,
            screen: world.pointer,
            mods,
        };
        send(runtime, world, &event);
        assert_eq!(runtime.modal_name(), Some("link.drag"));
    }

    fn let_go(runtime: &mut Runtime, world: &mut EditorWorld<Row>) {
        let event = OpEvent::Release {
            button: PointerButton::Primary,
            pos: world.pointer,
            mods: Modifiers::empty(),
        };
        send(runtime, world, &event);
        assert_eq!(runtime.modal_depth(), 0);
    }

    fn links(graph: &SharedGraph<Row>) -> Vec<Link> {
        let mut links = graph.borrow().links.clone();
        links.sort_by_key(|link| (link.from, link.to));
        links
    }

    /// Let go on nothing, a link taken off an input is gone — in one step, which undo
    /// takes back.
    #[test]
    fn a_link_taken_off_and_let_go_on_nothing_is_deleted_in_one_step() {
        let (graph, mut runtime, mut world) = linked();
        let before = state(&graph);
        let depth = runtime.history().depth();
        over(&mut world, 1, PortSide::Input);
        grab(&mut runtime, &mut world, Modifiers::empty());
        nowhere(&mut world, Point::new(120.0, 300.0));
        moved(&mut runtime, &mut world);
        let_go(&mut runtime, &mut world);
        assert_eq!(links(&graph), [Link::new(1, 2)], "0 → 1 is gone, 1 → 2 untouched");
        assert_eq!(runtime.history().depth(), depth + 1);
        assert_eq!(runtime.history_mut().undo(&mut world), Some("link.drag"));
        assert_eq!(state(&graph), before);
    }

    /// `Escape`, and a link let go where it came from, leave the graph as it was and the
    /// history without a step.
    #[test]
    fn escape_or_letting_go_where_it_came_from_changes_nothing() {
        let (graph, mut runtime, mut world) = linked();
        let before = state(&graph);
        let depth = runtime.history().depth();

        over(&mut world, 1, PortSide::Input);
        grab(&mut runtime, &mut world, Modifiers::empty());
        nowhere(&mut world, Point::new(120.0, 300.0));
        moved(&mut runtime, &mut world);
        let escape = OpEvent::Key {
            key: Key::Named(NamedKey::Escape),
            mods: Modifiers::empty(),
            down: true,
        };
        send(&mut runtime, &mut world, &escape);
        assert_eq!(state(&graph), before, "Escape puts the link back");

        over(&mut world, 1, PortSide::Input);
        grab(&mut runtime, &mut world, Modifiers::empty());
        moved(&mut runtime, &mut world);
        let_go(&mut runtime, &mut world);
        assert_eq!(state(&graph), before, "let go where it came from, it is where it was");
        assert_eq!(runtime.history().depth(), depth, "and neither wrote a step");
    }

    /// A port the graph will not connect to shows it while the pointer is on it, and
    /// letting go there makes nothing — a link taken off stays off, as on empty canvas.
    #[test]
    fn a_refused_port_shows_before_the_release_and_takes_nothing() {
        let (graph, mut runtime, mut world) = linked();
        graph.borrow_mut().refuses = Some(3);
        let depth = runtime.history().depth();

        over(&mut world, 2, PortSide::Output);
        grab(&mut runtime, &mut world, Modifiers::empty());
        over(&mut world, 0, PortSide::Input);
        moved(&mut runtime, &mut world);
        assert!(
            !world.link_preview.as_ref().expect("a curve").refused,
            "node 0 takes it"
        );
        over(&mut world, 3, PortSide::Input);
        moved(&mut runtime, &mut world);
        let preview = world.link_preview.clone().expect("a curve");
        assert!(preview.refused, "node 3 does not, and it shows before the release");
        let at = world.ports.position(world.node_rect(3), PortSide::Input, 0);
        assert_eq!(preview.curves[0].1, at, "on the port it would go to");
        let_go(&mut runtime, &mut world);
        assert_eq!(links(&graph), [Link::new(0, 1), Link::new(1, 2)], "nothing made");
        assert_eq!(runtime.history().depth(), depth, "and no step");

        over(&mut world, 2, PortSide::Input);
        grab(&mut runtime, &mut world, Modifiers::empty());
        over(&mut world, 3, PortSide::Input);
        moved(&mut runtime, &mut world);
        let_go(&mut runtime, &mut world);
        assert_eq!(links(&graph), [Link::new(0, 1)], "taken off and refused: off");
        assert_eq!(runtime.history().depth(), depth + 1);
    }

    /// Which port is lit is the keymap's to say: a port lights if some `link.drag` binding
    /// starts from it with the modifiers held — whether that drag draws a new link or takes
    /// one off — and with `link.drag` moved to `Alt`, only `Alt` lights it.
    #[test]
    fn the_keymap_says_which_port_a_press_and_drag_starts_from() {
        use blazy_ops::event::Pattern;
        use blazy_ops::keymap::{Binding, Keymap, Props};

        use crate::ops::port_hint;

        let (_graph, mut runtime, mut world) = linked();
        for (index, side) in [(1, PortSide::Input), (1, PortSide::Output), (3, PortSide::Output)] {
            over(&mut world, index, side);
            for mods in [Modifiers::empty(), Modifiers::CONTROL] {
                assert_eq!(
                    port_hint(&runtime, &world, mods),
                    Some((index, side, 0)),
                    "node {index} {side:?} with {mods:?}: the default keymap drags from every port"
                );
            }
            assert_eq!(port_hint(&runtime, &world, Modifiers::ALT), None, "no drag on Alt");
        }

        runtime.set_keymap(Keymap::new().with("canvas", vec![
            Binding::new(Pattern::drag(PointerButton::Primary).with_mods(Modifiers::ALT), "link.drag")
                .with_props(Props::new().with_bool("detach", true)),
        ]));
        over(&mut world, 1, PortSide::Input);
        assert_eq!(port_hint(&runtime, &world, Modifiers::empty()), None);
        assert_eq!(port_hint(&runtime, &world, Modifiers::CONTROL), None);
        assert_eq!(
            port_hint(&runtime, &world, Modifiers::ALT),
            Some((1, PortSide::Input, 0))
        );
    }

    /// An input holds one link by default, as in Blender: a link dropped on an input that has
    /// one takes its place — new or carried off another input — in one step that undo takes
    /// back whole. A graph whose inputs hold more keeps both.
    #[test]
    fn a_link_dropped_on_a_full_input_takes_its_place() {
        let (graph, mut runtime, mut world) = linked();
        let before = state(&graph);
        let depth = runtime.history().depth();

        // A new link out of node 3 onto node 1's input, which 0 → 1 holds.
        over(&mut world, 3, PortSide::Output);
        grab(&mut runtime, &mut world, Modifiers::empty());
        over(&mut world, 1, PortSide::Input);
        moved(&mut runtime, &mut world);
        let_go(&mut runtime, &mut world);
        assert_eq!(
            links(&graph),
            [Link::new(1, 2), Link::new(3, 1)],
            "3 → 1 replaced 0 → 1"
        );
        assert_eq!(runtime.history().depth(), depth + 1, "in one step");
        assert_eq!(runtime.history_mut().undo(&mut world), Some("link.drag"));
        assert_eq!(state(&graph), before, "which undo takes back whole");

        // 0 → 1 taken off node 1's input and dropped on node 2's, which 1 → 2 holds.
        over(&mut world, 1, PortSide::Input);
        grab(&mut runtime, &mut world, Modifiers::empty());
        over(&mut world, 2, PortSide::Input);
        moved(&mut runtime, &mut world);
        let_go(&mut runtime, &mut world);
        assert_eq!(links(&graph), [Link::new(0, 2)], "carried, it replaced 1 → 2");
        assert_eq!(runtime.history_mut().undo(&mut world), Some("link.drag"));
        assert_eq!(state(&graph), before, "and one undo puts both back");

        // The same from node 3's output, which the graph now lets inputs gather.
        graph.borrow_mut().holds = Some(2);
        over(&mut world, 3, PortSide::Output);
        grab(&mut runtime, &mut world, Modifiers::empty());
        over(&mut world, 1, PortSide::Input);
        moved(&mut runtime, &mut world);
        let_go(&mut runtime, &mut world);
        assert_eq!(
            links(&graph),
            [Link::new(0, 1), Link::new(1, 2), Link::new(3, 1)],
            "an input that holds two keeps both"
        );
    }

    /// `link.add` connects as a drag does: onto a full input, in place of what was there.
    #[test]
    fn link_add_on_a_full_input_takes_its_place() {
        use blazy_ops::keymap::Props;

        let (graph, mut runtime, mut world) = linked();
        let before = state(&graph);
        let add = Props::new().with_int("from", 3).with_int("to", 2);
        assert_eq!(
            runtime.exec(&mut world, "link.add", &add),
            blazy_ops::OpResult::Finished
        );
        assert_eq!(
            links(&graph),
            [Link::new(0, 1), Link::new(3, 2)],
            "3 → 2 replaced 1 → 2"
        );
        assert_eq!(runtime.history_mut().undo(&mut world), Some("link.drag"));
        assert_eq!(state(&graph), before);

        // A link the graph refuses changes nothing — not even the link it would have
        // replaced, which `connect` takes off first and has to put back.
        assert_eq!(world.connect(Link::new(1, 1)), None, "a node to itself is refused");
        assert_eq!(state(&graph), before, "and 0 → 1 is still there");
    }

    /// `Ctrl` on an output takes every link off it and carries their output ends together,
    /// to another node's output; a plain drag there starts a new link instead.
    #[test]
    fn ctrl_on_an_output_carries_all_its_links() {
        let (graph, mut runtime, mut world) = linked();
        assert!(world.add_link(Link::new(1, 3)));
        let before = state(&graph);

        over(&mut world, 1, PortSide::Output);
        grab(&mut runtime, &mut world, Modifiers::CONTROL);
        assert_eq!(
            links(&graph),
            [Link::new(0, 1)],
            "both links of node 1's output came off"
        );
        assert_eq!(world.link_preview.as_ref().expect("curves").curves.len(), 2);
        over(&mut world, 0, PortSide::Output);
        moved(&mut runtime, &mut world);
        let_go(&mut runtime, &mut world);
        assert_eq!(links(&graph), [Link::new(0, 1), Link::new(0, 2), Link::new(0, 3)]);
        assert_eq!(runtime.history_mut().undo(&mut world), Some("link.drag"));
        assert_eq!(state(&graph), before, "one undo for the lot");

        over(&mut world, 1, PortSide::Output);
        grab(&mut runtime, &mut world, Modifiers::empty());
        assert_eq!(state(&graph), before, "without Ctrl nothing comes off");
        runtime.cancel_all(&mut world);
    }
}

/// A harness for edge pan: node 0 is held by its middle at `(50, 30)` and dragged into the
/// right-hand band of the 800-pixel editor, at `(790, 200)`, where the pointer then stands
/// still. (Not at `y = 30`: that is in the top band, and the pointer would never have been
/// inside.)
fn edge_pan_harness(session: crate::SessionHandle<Row>, graph: &SharedGraph<Row>) -> TestHarness<NodeEditor<Row>> {
    let mut harness = editor_harness(graph, &session);
    harness.mouse_move(Point::new(50.0, 30.0));
    harness.mouse_button_press(Some(masonry::core::PointerButton::Primary));
    // Inside the band's inner edge first: a gesture that begins in the band waits for that.
    harness.mouse_move(Point::new(200.0, 200.0));
    harness.mouse_move(Point::new(790.0, 200.0));
    let _ = harness.redraw();
    harness
}

fn view_of(harness: &mut TestHarness<NodeEditor<Row>>) -> masonry::kurbo::Affine {
    harness.edit_root_widget(|mut editor| NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.view()))
}

/// From the canvas itself: the editor's copy is refreshed by the editor's own layout, and a
/// scroll lays out the canvas alone — so the copy would not see a scroll at all.
fn child_layouts(harness: &mut TestHarness<NodeEditor<Row>>) -> u64 {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.stats().counters.child_layouts)
    })
}

/// Sixty frames of a sixtieth of a second each, with the pointer where it is.
fn hold_still(harness: &mut TestHarness<NodeEditor<Row>>) {
    for _ in 0..60 {
        harness.animate_ms(16);
        let _ = harness.redraw();
    }
}

/// A node held in the band scrolls the view while the pointer stands still, stays under
/// the pointer as it does, and the scrolling stops — frames and all — when the pointer
/// comes back inside.
///
/// The pointer sends nothing while it stands still, so all of the movement here comes
/// from animation frames: the editor's, asked for only while the pointer is in the band.
#[test]
fn a_node_held_at_the_edge_scrolls_the_view_and_stays_under_the_pointer() {
    let graph = row(3);
    let session = crate::EditorSession::new(&graph)
        .with_edge_pan(Some(crate::EdgePan::default()))
        .share();
    let mut harness = edge_pan_harness(session.clone(), &graph);
    let before = view_of(&mut harness);
    let layouts = child_layouts(&mut harness);

    hold_still(&mut harness);
    let after = view_of(&mut harness);
    let scrolled = before.translation().x - after.translation().x;
    // About a second at full speed, less the eased-in half second of the delay.
    assert!(scrolled > 200.0, "the view scrolled right: {scrolled}");
    assert_eq!(after.translation().y, before.translation().y, "and only right");
    let node = after * origin(&graph, 0);
    assert!(
        (node.x - 740.0).abs() < 1e-6 && (node.y - 170.0).abs() < 1e-6,
        "node 0 is still held by its middle, under the pointer: {node:?}"
    );
    assert_eq!(
        child_layouts(&mut harness),
        layouts,
        "a scroll is the view's, and the view goes into paint (§22)"
    );
    let counters = harness.root_widget().edge_pan_counters();
    assert!(counters.pans >= 50, "{counters:?}");

    // Back inside: the frame already asked for arrives and asks for no other.
    harness.mouse_move(Point::new(400.0, 200.0));
    harness.animate_ms(16);
    let _ = harness.redraw();
    let frames = harness.root_widget().edge_pan_counters().frames;
    let view = view_of(&mut harness);
    hold_still(&mut harness);
    assert_eq!(
        harness.root_widget().edge_pan_counters().frames,
        frames,
        "no frames asked for"
    );
    assert_eq!(view_of(&mut harness), view, "and the view stands");

    harness.mouse_button_release(Some(masonry::core::PointerButton::Primary));
    assert_eq!(harness.root_widget().modal_depth(), 0);
    assert_eq!(harness.root_widget().history_depth(), 2, "a selection and a move");
}

/// With edge pan off, the same gesture carries on past the edge and the view stands —
/// whether the application turned it off or the binding did not ask for it.
#[test]
fn without_edge_pan_the_view_stands_and_the_node_follows_the_pointer() {
    use blazy_ops::event::Pattern;
    use blazy_ops::keymap::{Binding, Keymap};

    let drag = |props: Props| {
        Keymap::new().with(crate::ops::CANVAS_CONTEXT, vec![
            Binding::new(Pattern::drag(masonry::core::PointerButton::Primary), "node.move").with_props(props),
        ])
    };
    let cases = [
        ("the session has none", None),
        (
            "the binding says false",
            Some(drag(Props::new().with_bool(crate::EDGE_PAN, false))),
        ),
        ("the binding says nothing", Some(drag(Props::new()))),
    ];
    for (case, keymap) in cases {
        let graph = row(3);
        let session = match keymap {
            None => crate::EditorSession::new(&graph),
            Some(keymap) => crate::EditorSession::with_runtime(&graph, crate::ops::runtime_with(keymap))
                .with_edge_pan(Some(crate::EdgePan::default())),
        }
        .share();
        let mut harness = edge_pan_harness(session.clone(), &graph);
        let before = view_of(&mut harness);
        assert_eq!(harness.root_widget().modal_depth(), 1, "{case}: the node is held");
        hold_still(&mut harness);
        assert_eq!(view_of(&mut harness), before, "{case}: the view stands");
        assert_eq!(
            harness.root_widget().edge_pan_counters().frames,
            0,
            "{case}: no frames asked for"
        );
        // Past the edge of the editor, the node goes on following the pointer.
        harness.mouse_move(Point::new(1000.0, 200.0));
        assert_eq!(origin(&graph, 0), Point::new(950.0, 170.0), "{case}");
        harness.mouse_button_release(Some(masonry::core::PointerButton::Primary));
        assert_eq!(harness.root_widget().modal_depth(), 0, "{case}");
    }
}

/// A gesture cancelled after scrolling puts the view back with the node, as Blender's does.
#[test]
fn a_cancelled_gesture_puts_the_view_back() {
    let graph = row(3);
    let session = crate::EditorSession::new(&graph)
        .with_edge_pan(Some(crate::EdgePan::default()))
        .share();
    let mut harness = edge_pan_harness(session.clone(), &graph);
    let before = view_of(&mut harness);
    let start = Point::new(0.0, 0.0);
    hold_still(&mut harness);
    assert_ne!(view_of(&mut harness), before, "it scrolled");

    let editor = harness.root_widget().ctx().widget_id();
    harness.set_focus_fallback(Some(editor));
    harness.process_text_event(masonry::core::TextEvent::Keyboard(
        masonry::core::keyboard::KeyboardEvent {
            state: masonry::core::keyboard::KeyState::Down,
            key: masonry::core::keyboard::Key::Named(masonry::core::keyboard::NamedKey::Escape),
            code: masonry::core::keyboard::Code::Escape,
            modifiers: masonry::core::keyboard::Modifiers::empty(),
            ..Default::default()
        },
    ));
    let _ = harness.redraw();
    assert_eq!(harness.root_widget().modal_depth(), 0, "Escape cancelled the move");
    assert_eq!(origin(&graph, 0), start, "the node went back");
    assert_eq!(view_of(&mut harness), before, "and so did the view");
    assert_eq!(harness.root_widget().edge_pan_counters().restores, 1);
    let frames = harness.root_widget().edge_pan_counters().frames;
    hold_still(&mut harness);
    assert_eq!(harness.root_widget().edge_pan_counters().frames, frames);
    harness.mouse_button_release(Some(masonry::core::PointerButton::Primary));
}

// --- MARK: COPIES (§57)

mod copies {
    use std::collections::BTreeSet;

    use blazy_ops::event::OpEvent;
    use blazy_ops::keymap::{Props, Scope};
    use blazy_ops::runtime::{OpRuntime, Seat};
    use masonry::core::keyboard::{Key, Modifiers};
    use masonry::kurbo::{Point, Rect};

    use super::{Row, row};
    use crate::ops::CANVAS_SCOPE;
    use crate::{EditorWorld, Link, NodeGraph, SharedGraph};

    /// Everything the graph holds, as a value: each live node's name, rectangle and number,
    /// and the links as a set. What "undo returns the graph" is compared on.
    pub(super) type State = (Vec<(usize, Rect, u32)>, BTreeSet<(u32, u16, u32, u16)>);

    pub(super) fn state(graph: &SharedGraph<Row>) -> State {
        let graph = graph.borrow();
        let nodes = (0..graph.rects.len())
            .filter_map(|i| graph.rects[i].map(|rect| (i, rect, graph.values[i])))
            .collect();
        let links = graph
            .links
            .iter()
            .map(|l| (l.from, l.from_port, l.to, l.to_port))
            .collect();
        (nodes, links)
    }

    /// Four nodes in a row, `0 → 1 → 2`, with 0 and 1 selected: a link inside the
    /// selection and one leaving it.
    pub(super) fn linked() -> (SharedGraph<Row>, OpRuntime<EditorWorld<Row>>, EditorWorld<Row>) {
        let graph = row(4);
        let mut runtime = crate::ops::runtime();
        let mut world = EditorWorld::new(&graph);
        for (from, to) in [(0, 1), (1, 2)] {
            let link = Props::new().with_int("from", from).with_int("to", to);
            assert_eq!(
                runtime.exec(&mut world, "link.add", &link),
                blazy_ops::OpResult::Finished
            );
        }
        world.select(BTreeSet::from([0, 1]));
        (graph, runtime, world)
    }

    pub(super) fn key(
        runtime: &mut OpRuntime<EditorWorld<Row>>,
        world: &mut EditorWorld<Row>,
        key: &str,
        mods: Modifiers,
    ) {
        let event = OpEvent::Key {
            key: Key::Character(key.into()),
            mods,
            down: true,
        };
        let _ = runtime.dispatch(world, &event, Scope(&CANVAS_SCOPE), Seat::Tree);
    }

    pub(super) fn move_to(runtime: &mut OpRuntime<EditorWorld<Row>>, world: &mut EditorWorld<Row>, pos: Point) {
        world.pointer = pos;
        world.pointer_screen = pos;
        let event = OpEvent::Move {
            pos,
            mods: Modifiers::empty(),
        };
        let _ = runtime.dispatch(world, &event, Scope(&CANVAS_SCOPE), Seat::Tree);
    }

    /// A duplicate of two linked nodes is two new nodes holding what they held, the link
    /// between them and not the one leaving them, selected — one step, which undo takes
    /// back to the graph exactly as it was and redo brings back exactly as it came.
    #[test]
    fn a_duplicate_copies_contents_and_the_links_between_in_one_step() {
        let (graph, mut runtime, mut world) = linked();
        let before = state(&graph);
        let depth = runtime.history().depth();

        let offset = Props::new().with_float("dx", 0.0).with_float("dy", 200.0);
        assert_eq!(
            runtime.exec(&mut world, "node.duplicate", &offset),
            blazy_ops::OpResult::Finished
        );
        assert_eq!(runtime.history().depth(), depth + 1, "one step for the lot");
        let after = state(&graph);
        let rect = |i: usize| graph.borrow().node_rect(i);
        assert_eq!(graph.borrow().node_count(), 6);
        assert_eq!(
            graph.borrow().values[4..],
            [100, 101],
            "the copies hold what they copied"
        );
        assert_eq!(rect(4), rect(0) + masonry::kurbo::Vec2::new(0.0, 200.0));
        assert!(
            graph.borrow().links.contains(&Link::new(4, 5)),
            "the link between them came"
        );
        assert_eq!(graph.borrow().links.len(), 3, "and the one leaving them did not");
        assert_eq!(world.selection(), BTreeSet::from([4, 5]), "the copies are selected");

        assert_eq!(runtime.history_mut().undo(&mut world), Some("node.duplicate"));
        assert_eq!(state(&graph), before, "undo is the graph as it was");
        assert_eq!(world.selection(), BTreeSet::from([0, 1]), "with its selection");
        assert_eq!(runtime.history_mut().redo(&mut world), Some("node.duplicate"));
        assert_eq!(state(&graph), after, "redo is the graph as it came");
    }

    /// A copy pastes in another session over the graph — another area, or another window
    /// — centred on that session's pointer, into that session's history.
    #[test]
    fn a_paste_lands_at_the_pointer_of_the_area_it_runs_in() {
        let (graph, mut here, mut world) = linked();
        let mut there = crate::ops::runtime();
        let mut other = EditorWorld::new(&graph);
        assert!(!there.poll(&mut other, "node.paste", &Props::new()).unwrap_or(true));

        assert_eq!(
            here.exec(&mut world, "node.copy", &Props::new()),
            blazy_ops::OpResult::Finished
        );
        assert_eq!(graph.borrow().node_count(), 4, "a copy changes nothing");
        let depth = here.history().depth();

        let at = Point::new(1000.0, 500.0);
        move_to(&mut there, &mut other, at);
        key(&mut there, &mut other, "v", Modifiers::CONTROL);
        assert_eq!(graph.borrow().node_count(), 6, "Ctrl+V pasted two nodes");
        let pasted = graph.borrow().node_rect(4).union(graph.borrow().node_rect(5));
        assert_eq!(pasted.center(), at, "centred on the pointer of the area that pasted");
        assert!(graph.borrow().links.contains(&Link::new(4, 5)));
        assert_eq!(there.history().depth(), 1, "the step is in the pasting area's history");
        assert_eq!(here.history().depth(), depth, "and not in the copying one's");

        // Again: the clipboard is not used up.
        key(&mut there, &mut other, "v", Modifiers::CONTROL);
        assert_eq!(graph.borrow().node_count(), 8);
    }

    /// `Shift+D` copies and grabs: the copies follow the pointer, a click puts them down,
    /// and one undo takes back both halves. A grab cut off by the runtime — the window
    /// lost focus — leaves the copies where they were made, with their step.
    #[test]
    fn shift_d_grabs_the_copies_and_one_undo_takes_back_both() {
        let (graph, mut runtime, mut world) = linked();
        let before = state(&graph);
        let start = Point::new(10.0, 10.0);
        move_to(&mut runtime, &mut world, start);
        key(&mut runtime, &mut world, "d", Modifiers::SHIFT);
        assert_eq!(runtime.modal_name(), Some("node.duplicate"));
        move_to(&mut runtime, &mut world, start + masonry::kurbo::Vec2::new(30.0, 300.0));
        let moved = graph.borrow().node_rect(0) + masonry::kurbo::Vec2::new(30.0, 300.0);
        assert_eq!(graph.borrow().node_rect(4), moved, "the copy follows the pointer");
        assert_eq!(runtime.history().depth(), 2, "nothing is written while it runs");

        let press = OpEvent::Press {
            button: masonry::ui_events::pointer::PointerButton::Primary,
            pos: world.pointer,
            mods: Modifiers::empty(),
        };
        let _ = runtime.dispatch(&mut world, &press, Scope(&CANVAS_SCOPE), Seat::Tree);
        assert_eq!(runtime.modal_depth(), 0, "a click puts the copies down");
        assert_eq!(runtime.history().depth(), 3, "in one step");
        let placed = state(&graph);

        assert_eq!(runtime.history_mut().undo(&mut world), Some("node.duplicate"));
        assert_eq!(state(&graph), before, "one undo takes back the copy and the move");
        assert_eq!(runtime.history_mut().redo(&mut world), Some("node.duplicate"));
        assert_eq!(state(&graph), placed, "and redo puts them where they were put down");

        // Cut off: the copies stay, at the place they were made, and are undoable.
        key(&mut runtime, &mut world, "d", Modifiers::SHIFT);
        move_to(&mut runtime, &mut world, Point::new(400.0, 400.0));
        runtime.cancel_all(&mut world);
        assert_eq!(graph.borrow().node_count(), 8);
        assert_eq!(graph.borrow().node_rect(6), graph.borrow().node_rect(4));
        assert_eq!(runtime.history_mut().undo(&mut world), Some("node.duplicate"));
        assert_eq!(state(&graph), placed);
    }

    /// A grab the runtime cuts off puts the nodes back, as `Escape` does.
    ///
    /// It used to answer the missing event as it answers every unknown one — keep going —
    /// and the window losing focus mid-drag left the nodes where the pointer had taken
    /// them, with no step in the history to undo it by.
    #[test]
    fn a_grab_cut_off_puts_the_nodes_back() {
        let (graph, mut runtime, mut world) = linked();
        let before = state(&graph);
        move_to(&mut runtime, &mut world, Point::ORIGIN);
        key(&mut runtime, &mut world, "g", Modifiers::empty());
        move_to(&mut runtime, &mut world, Point::new(50.0, 70.0));
        assert_ne!(state(&graph), before);
        runtime.cancel_all(&mut world);
        assert_eq!(state(&graph), before);
    }

    /// Undoing a delete brings back what the nodes held and every link between them.
    ///
    /// Two defects the copies found: the content was left to the graph's slot — which one
    /// example wiped, so a deleted node came back with its slider at the default — and the
    /// nodes came back one at a time with their links, so a link between two deleted
    /// nodes was offered while its other end was still gone, refused, and lost.
    #[test]
    fn an_undone_delete_brings_back_contents_and_the_links_between() {
        let (graph, mut runtime, mut world) = linked();
        let before = state(&graph);
        assert_eq!(
            runtime.exec(&mut world, "node.delete", &Props::new()),
            blazy_ops::OpResult::Finished
        );
        assert_eq!(graph.borrow().node_count(), 2);
        assert_eq!(runtime.history_mut().undo(&mut world), Some("node.delete"));
        assert_eq!(state(&graph), before);
    }
}

// --- MARK: CUT OFF (`issues/modal operators cut off without an event.md`)

mod cut_off {
    use blazy_canvas::{CanvasHit, PortSide};
    use blazy_ops::event::OpEvent;
    use blazy_ops::keymap::Scope;
    use blazy_ops::runtime::{OpRuntime, Seat};
    use masonry::core::keyboard::Modifiers;
    use masonry::kurbo::{Point, Vec2};
    use masonry::ui_events::pointer::PointerButton;

    use super::Row;
    use super::copies::{key, linked, move_to, state};
    use crate::EditorWorld;
    use crate::ops::CANVAS_SCOPE;

    type Runtime = OpRuntime<EditorWorld<Row>>;
    /// What takes an operator from nothing to the middle of its gesture.
    type Begin = fn(&mut Runtime, &mut EditorWorld<Row>);

    fn send(runtime: &mut Runtime, world: &mut EditorWorld<Row>, event: &OpEvent) {
        let _ = runtime.dispatch(world, event, Scope(&CANVAS_SCOPE), Seat::Tree);
    }

    /// A drag that began at `at`, as the runtime hands one on once it is decided.
    fn drag(button: PointerButton, at: Point) -> OpEvent {
        OpEvent::Drag {
            button,
            pos: at,
            screen: at,
            mods: Modifiers::empty(),
        }
    }

    /// How each modal operator is started and taken somewhere, and whether it is recorded
    /// when cut off (`true`) or leaves the graph as it found it (`false`). `None` for an
    /// operator that is never modal.
    ///
    /// Keyed by the registry's names, and the test fails on a name this does not know: a
    /// new operator has to be put here, modal or not, before it passes.
    fn start(name: &str) -> Option<Vec<(Begin, bool)>> {
        Some(vec![match name {
            "node.box_select" => (
                |runtime, world| {
                    key(runtime, world, "b", Modifiers::empty());
                    let press = OpEvent::Press {
                        button: PointerButton::Primary,
                        pos: world.pointer,
                        mods: Modifiers::empty(),
                    };
                    send(runtime, world, &press);
                    move_to(runtime, world, Point::new(300.0, 200.0));
                    assert!(world.band.is_some(), "the band is showing");
                },
                false,
            ),
            "node.move" => (
                |runtime, world| {
                    key(runtime, world, "g", Modifiers::empty());
                    move_to(runtime, world, Point::new(50.0, 70.0));
                },
                false,
            ),
            "node.duplicate" => (
                |runtime, world| {
                    key(runtime, world, "d", Modifiers::SHIFT);
                    move_to(runtime, world, Point::new(50.0, 70.0));
                },
                // The duplicate happened; only the move is cut off (§57).
                true,
            ),
            "view.pan" => (
                |runtime, world| {
                    world.hover = None;
                    send(runtime, world, &drag(PointerButton::Primary, world.pointer));
                    move_to(runtime, world, Point::new(40.0, 30.0));
                    assert_ne!(world.pan, Vec2::ZERO, "the view moved");
                },
                false,
            ),
            "link.drag" => {
                return Some(vec![
                    (
                        |runtime, world| {
                            world.hover = Some(CanvasHit::Port {
                                index: 0,
                                side: PortSide::Output,
                                port: 0,
                                pos: world.pointer,
                            });
                            send(runtime, world, &drag(PointerButton::Primary, world.pointer));
                            move_to(runtime, world, Point::new(300.0, 200.0));
                            assert!(world.link_preview.is_some(), "the curve is showing");
                        },
                        false,
                    ),
                    // Off an input with a link: cut off, the link has to come back, or it
                    // stays off with nothing in the history to undo it by.
                    (
                        |runtime, world| {
                            world.hover = Some(CanvasHit::Port {
                                index: 1,
                                side: PortSide::Input,
                                port: 0,
                                pos: world.pointer,
                            });
                            send(runtime, world, &drag(PointerButton::Primary, world.pointer));
                            move_to(runtime, world, Point::new(300.0, 200.0));
                            assert_eq!(world.graph.borrow().links.len(), 1, "the link came off");
                        },
                        false,
                    ),
                ]);
            },
            "node.select" | "view.zoom" | "menu.node" | "node.add" | "node.delete" | "node.copy" | "node.paste"
            | "link.add" | "link.delete" | "ed.undo" | "ed.redo" => return None,
            other => panic!("`{other}` is not classified: modal or not, put it in `start`"),
        }])
    }

    /// Every modal operator of the editor, cut off mid-gesture — the window lost focus —
    /// ends, says so, and leaves nothing behind: no band, no curve, no pick on every move,
    /// and a graph that is either as it was or recorded in one step.
    ///
    /// `link.drag` and `node.box_select` used to answer the missing event as they answer
    /// every unknown one — keep going — and their curve and band stayed on screen; `node.move`
    /// did the same until §57.
    #[test]
    fn every_modal_operator_cut_off_leaves_nothing_behind() {
        let names = crate::ops::runtime::<Row>().operators().to_vec();
        let mut modal = 0;
        let scenarios = names
            .into_iter()
            .filter_map(|name| Some(start(name)?.into_iter().map(move |scenario| (name, scenario))))
            .flatten();
        for (name, (begin, recorded)) in scenarios {
            modal += 1;
            let (graph, mut runtime, mut world) = linked();
            move_to(&mut runtime, &mut world, Point::new(10.0, 10.0));
            let before = state(&graph);
            let depth = runtime.history().depth();
            begin(&mut runtime, &mut world);
            assert_eq!(runtime.modal_name(), Some(name), "{name} is running");

            runtime.cancel_all(&mut world);
            assert_eq!(runtime.modal_depth(), 0);
            assert_eq!(
                runtime.counters().cut_off_running,
                0,
                "{name} answered as if it went on"
            );
            assert_eq!(world.band, None, "{name} left a band");
            assert_eq!(world.link_preview, None, "{name} left a curve");
            assert!(!world.track_hover, "{name} left the pick on every move");
            if name == "view.pan" {
                // Not a trace: the view has no history, and one that jumps back when the
                // window loses focus is a surprise (`PanOp::modal`).
                assert_ne!(world.pan, Vec2::ZERO, "a pan cut off keeps the view where it went");
            }
            if recorded {
                assert_eq!(runtime.history().depth(), depth + 1, "{name} recorded what it did");
            } else {
                assert_eq!(state(&graph), before, "{name} left the graph changed");
                assert_eq!(runtime.history().depth(), depth, "{name} wrote a step");
            }
        }
        assert_eq!(modal, 6, "every modal operator was reached, `link.drag` twice");
    }
}
