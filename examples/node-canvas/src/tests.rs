//! Correctness tests for virtualisation.
//!
//! The benchmark answers whether virtualisation is *fast*. These answer whether it
//! is *correct*, which is the harder half: a canvas that quietly loses the user's
//! edits when a node scrolls off screen would post excellent numbers.

use blazy_canvas::{CanvasHit, CanvasLayer};
use masonry::core::{NewWidget, WidgetId, WidgetRef};
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Point, Vec2};
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;
use masonry::ui_events::pointer::PointerButton;

use crate::build_canvas_with;
use crate::editor::NodeEditor;
use crate::model::{NODE_SIZE, SharedGraph};
use crate::node::GraphNode;

fn harness(count: usize) -> (TestHarness<NodeEditor>, SharedGraph) {
    harness_with(count, false)
}

fn harness_with(count: usize, controls_on_hover: bool) -> (TestHarness<NodeEditor>, SharedGraph) {
    let (canvas, graph) = build_canvas_with(count, controls_on_hover);
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

/// Borrows a live node as its concrete type.
fn node_ref<'a>(harness: &'a TestHarness<NodeEditor>, id: WidgetId) -> WidgetRef<'a, GraphNode> {
    harness
        .get_widget_with_id(id)
        .downcast::<GraphNode>()
        .expect("a canvas child should be a GraphNode")
}

/// Whether the node with this id currently carries real control widgets.
fn has_controls(harness: &TestHarness<NodeEditor>, id: WidgetId) -> bool {
    node_ref(harness, id).checkbox_id().is_some()
}

/// The live index/id pair for `index`, if it is materialised.
fn live_id(harness: &mut TestHarness<NodeEditor>, index: usize) -> Option<WidgetId> {
    live(harness).into_iter().find(|(i, _)| *i == index).map(|(_, id)| id)
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

    assert_eq!(
        node_ref(&harness, *id).built_value(),
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
    let id = live_id(&mut harness, index).expect("hovered node should be live");
    let checkbox = node_ref(&harness, id)
        .checkbox_id()
        .expect("the hovered node should have controls");

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

/// By default every node on screen at `Full` carries real controls.
///
/// The painted stand-in is for `Simplified` only, where the node is too small to
/// interact with anyway. Using it at `Full` is possible but off by default: see
/// `only_the_hovered_node_has_controls`.
#[test]
fn full_detail_nodes_all_have_controls() {
    let (mut harness, _graph) = harness(5000);
    let live = live(&mut harness);
    assert!(!live.is_empty());
    for (_, id) in &live {
        assert!(
            has_controls(&harness, *id),
            "a node at full detail should carry real controls"
        );
    }
}

/// With `with_controls_on_hover`, controls exist for exactly one node.
///
/// Stashing them instead would look identical on screen and cost the same as before,
/// which is exactly the trap section 20.2 of the architecture note describes.
#[test]
fn only_the_hovered_node_has_controls() {
    let (mut harness, _graph) = harness_with(5000, true);

    let with_controls = |h: &mut TestHarness<NodeEditor>| -> Vec<usize> {
        live(h)
            .into_iter()
            .filter(|(_, id)| has_controls(h, *id))
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

    // Crossing into Simplified drops the controls; coming back rebuilds them, and the
    // rebuilt widget has to pick up what the model holds now.
    zoom_out(&mut harness, 0.2);
    assert!(
        live(&mut harness).iter().any(|(i, _)| *i == target),
        "the node should still be materialised at simplified detail"
    );
    zoom_out(&mut harness, 5.0);

    let id = live_id(&mut harness, target).expect("node should be live again");
    let node = node_ref(&harness, id);
    assert!(node.checkbox_id().is_some(), "back at full detail the controls return");
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
    let before = stats(&mut harness).counters.far_repaints;
    assert!(before > 0, "entering the far field should have recorded a scene");

    for _ in 0..30 {
        pan(&mut harness, Vec2::new(-6.0, -2.0));
    }

    let after = stats(&mut harness).counters.far_repaints;
    assert_eq!(
        after,
        before,
        "panning re-recorded the far-field scene {} times",
        after - before
    );
}

// --- MARK: PICKING

/// What the widget tree says is under a window position.
fn under_pointer(harness: &TestHarness<NodeEditor>, pos: Point) -> Option<WidgetId> {
    harness
        .root_widget()
        .as_dyn()
        .find_widget_under_pointer(pos)
        .map(|widget| widget.id())
}

/// The canvas's own statistics.
///
/// Not `NodeEditor::stats`, which is a copy taken during layout: a pick deliberately
/// does not run one (§25.4), so the copy would be from before the pointer moved.
fn canvas_stats(harness: &mut TestHarness<NodeEditor>) -> blazy_canvas::CanvasStats {
    harness.edit_root_widget(|mut editor| NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.stats()))
}

/// What the canvas says is under a window position.
fn pick(harness: &mut TestHarness<NodeEditor>, pos: Point) -> Option<CanvasHit> {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::hit_test(&mut canvas, pos))
    })
}

/// The canvas-space position of a node, which at the identity view is also its
/// position in the window.
fn node_pos(harness: &mut TestHarness<NodeEditor>, index: usize) -> Point {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::child_pos(&mut canvas, index).expect("node exists")
        })
    })
}

/// A node that sits well inside the viewport, with its right-hand neighbour.
///
/// The generated grid starts at the origin and jitters, so the first few nodes hang
/// off the top-left corner of the window; a test about window positions has to pick
/// one that is actually in the window.
fn node_inside_the_viewport(harness: &mut TestHarness<NodeEditor>) -> usize {
    // Row 0 straddles the top edge of the window, so the search has to reach the
    // second row: the grid is 80 wide.
    (0..200)
        .find(|&index| {
            let pos = node_pos(harness, index);
            pos.x > 20.0 && pos.y > 20.0 && pos.x + NODE_SIZE.width < 900.0 && pos.y + NODE_SIZE.height < 600.0
        })
        .expect("some node is fully on screen")
}

/// The precise phase, in the widget tree: a node's corner is drawn round, so it is
/// not the node.
///
/// This is the half of `blazy-shape` that runs inside Masonry's own descent
/// (§6.2) — the canvas is not consulted at all.
#[test]
fn a_rounded_corner_is_not_the_node_in_the_widget_tree() {
    let (mut harness, _graph) = harness(500);
    let index = node_inside_the_viewport(&mut harness);
    let id = live_id(&mut harness, index).expect("the node is on screen");
    let pos = node_pos(&mut harness, index);

    let corner = Point::new(pos.x + 1.0, pos.y + 1.0);
    let header = Point::new(pos.x + NODE_SIZE.width / 2.0, pos.y + 6.0);

    assert_eq!(under_pointer(&harness, header), Some(id), "the body is the node");
    assert_ne!(
        under_pointer(&harness, corner),
        Some(id),
        "a point outside the rounded corner must fall through to what is behind"
    );
}

/// The same question asked of the canvas, which answers from the model.
#[test]
fn the_canvas_picks_the_node_by_its_shape() {
    let (mut harness, _graph) = harness(500);
    let index = node_inside_the_viewport(&mut harness);
    let pos = node_pos(&mut harness, index);

    let centre = Point::new(pos.x + NODE_SIZE.width / 2.0, pos.y + NODE_SIZE.height / 2.0);
    assert_eq!(pick(&mut harness, centre).and_then(CanvasHit::node), Some(index));
    // The corner falls through to whatever is behind it, which in a wired graph is
    // usually a link — that is what falling through is for.
    assert_eq!(
        pick(&mut harness, Point::new(pos.x + 1.0, pos.y + 1.0)).and_then(CanvasHit::node),
        None
    );
}

/// A link is not a widget, so this is the only route to it that exists.
#[test]
fn a_link_can_be_picked_and_highlights_under_the_pointer() {
    let (mut harness, _graph) = harness(500);
    // Every node is wired to its right-hand neighbour; the curve leaves the right
    // edge at half height and arrives at the next node's left edge, and its midpoint
    // is the midpoint of the two ends because the handles are symmetric.
    let index = node_inside_the_viewport(&mut harness);
    let from = node_pos(&mut harness, index);
    let to = node_pos(&mut harness, index + 1);
    let on_curve = Point::new(
        (from.x + NODE_SIZE.width + to.x) / 2.0,
        (from.y + to.y) / 2.0 + NODE_SIZE.height / 2.0,
    );

    let hit = pick(&mut harness, on_curve);
    assert!(
        matches!(hit, Some(CanvasHit::Link { .. })),
        "expected a link at {on_curve:?}, got {hit:?}"
    );

    // And the interactive route: moving the pointer there records the same thing.
    harness.mouse_move(on_curve);
    let _ = harness.redraw();
    assert_eq!(canvas_stats(&mut harness).hovered, hit);
}

/// Below the far-field threshold nothing is a widget, and picking still works.
///
/// The point of answering from the model: `find_widget_under_pointer` has nothing
/// to find here, because the nodes are painted into the canvas's own scene.
#[test]
fn picking_works_where_there_are_no_widgets() {
    let (mut harness, _graph) = harness(5000);
    zoom_out(&mut harness, 0.05);
    assert!(live(&mut harness).is_empty(), "far field: no widgets at all");

    let pos = node_pos(&mut harness, 0);
    let view_zoom = canvas_stats(&mut harness).zoom;
    let centre = Point::new(pos.x + NODE_SIZE.width / 2.0, pos.y + NODE_SIZE.height / 2.0);
    // Canvas coordinates to window coordinates, by hand: the view is a scale about
    // the viewport centre, and this test knows how it got there.
    let centre = Point::new(550.0, 375.0) + (centre - Point::new(550.0, 375.0)) * view_zoom;

    // Some widget is always under the pointer — the canvas itself — but no node is.
    let found = under_pointer(&harness, centre).expect("the canvas is there");
    assert!(
        harness.get_widget_with_id(found).downcast::<GraphNode>().is_none(),
        "the far field has no node widgets to find"
    );
    assert_eq!(pick(&mut harness, centre).and_then(CanvasHit::node), Some(0));
}

/// Picking must not drag a layout pass behind it: a pointer moving over a graph is
/// the most common thing that happens to a canvas (§25.4).
#[test]
fn hovering_does_not_relayout() {
    let (mut harness, _graph) = harness(5000);
    let before = canvas_stats(&mut harness).counters;

    for i in 0..40 {
        harness.mouse_move(Point::new(120.0 + i as f64 * 12.0, 300.0));
        let _ = harness.redraw();
    }

    let after = canvas_stats(&mut harness).counters;
    assert_eq!(
        after.child_layouts,
        before.child_layouts,
        "hovering laid out {} children",
        after.child_layouts - before.child_layouts
    );
    assert!(after.hit_queries >= 40, "the moves were picked: {}", after.hit_queries);
}

/// One look at the whole graph must not make the close-up view slow for ever.
///
/// §28: the recorded regions used to be able to grow and never shrink, so a trip to
/// the overview zoom left every edge in the graph recorded, and the canvas kept
/// drawing all of them at a zoom whose viewport held a hundred. Measured before the
/// fix: 9857 curves and 2.46 ms a frame where a fresh canvas took 0.27.
#[test]
fn a_look_at_the_whole_graph_does_not_stay_expensive() {
    let (mut harness, _graph) = harness(5000);
    let close = canvas_stats(&mut harness).recorded_links;
    assert!(close > 0, "the viewport holds some links to begin with");

    zoom_out(&mut harness, 0.05);
    let wide = canvas_stats(&mut harness);
    assert!(
        wide.recorded_links > close * 10,
        "the overview really does record the graph: {} against {close}",
        wide.recorded_links
    );

    // Back to where we started.
    zoom_out(&mut harness, 20.0);
    let back = canvas_stats(&mut harness);
    assert!(
        back.recorded_links <= close * 2,
        "the recorded set stayed at {} after returning, against {close} on arrival",
        back.recorded_links
    );
    assert_eq!(back.recorded_far, 0, "and the far field let its nodes go");
}
