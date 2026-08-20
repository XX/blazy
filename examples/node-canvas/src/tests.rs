//! Correctness tests for virtualisation.
//!
//! The benchmark answers whether virtualisation is *fast*. These answer whether it
//! is *correct*, which is the harder half: a canvas that quietly loses the user's
//! edits when a node scrolls off screen would post excellent numbers.

use blazy_canvas::CanvasLayer;
use masonry::core::NewWidget;
use masonry::dpi::PhysicalSize;
use masonry::kurbo::Vec2;
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;
use masonry::ui_events::pointer::PointerButton;

use crate::build_canvas;
use crate::editor::NodeEditor;
use crate::model::SharedGraph;
use crate::node::GraphNode;

fn harness(count: usize) -> (TestHarness<NodeEditor>, SharedGraph) {
    let (canvas, graph) = build_canvas(count);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::new(canvas)),
        PhysicalSize::new(1100, 750),
    );
    let _ = harness.redraw();
    (harness, graph)
}

fn pan(harness: &mut TestHarness<NodeEditor>, delta: Vec2) {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::pan(&mut canvas, delta);
        });
    });
    let _ = harness.redraw();
}

fn live(harness: &mut TestHarness<NodeEditor>) -> Vec<(usize, masonry::core::WidgetId)> {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::live_children(&mut canvas))
    })
}

#[test]
fn only_visible_nodes_are_materialised() {
    let (mut harness, _graph) = harness(5000);
    let live = live(&mut harness);
    assert!(
        live.len() < 100,
        "expected a viewport-bounded number of widgets, got {}",
        live.len()
    );
}

#[test]
fn materialised_count_is_independent_of_graph_size() {
    let (mut small, _a) = harness(500);
    let (mut large, _b) = harness(20_000);
    let small = live(&mut small).len();
    let large = live(&mut large).len();
    assert_eq!(
        small, large,
        "a 40x bigger graph materialised a different number of widgets ({small} vs {large})"
    );
}

#[test]
fn nodes_dematerialise_when_panned_away() {
    let (mut harness, _graph) = harness(5000);
    let before: Vec<_> = live(&mut harness).iter().map(|(i, _)| *i).collect();
    assert!(before.contains(&0), "node 0 should start on screen");

    // Pan far enough that the original viewport is nowhere near the visible region.
    pan(&mut harness, Vec2::new(-5000.0, -3000.0));

    let after: Vec<_> = live(&mut harness).iter().map(|(i, _)| *i).collect();
    assert!(
        !after.contains(&0),
        "node 0 should have left the tree after panning away"
    );
    assert!(!after.is_empty(), "some other nodes should have entered");
}

#[test]
fn state_survives_a_round_trip_out_of_view() {
    let (mut harness, graph) = harness(5000);
    assert!(live(&mut harness).iter().any(|(i, _)| *i == 0));

    // Simulate an edit that a control inside node 0 would have written back.
    graph.borrow_mut().set_value(0, 0.875);

    pan(&mut harness, Vec2::new(-5000.0, -3000.0));
    assert!(!live(&mut harness).iter().any(|(i, _)| *i == 0));

    pan(&mut harness, Vec2::new(5000.0, 3000.0));
    let live = live(&mut harness);
    let (_, id) = live
        .iter()
        .find(|(i, _)| *i == 0)
        .expect("node 0 should be back on screen");

    let widget = harness.get_widget_with_id(*id);
    let node = widget.downcast::<GraphNode>().expect("node 0 should be a GraphNode");
    assert_eq!(
        node.built_value(),
        0.875,
        "the rebuilt widget did not pick up the model's current value"
    );
}

#[test]
fn controls_write_back_to_the_model() {
    let (mut harness, graph) = harness(500);
    let live = live(&mut harness);

    // Pick a node comfortably inside the viewport: the canvas clips to its bounds,
    // and a control hanging off the left edge is not clickable.
    let (index, id) = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            live.iter()
                .copied()
                .find(|(i, _)| CanvasLayer::child_pos(&mut canvas, *i).is_some_and(|p| p.x > 40.0 && p.y > 40.0))
                .expect("some node should be fully inside the viewport")
        })
    });

    let before = graph.borrow().node(index).checked;
    let checkbox = {
        let widget = harness.get_widget_with_id(id);
        widget
            .downcast::<GraphNode>()
            .expect("live child should be a GraphNode")
            .checkbox_id()
    };

    harness.mouse_click_on(checkbox, Some(PointerButton::Primary));

    let after = graph.borrow().node(index).checked;
    assert_ne!(before, after, "toggling the checkbox should have reached the model");
}

/// Zooms out far enough that the canvas switches to far-field painting.
fn zoom_out(harness: &mut TestHarness<NodeEditor>, factor: f64) {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::zoom_around(&mut canvas, masonry::kurbo::Point::new(550.0, 375.0), factor);
        });
    });
    let _ = harness.redraw();
}

#[test]
fn far_field_materialises_no_widgets() {
    let (mut harness, _graph) = harness(5000);
    assert!(!live(&mut harness).is_empty());

    zoom_out(&mut harness, 0.05);
    assert!(
        live(&mut harness).is_empty(),
        "below the box threshold the canvas should paint nodes instead of building them"
    );
}

/// The far field must actually be drawn.
///
/// Without this the optimisation would look like a huge win precisely because it
/// stopped rendering anything: no widgets and no painting is very fast and very
/// wrong. Rendering to an image and counting non-background pixels is the only
/// check that cannot be satisfied by doing nothing.
#[test]
fn far_field_is_actually_painted() {
    let (mut harness, _graph) = harness(5000);
    zoom_out(&mut harness, 0.05);
    assert!(live(&mut harness).is_empty(), "expected far-field mode");

    let image = harness.render();
    // The editor paints a near-black background; node tints are all lighter.
    let lit = image
        .pixels()
        .filter(|p| p.0[0] as u32 + p.0[1] as u32 + p.0[2] as u32 > 3 * 0x40)
        .count();
    let total = image.pixels().count();
    assert!(
        lit > total / 100,
        "expected the far field to cover a meaningful part of the canvas, \
         got {lit} lit pixels of {total}"
    );
}

#[test]
fn far_field_nodes_stay_draggable() {
    let (mut harness, _graph) = harness(5000);
    zoom_out(&mut harness, 0.05);

    let before = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            let p = CanvasLayer::child_pos(&mut canvas, 7).unwrap();
            CanvasLayer::move_child(&mut canvas, 7, masonry::kurbo::Point::new(p.x + 500.0, p.y));
            p
        })
    });
    let _ = harness.redraw();

    let after = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::child_pos(&mut canvas, 7).unwrap()
        })
    });
    assert_eq!(
        after.x,
        before.x + 500.0,
        "a node with no widget should still be movable through the model"
    );
}
