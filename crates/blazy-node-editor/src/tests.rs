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
    a == b || a == b.reversed()
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
#[test]
fn a_menu_opened_by_the_edge_stays_inside_the_window() {
    use masonry::core::TextEvent;
    use masonry::core::keyboard::{Code, Key, KeyState, KeyboardEvent, Modifiers};
    use masonry::kurbo::Point;
    let graph = row(3);
    let session = crate::EditorSession::new(&graph).share();
    let mut harness = editor_harness(&graph, &session);
    let editor = harness.root_widget().ctx().widget_id();
    harness.set_focus_fallback(Some(editor));
    harness.mouse_move(Point::new(790.0, 390.0));
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
    assert!(origin.y + size.height <= 400.0 + 1e-9, "{origin:?} {size:?}");
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
}
