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
use crate::model::{NODE_SIZE, SharedGraph};
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

/// Moves the pointer over node `index` and settles the resulting passes.
///
/// Controls are materialised only for the node under the pointer, so anything that
/// wants to touch a slider or a checkbox has to hover first — exactly as a user does.
fn hover_node(harness: &mut TestHarness<NodeEditor>, index: usize) {
    let centre = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            let pos = CanvasLayer::child_pos(&mut canvas, index).expect("node exists");
            masonry::kurbo::Point::new(pos.x + NODE_SIZE.width / 2.0, pos.y + 6.0)
        })
    });
    harness.mouse_move(centre);
    let _ = harness.redraw();
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

    // Pick a node comfortably inside the viewport: the canvas clips to its bounds,
    // and a control hanging off the left edge is not clickable.
    let live_now = live(&mut harness);
    let index = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            live_now
                .iter()
                .map(|(i, _)| *i)
                .find(|i| CanvasLayer::child_pos(&mut canvas, *i).is_some_and(|p| p.x > 40.0 && p.y > 40.0))
                .expect("some node should be fully inside the viewport")
        })
    });

    hover_node(&mut harness, index);

    let before = graph.borrow().node(index).checked;
    let id = live(&mut harness)
        .into_iter()
        .find(|(i, _)| *i == index)
        .map(|(_, id)| id)
        .expect("hovered node should be live");
    let checkbox = {
        let widget = harness.get_widget_with_id(id);
        widget
            .downcast::<GraphNode>()
            .expect("live child should be a GraphNode")
            .checkbox_id()
            .expect("the hovered node should have controls")
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

/// The HUD must survive being cached.
///
/// Its shaped text is now rebuilt only when the string changes, which is exactly the
/// kind of optimisation that shows up as a large speed-up when it silently stops
/// drawing. Same guard as `far_field_is_actually_painted`: look at the pixels.
#[test]
fn hud_is_painted_and_survives_reshaping() {
    let (mut harness, _graph) = harness(500);

    let lit_in_hud = |image: &image::RgbaImage| {
        let h = image.height();
        image
            .enumerate_pixels()
            .filter(|(_, y, p)| {
                // Bottom strip only, and brighter than the HUD panel background.
                *y > h - 46 && p.0[0] as u32 + p.0[1] as u32 + p.0[2] as u32 > 3 * 0x60
            })
            .count()
    };

    let before = lit_in_hud(&harness.render());
    assert!(before > 50, "expected HUD text pixels, got {before}");

    // Force the text to change, which invalidates the cached shaping.
    zoom_out(&mut harness, 0.5);
    let after = lit_in_hud(&harness.render());
    assert!(
        after > 50,
        "HUD disappeared after its text changed, got {after} lit pixels"
    );
}

/// Controls exist for exactly one node: the one under the pointer.
///
/// Everything else shows a painted stand-in. Stashing the controls instead would look
/// identical on screen and cost the same as before, which is exactly the trap section
/// 20.2 of the architecture note describes.
#[test]
fn only_the_hovered_node_has_controls() {
    let (mut harness, _graph) = harness(5000);

    let with_controls = |h: &mut TestHarness<NodeEditor>| -> Vec<usize> {
        live(h)
            .into_iter()
            .filter(|(_, id)| {
                h.get_widget_with_id(*id)
                    .downcast::<GraphNode>()
                    .expect("a GraphNode")
                    .checkbox_id()
                    .is_some()
            })
            .map(|(i, _)| i)
            .collect()
    };

    assert!(
        with_controls(&mut harness).is_empty(),
        "nothing is hovered, so no node should carry control widgets"
    );

    let target = live(&mut harness)
        .iter()
        .map(|(i, _)| *i)
        .find(|i| *i > 0)
        .expect("several nodes on screen");
    hover_node(&mut harness, target);

    assert_eq!(
        with_controls(&mut harness),
        vec![target],
        "exactly the hovered node should carry control widgets"
    );
}

/// Rebuilding a node at a new detail level must not lose the user's edits.
#[test]
fn detail_rebuild_preserves_state() {
    let (mut harness, graph) = harness(5000);
    let target = live(&mut harness)
        .iter()
        .map(|(i, _)| *i)
        .find(|i| *i > 0)
        .expect("several nodes on screen");

    graph.borrow_mut().set_value(target, 0.625);

    // Hovering promotes the node to Full, which rebuilds it with real controls.
    hover_node(&mut harness, target);

    let id = live(&mut harness)
        .into_iter()
        .find(|(i, _)| *i == target)
        .map(|(_, id)| id)
        .expect("hovered node should be live");
    let widget = harness.get_widget_with_id(id);
    let node = widget.downcast::<GraphNode>().expect("a GraphNode");
    assert!(node.checkbox_id().is_some(), "hovering should have promoted the node");
    assert_eq!(
        node.built_value(),
        0.625,
        "rebuilding at a new detail level dropped the model value"
    );
}

/// Panning inside the far field must not re-record its scene.
///
/// The scene is stored in canvas coordinates, so a pan is a change of one `Affine`
/// and nothing else. This is the property the whole vector-display-list argument
/// rests on, so it is worth asserting rather than assuming.
#[test]
fn far_field_does_not_repaint_while_panning() {
    let (mut harness, _graph) = harness(5000);
    zoom_out(&mut harness, 0.04);

    let stats = |h: &mut TestHarness<NodeEditor>| h.edit_root_widget(|editor| editor.widget.stats());

    // The counters are captured during layout, which runs before paint, so let one
    // pan settle before reading the baseline.
    pan(&mut harness, Vec2::new(-6.0, -2.0));
    pan(&mut harness, Vec2::new(-6.0, -2.0));
    let before = stats(&mut harness).far_repaints;
    assert!(before > 0, "entering the far field should have recorded a scene");

    for _ in 0..30 {
        pan(&mut harness, Vec2::new(-6.0, -2.0));
    }

    let after = stats(&mut harness).far_repaints;
    assert_eq!(
        after,
        before,
        "panning re-recorded the far-field scene {} times",
        after - before
    );
}
