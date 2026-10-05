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

/// The smallest graph the seam allows: rectangles in a row, its links, and the views
/// showing it.
///
/// Names are indices with holes, and the holes are handed out again before fresh names
/// — the free list of §41.2, which is what `insert_node` asks an application for.
#[derive(Default)]
struct Row {
    rects: Vec<Option<Rect>>,
    free: Vec<usize>,
    links: Vec<Link>,
    views: Views,
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
        match self.free.pop() {
            Some(index) => {
                self.rects[index] = Some(rect);
                index
            },
            None => {
                self.rects.push(Some(rect));
                self.rects.len() - 1
            },
        }
    }

    fn restore_node(&mut self, index: usize, rect: Rect) {
        if self.rects.len() <= index {
            self.rects.resize(index + 1, None);
        }
        if let Some(at) = self.free.iter().position(|&free| free == index) {
            self.free.swap_remove(at);
        }
        self.rects[index] = Some(rect);
    }

    fn remove_node(&mut self, index: usize) -> Vec<Link> {
        if self.rects.get(index).copied().flatten().is_none() {
            return Vec::new();
        }
        self.rects[index] = None;
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
}

/// Two links are the same link whichever way round they are written.
fn same_link(a: Link, b: Link) -> bool {
    (a.from, a.to) == (b.from, b.to) || (a.from, a.to) == (b.to, b.from)
}

const SIZE: Size = Size::new(100.0, 60.0);

fn row(count: usize) -> SharedGraph<Row> {
    let rects = (0..count)
        .map(|i| Some(Rect::from_origin_size(Point::new(i as f64 * 150.0, 0.0), SIZE)))
        .collect();
    Rc::new(RefCell::new(Row {
        rects,
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
    assert_eq!(world.selection.iter().copied().collect::<Vec<_>>(), [1, 2]);
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
    assert_eq!(world.selection.iter().copied().collect::<Vec<_>>(), [1], "and selected");
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

/// A harness over a fresh canvas and an existing session.
fn editor_harness(graph: &SharedGraph<Row>, session: &crate::SessionHandle<Row>) -> TestHarness<NodeEditor<Row>> {
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
        PhysicalSize::new(800, 400),
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
