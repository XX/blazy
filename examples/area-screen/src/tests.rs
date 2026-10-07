//! Phase 1.1: does the renderer actually keep its promise?
//!
//! Every other test in this workspace counts something — widgets in the tree, layouts
//! per frame, area resizes. None of them look at a pixel. These do, because
//! `rnd/architecture.md` §9 makes a claim about pixels that the whole choice of
//! `imaging` partly rests on: changing any of the three multipliers is correct
//! *without loss of sharpness*, which an atlas-based renderer or a tessellator that
//! fixed its geometry at one scale could not manage.
//!
//! The three multipliers are checked separately because they travel by different
//! routes. `ui_scale` goes through layout; `view` and `device_scale` are transforms
//! applied at composition. Two of the three can be driven through Masonry's test
//! harness; the third cannot, and §23 says why.

use bench_utils::render::{block_magnified, differing_fraction, sharpness_gain};
use blazy::areas::{AreaContent, RegionKind, UiScale};
use blazy::canvas::CanvasLayer;
use blazy::masonry::core::{NewWidget, WidgetId};
use blazy::masonry::dpi::PhysicalSize;
use blazy::masonry::kurbo::{Point, Rect, Size, Vec2};
use blazy::masonry::peniko::Color;
use blazy::masonry::testing::{TestHarness, TestHarnessParams, assert_render_snapshot};
use blazy::masonry::ui_events::pointer::PointerButton;
use blazy::node_editor::Change;
use blazy::shell::Host;
use image::RgbaImage;
use node_canvas::editor::NodeEditor;
use node_canvas::model::SharedGraph;
use node_canvas::node::GraphNode;
use node_canvas::{build_canvas, property_set};

use crate::header::ScaledHeader;
use crate::{Screen, build_screen};

/// Base viewport for the magnification tests, kept small so the images stay cheap.
const BASE: (u32, u32) = (160, 40);
/// How much everything is magnified by.
const MAGNIFY: u32 = 4;

/// How much sharper a redraw must be than a smooth upscale of the same content.
///
/// Measured gains are 4–5x; the bound is set low enough that anti-aliasing changes or
/// a different content mix cannot trip it, and high enough that a renderer which
/// magnified a bitmap (gain ≈ 1.0) fails immediately.
const MIN_GAIN: f64 = 2.0;

const TINT: Color = Color::from_rgb8(0x6b, 0x4b, 0x8a);

// --- MARK: ui_scale

fn header_harness(scale: f64, size: (u32, u32)) -> TestHarness<AreaContent> {
    // The caption alone, because the metric compares the same content at two scales and
    // a bar lays out as many controls as fit: with buttons, the small image and the
    // magnified one show different things and the comparison means nothing (§46).
    let header = NewWidget::new(ScaledHeader::caption(TINT, "ui")).erased();
    let content = AreaContent::new(vec![(RegionKind::Main, 0.0, header)]).with_ui_scale(0, scale);
    TestHarness::create_with_size(
        property_set(),
        NewWidget::new(content),
        PhysicalSize::new(size.0, size.1),
    )
}

/// §9, multiplier two: `ui_scale` goes through layout, and the result is drawn at the
/// new size rather than drawn once and stretched.
#[test]
fn ui_scale_magnifies_without_blurring() {
    let small = header_harness(1.0, BASE).render();
    let big = header_harness(f64::from(MAGNIFY), (BASE.0 * MAGNIFY, BASE.1 * MAGNIFY)).render();

    let gain = sharpness_gain(&small, &big).expect("the header has edges");
    println!("ui_scale x{MAGNIFY}: sharpness gain {gain:.2}x");
    assert!(
        gain > MIN_GAIN,
        "ui_scale gain was {gain:.2}x, wanted more than {MIN_GAIN}"
    );
    assert_redrawn_not_repeated(&small, &big);
}

// --- MARK: view

fn canvas_harness(zoom: f64, size: (u32, u32)) -> TestHarness<CanvasLayer> {
    let (canvas, _graph) = build_canvas(400);
    let mut harness = TestHarness::create_with_size(
        property_set(),
        NewWidget::new(canvas),
        PhysicalSize::new(size.0, size.1),
    );
    if zoom != 1.0 {
        // About the origin, so both viewports cover the same canvas rectangle: the
        // larger one simply has more pixels for it.
        harness.edit_root_widget(|mut canvas| CanvasLayer::zoom_around(&mut canvas, Point::ORIGIN, zoom));
    }
    harness
}

/// §9, multiplier three: `view` is a transform at composition time, and the scene is
/// curves rather than pixels, so magnifying it re-rasterises rather than resamples.
#[test]
fn canvas_zoom_magnifies_without_blurring() {
    let small = canvas_harness(1.0, BASE).render();
    let big = canvas_harness(f64::from(MAGNIFY), (BASE.0 * MAGNIFY, BASE.1 * MAGNIFY)).render();

    let gain = sharpness_gain(&small, &big).expect("the graph has edges");
    println!("view x{MAGNIFY}: sharpness gain {gain:.2}x");
    assert!(gain > MIN_GAIN, "zoom gain was {gain:.2}x, wanted more than {MIN_GAIN}");
    assert_redrawn_not_repeated(&small, &big);
}

// --- MARK: device_scale

/// Renders a harness's visual layer plan at a device scale factor.
///
/// Masonry's own `TestHarness::render` cannot do this: the plan is in logical
/// coordinates and applying the window's scale factor is the host's job (§4.2), which
/// the harness does not do. §23.4 wrote that host by hand here, in forty lines; it
/// now lives in `blazy-shell` and this calls it, so the test exercises the code the
/// window runs rather than a copy of it (§26.4).
fn render_at_device_scale(harness: &mut TestHarness<AreaContent>, scale: f64, size: (u32, u32)) -> RgbaImage {
    let (plan, _tree) = harness.redraw();
    let mut host = Host::any()
        .expect("some backend opens")
        .with_device_scale(scale)
        .with_background(Color::from_rgb8(0xff, 0xff, 0xff));

    let frame = host
        .render(&plan, Size::new(f64::from(size.0), f64::from(size.1)))
        .expect("the host renders");
    RgbaImage::from_vec(frame.image.width, frame.image.height, frame.image.data).expect("rgba image")
}

/// §9, multiplier one: the window's HiDPI factor is an `Affine` like any other, and
/// the same scene replayed under it comes out re-rasterised rather than stretched.
#[test]
fn device_scale_magnifies_without_blurring() {
    let mut harness = header_harness(1.0, BASE);
    let small = render_at_device_scale(&mut harness, 1.0, BASE);
    let big = render_at_device_scale(&mut harness, f64::from(MAGNIFY), BASE);

    let gain = sharpness_gain(&small, &big).expect("the header has edges");
    println!("device_scale x{MAGNIFY}: sharpness gain {gain:.2}x");
    assert!(
        gain > MIN_GAIN,
        "device scale gain was {gain:.2}x, wanted more than {MIN_GAIN}"
    );
    assert_redrawn_not_repeated(&small, &big);
}

/// The companion check to a sharpness gain.
///
/// A gain says the edges are as thin as they would be if drawn at this size, and
/// pixel doubling satisfies that while doing no work at all. A real redraw resamples
/// every edge, so it differs from block magnification.
fn assert_redrawn_not_repeated(small: &RgbaImage, big: &RgbaImage) {
    let blocky = block_magnified(small, big.width(), big.height());
    let differing = differing_fraction(big, &blocky);
    assert!(
        differing > 0.001,
        "the magnified image is a block magnification of the small one ({differing:.4} differ)"
    );
}

// --- MARK: appearance

/// The screen still looks like itself.
///
/// Snapshots are viable here for reasons that are not true of golden images in
/// general, and §23 lists them: the harness pins its font and disables system fonts,
/// the rasteriser is CPU-side, and the upstream commit is pinned. Verified
/// byte-identical across repeated runs and across the debug and release profiles;
/// a different CPU architecture is the untested case.
///
/// Regenerate with `MASONRY_TEST_BLESS=1 cargo make test`.
#[test]
fn screen_appearance() {
    let (screen, _graph) = build_screen(4, 200, None);
    let mut harness = TestHarness::create_with(
        property_set(),
        NewWidget::new(screen),
        TestHarnessParams::size_and_padding(PhysicalSize::new(240, 160), 0),
    );
    assert_render_snapshot!(harness, "screen_four_areas");
}

/// The shape the window actually builds: areas with the operator layer, a selected node
/// and the status line, at a size where both are drawn.
///
/// `screen_four_areas` above is the geometry picture, and it is taken of a screen
/// **without** operators, tiled small enough that a node fills an area. Three defects
/// walked past it (§44.9) because nothing it contains could show them: with no operator
/// layer there is no status line and no selection, and those are exactly the two things
/// that were painted outside their area and over the neighbour. A snapshot of a scene
/// the application does not build checks another product (§44.6); this one has both
/// objects and the boundary they crossed.
///
/// Regenerate with `MASONRY_TEST_BLESS=1 cargo make test`.
#[test]
fn screen_with_operators_appearance() {
    let (screen, _graph) = crate::ScreenSpec::new(2, 60).with_ops(true).build();
    let mut harness = TestHarness::create_with(
        property_set(),
        NewWidget::new(screen),
        // Larger than the default cap, because this picture is meant to have detail in
        // it: the status line and the outline of a selected node are the objects under
        // test, and they do not survive a thumbnail.
        TestHarnessParams::size_and_padding(PhysicalSize::new(520, 260), 0)
            .with_max_screenshot_size(32 * TestHarnessParams::KIBIBYTE),
    );
    let id = editor_id(&harness, 0);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(
            &mut editor,
            "node.select",
            &blazy::ops::keymap::Props::new().with_int("index", 0),
        );
    });
    assert_render_snapshot!(harness, "screen_with_operators");
}

/// The graph with its edges.
///
/// Worth a picture of its own: the existing screen snapshot tiles the window small
/// enough that a single node fills an area, so it would go on passing whether or not
/// links were drawn at all — which it did, when they were added.
#[test]
fn canvas_with_links_appearance() {
    let (canvas, _graph) = build_canvas(400);
    let mut harness = TestHarness::create_with(
        property_set(),
        NewWidget::new(canvas),
        TestHarnessParams::size_and_padding(PhysicalSize::new(260, 180), 0),
    );
    // Far enough out that several nodes and the curves between them are on screen.
    harness.edit_root_widget(|mut canvas| CanvasLayer::zoom_around(&mut canvas, Point::ORIGIN, 0.45));
    assert_render_snapshot!(harness, "canvas_with_links");
}

/// A header at two interface scales, as pictures rather than as a number.
#[test]
fn header_appearance_at_two_scales() {
    let mut plain = header_harness(1.0, BASE);
    assert_render_snapshot!(plain, "header_scale_1");

    let mut scaled = header_harness(2.0, (BASE.0, BASE.1 * 2));
    assert_render_snapshot!(scaled, "header_scale_2");
}

/// The property is what carries the scale, so it is worth one assertion that the
/// picture and the property agree.
#[test]
fn the_snapshot_scales_are_the_ones_that_were_set() {
    let harness = header_harness(2.0, BASE);
    let id = harness.root_widget().region_ids()[0];
    let seen = harness
        .get_widget_with_id(id)
        .downcast::<ScaledHeader>()
        .expect("region 0 is a header")
        .seen_scale();
    assert_eq!(seen, 2.0);
    assert_eq!(UiScale::default().0, 1.0);
}

// --- MARK: several views of one graph (§30)

/// A screen of `areas` areas over one shared graph.
fn screen_harness(areas: usize, nodes: usize) -> (TestHarness<Screen>, SharedGraph) {
    let (screen, graph) = build_screen(areas, nodes, None);
    let mut harness =
        TestHarness::create_with_size(property_set(), NewWidget::new(screen), PhysicalSize::new(1400, 900));
    let _ = harness.redraw();
    (harness, graph)
}

/// The canvas region of one area.
fn canvas_id(harness: &TestHarness<Screen>, area: usize) -> WidgetId {
    let area_id = harness.root_widget().area_ids()[area];
    *harness
        .get_widget_with_id(area_id)
        .downcast::<AreaContent>()
        .expect("every area holds a region stack")
        .region_ids()
        .last()
        .expect("an area has regions")
}

fn child_pos(harness: &mut TestHarness<Screen>, area: usize, index: usize) -> Point {
    let id = canvas_id(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut canvas = widget.downcast::<CanvasLayer>();
        CanvasLayer::child_pos(&mut canvas, index).expect("the node exists")
    })
}

fn live_nodes(harness: &mut TestHarness<Screen>, area: usize) -> Vec<(usize, WidgetId)> {
    let id = canvas_id(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut canvas = widget.downcast::<CanvasLayer>();
        CanvasLayer::live_children(&mut canvas)
    })
}

/// Where a canvas-space point of area `area` lands in the window.
fn to_window(harness: &TestHarness<Screen>, area: usize, canvas_pos: Point) -> Point {
    let canvas = harness
        .get_widget_with_id(canvas_id(harness, area))
        .downcast::<CanvasLayer>()
        .expect("the main region is a canvas");
    canvas.ctx().window_transform() * (canvas.view() * canvas_pos)
}

/// Dragging a node in one area moves it in every other view of the same graph.
///
/// Node geometry is state, and by §20.2 state belongs to the model: each canvas keeps
/// its own copy of it, so a drag that only moved the copy split the two views apart
/// permanently. The assertion on the model is half the test — a fix that synchronised
/// the views but left the model behind would lose the move the moment a node scrolls
/// out and back in.
#[test]
fn a_drag_in_one_area_moves_the_node_in_the_others() {
    let (mut harness, graph) = screen_harness(2, 200);

    let live = live_nodes(&mut harness, 0);
    assert!(!live.is_empty(), "area 0 shows something to drag");
    let index = live[live.len() / 2].0;
    let before = child_pos(&mut harness, 0, index);
    assert_eq!(child_pos(&mut harness, 1, index), before, "the views start together");

    // Grab the node by its header strip, which carries no controls of its own.
    let grab = to_window(&harness, 0, before + Vec2::new(80.0, 8.0));
    harness.mouse_move(grab);
    harness.mouse_button_press(Some(PointerButton::Primary));
    harness.mouse_move(grab + Vec2::new(30.0, 20.0));
    harness.mouse_button_release(Some(PointerButton::Primary));
    let _ = harness.redraw();

    let moved = child_pos(&mut harness, 0, index);
    assert_ne!(moved, before, "the drag has to have moved the node it grabbed");
    assert_eq!(
        child_pos(&mut harness, 1, index),
        moved,
        "the other area must follow the drag"
    );
    assert_eq!(
        graph.borrow().node(index).pos,
        moved,
        "and the model must have recorded it"
    );
}

/// Toggling a node's checkbox in one area reaches the same node in another.
///
/// Two things at once, and both were broken: the edit did not reach the other view,
/// and it did not reach the checkbox that was clicked either — Masonry's `Checkbox`
/// deliberately leaves its state to whoever owns the source of truth.
#[test]
fn a_control_edit_in_one_area_reaches_the_others() {
    let (mut harness, graph) = screen_harness(2, 200);

    let in_zero = live_nodes(&mut harness, 0);
    let in_one = live_nodes(&mut harness, 1);
    // A node both areas show, whose checkbox is not clipped by its area's viewport:
    // a node at the edge of an area has a widget and no reachable control, and picking
    // one would test the harness's patience rather than the propagation.
    let (index, own, checkbox) = in_zero
        .iter()
        .filter(|(i, _)| in_one.iter().any(|(j, _)| j == i))
        .find_map(|&(index, node)| {
            let checkbox = harness
                .get_widget_with_id(node)
                .downcast::<GraphNode>()
                .expect("a canvas child is a GraphNode")
                .checkbox_id()?;
            let widget = harness.get_widget_with_id(checkbox);
            let centre = widget.ctx().window_transform() * widget.ctx().border_box().center();
            let reachable = harness
                .root_widget()
                .as_dyn()
                .find_widget_under_pointer(centre)
                .map(|w| w.id())
                == Some(checkbox);
            reachable.then_some((index, node, checkbox))
        })
        .expect("some node is fully visible in both areas");
    let peer = in_one
        .iter()
        .find_map(|&(i, id)| (i == index).then_some(id))
        .expect("the shared node");

    let before = graph.borrow().node(index).checked;
    harness.mouse_click_on(checkbox, Some(PointerButton::Primary));
    let _ = harness.redraw();

    assert_eq!(
        graph.borrow().node(index).checked,
        !before,
        "the model records the edit"
    );
    let shows = |harness: &TestHarness<Screen>, id: WidgetId| {
        harness
            .get_widget_with_id(id)
            .downcast::<GraphNode>()
            .expect("a canvas child is a GraphNode")
            .checked()
    };
    assert_eq!(shows(&harness, own), !before, "the node that was clicked shows it");
    assert_eq!(shows(&harness, peer), !before, "and so does the other area's copy");
}

// --- MARK: operators inside an area (§38)

/// A screen whose areas carry the operator layer, over one shared graph.
fn ops_screen(areas: usize, nodes: usize) -> (TestHarness<Screen>, SharedGraph) {
    let (screen, graph) = crate::ScreenSpec::new(areas, nodes).with_ops(true).build();
    let mut harness =
        TestHarness::create_with_size(property_set(), NewWidget::new(screen), PhysicalSize::new(1400, 900));
    let _ = harness.redraw();
    (harness, graph)
}

/// A harness over a screen that was built elsewhere: the second window of a test.
fn harness_of(screen: Screen) -> TestHarness<Screen> {
    let mut harness =
        TestHarness::create_with_size(property_set(), NewWidget::new(screen), PhysicalSize::new(1400, 900));
    let _ = harness.redraw();
    harness
}

/// Brings every editor of a window up to date with the graph, and counts the changes.
///
/// What an application's driver does through `sync_window`; a harness does not hand out
/// its `RenderRoot`, so a test walks the areas itself.
fn sync(harness: &mut TestHarness<Screen>, graph: &SharedGraph) -> usize {
    let areas = harness.root_widget().area_ids().len();
    let mut applied = 0;
    for area in 0..areas {
        let id = editor_id(harness, area);
        applied += harness.edit_widget_with_id(id, |mut widget| {
            let mut editor = widget.downcast::<NodeEditor>();
            crate::sync_editor(&mut editor, graph)
        });
    }
    applied
}

/// The editor of one area.
fn editor_id(harness: &TestHarness<Screen>, area: usize) -> WidgetId {
    let area_id = harness.root_widget().area_ids()[area];
    *harness
        .get_widget_with_id(area_id)
        .downcast::<AreaContent>()
        .expect("every area holds a region stack")
        .region_ids()
        .last()
        .expect("an area has regions")
}

fn editor_canvas_pos(harness: &mut TestHarness<Screen>, area: usize, index: usize) -> Option<Point> {
    let id = editor_id(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::child_pos(&mut canvas, index))
    })
}

/// A grab driven through the operator layer in one area moves the node everywhere.
///
/// The §30 rule, through the path §38 built: the operator writes the model, the
/// driver moves its own canvas and names the other views. Two areas rather than one,
/// because with one view "the truth is in the model" and "the truth is in the widget"
/// are the same sentence.
#[test]
fn an_operator_move_in_one_area_reaches_the_others() {
    use blazy::ops::keymap::Props;

    let (mut harness, graph) = ops_screen(2, 200);
    let live = harness.edit_widget_with_id(editor_id(&harness, 0), |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::live_children(&mut canvas))
    });
    assert!(!live.is_empty(), "area 0 shows something");
    let index = live[live.len() / 2].0;
    let before = graph.borrow().node(index).pos;
    assert_eq!(editor_canvas_pos(&mut harness, 1, index), Some(before));

    let id = editor_id(&harness, 0);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(
            &mut editor,
            "node.select",
            &Props::new().with_int("index", index as i64),
        );
        NodeEditor::exec(
            &mut editor,
            "node.move",
            &Props::new().with_float("dx", 40.0).with_float("dy", -25.0),
        );
    });
    let _ = harness.redraw();

    let moved = graph.borrow().node(index).pos;
    assert_ne!(moved, before, "the operator moved the node in the model");
    assert_eq!(
        editor_canvas_pos(&mut harness, 0, index),
        Some(moved),
        "the area that ran the operator follows it"
    );
    assert_eq!(
        editor_canvas_pos(&mut harness, 1, index),
        Some(moved),
        "and so does the other view of the same graph"
    );
}

/// A structural edit in one area reaches the other view of the same graph (§43).
///
/// The §30 rule again, for the shape of the graph rather than for a position — and it
/// needs two views for the same reason: with one, "the truth is in the model" and "the
/// truth is in the view" are indistinguishable. A node added in one area has to exist in
/// both, under the same name, and a node deleted has to be gone from both.
#[test]
fn a_structural_edit_in_one_area_reaches_the_others() {
    use blazy::ops::keymap::Props;

    let (mut harness, graph) = ops_screen(2, 200);
    let names_before = graph.borrow().names();
    let id = editor_id(&harness, 0);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(
            &mut editor,
            "node.add",
            &Props::new().with_float("x", 40.0).with_float("y", 40.0),
        );
    });
    let _ = harness.redraw();

    let added = names_before;
    let place = Point::new(40.0, 40.0);
    assert_eq!(graph.borrow().try_node(added).map(|node| node.pos), Some(place));
    assert_eq!(
        editor_canvas_pos(&mut harness, 0, added),
        Some(place),
        "the area that added it has it"
    );
    assert_eq!(
        editor_canvas_pos(&mut harness, 1, added),
        Some(place),
        "and so does the other view"
    );

    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(
            &mut editor,
            "node.delete",
            &Props::new().with_int("index", added as i64),
        );
    });
    let _ = harness.redraw();

    assert_eq!(graph.borrow().try_node(added), None);
    for area in [0, 1] {
        assert_eq!(
            editor_canvas_pos(&mut harness, area, added),
            None,
            "area {area} still shows a node the graph does not have"
        );
    }
}

/// §30 stops at the window boundary, and the pull is what carries it across.
///
/// Two screens over one graph, each in its own `RenderRoot` — which is what two windows
/// are. The push fan-out reaches the canvases of one of them and is silently dropped for
/// the other, because `mutate_later` names a widget in one arena. Measured rather than
/// argued: the model moves, the second window shows the old place, and one `sync_window`
/// puts it right.
#[test]
fn a_change_in_one_window_reaches_the_other_through_the_model() {
    use blazy::ops::keymap::Props;

    let (screen_a, graph) = crate::ScreenSpec::new(2, 200).with_ops(true).build();
    let screen_b = crate::ScreenSpec::new(2, 200).with_ops(true).over(&graph);
    let mut a = harness_of(screen_a);
    let mut b = harness_of(screen_b);

    let before = graph.borrow().node(0).pos;
    let id = editor_id(&a, 0);
    a.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 0));
        NodeEditor::exec(&mut editor, "node.move", &Props::new().with_float("dx", 40.0));
    });
    let _ = a.redraw();
    let _ = b.redraw();

    let moved = graph.borrow().node(0).pos;
    assert_ne!(moved, before, "the operator wrote the model");
    assert_eq!(
        editor_canvas_pos(&mut a, 0, 0),
        Some(moved),
        "the window that ran it follows"
    );
    assert_eq!(
        editor_canvas_pos(&mut b, 0, 0),
        Some(before),
        "and the other window does not: `mutate_later` cannot cross a `RenderRoot`"
    );

    let applied = sync(&mut b, &graph);
    assert!(applied > 0, "the second window had something to catch up on");
    let _ = b.redraw();
    assert_eq!(
        editor_canvas_pos(&mut b, 0, 0),
        Some(moved),
        "and after the pull it shows what the model says"
    );
    assert_eq!(sync(&mut b, &graph), 0, "a window that is up to date pulls nothing");
}

/// Every area's layer stays inside the area, and the areas tile the window.
///
/// The promise §36 asks a caller for — a cached layer owns its rectangle — checked at the
/// one place it can be: the rectangle a layer claims is the one the host works out by
/// walking its scene, so anything an area paints outside its own box moves that rectangle
/// onto its neighbour. Two things did: the outline of a selected node and the status
/// line, neither of which is a child and neither of which anything clipped. The cost was
/// not cosmetic — eight layers claiming 1.8 windows' worth of pixels put the cache over
/// its ceiling, and the eviction blanked whichever area was copied that frame (§44.9).
///
/// Containment rather than only disjointness, because disjointness is the *consequence*:
/// a screen of one area has nothing to overlap and would have passed while spilling over
/// everything around it (§45).
#[test]
fn every_areas_layer_stays_inside_its_area() {
    use blazy::shell::layers::scene_bounds;

    let (screen, _graph) = crate::ScreenSpec::new(8, 200)
        .with_ops(true)
        .with_isolated_layers(true)
        .build();
    let mut harness = harness_of(screen);
    // A selection, because the outline of a selected node is one of the two things that
    // used to spill, and an area with nothing selected cannot show it.
    let id = editor_id(&harness, 0);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(
            &mut editor,
            "node.select",
            &blazy::ops::keymap::Props::new().with_int("index", 0),
        );
    });

    // What the host does on every frame, and without which there are no layers to
    // check: a layer lives exactly one paint, so an area that is clean is not painted
    // and stops being one (§26.1). This is `ShellDriver::layers` by hand.
    let areas: Vec<WidgetId> = harness.root_widget().area_ids();
    let mut boxes: Vec<(WidgetId, Rect)> = Vec::new();
    for area in &areas {
        let rect = harness.edit_widget_with_id(*area, |mut widget| {
            widget.ctx.request_paint_only();
            widget
                .ctx
                .window_transform()
                .transform_rect_bbox(widget.ctx.content_box())
        });
        boxes.push((*area, rect));
    }
    let (plan, _) = harness.redraw();

    let frame = PhysicalSize::new(1400, 900);
    let rects: Vec<_> = plan
        .layers
        .iter()
        .filter_map(|layer| match &layer.kind {
            blazy::masonry::app::VisualLayerKind::Scene(scene) => boxes
                .iter()
                .find(|(id, _)| *id == layer.widget_id)
                .and_then(|(id, area)| scene_bounds(scene, layer.transform, frame).map(|rect| (*id, *area, rect))),
            _ => None,
        })
        .collect();
    assert_eq!(rects.len(), areas.len(), "every area is a layer of its own");

    for (id, area, drawn) in &rects {
        // A whole pixel of slack on each side, because the layer's rectangle is rounded
        // outwards to whole pixels and the area's is not.
        let allowed = area.inflate(1.0, 1.0);
        let drawn = Rect::new(
            f64::from(drawn.x),
            f64::from(drawn.y),
            f64::from(drawn.x + drawn.width),
            f64::from(drawn.y + drawn.height),
        );
        assert_eq!(
            drawn.union(allowed),
            allowed,
            "{id:?} painted {drawn:?}, outside its area {area:?}"
        );
    }

    // And nothing claims another's pixels — the consequence, kept because it is what the
    // host counts at runtime (`overlaps`) and the two must agree.
    for (i, (id, _, a)) in rects.iter().enumerate() {
        for (other, _, b) in &rects[i + 1..] {
            let overlap =
                (a.x < b.x + b.width) && (b.x < a.x + a.width) && (a.y < b.y + b.height) && (b.y < a.y + a.height);
            assert!(!overlap, "layers of {id:?} and {other:?} claim the same pixels");
        }
    }
}

/// A slider or a checkbox inside a node reaches the other window.
///
/// Node geometry was not the only thing a view copies out of the model when it builds a
/// node: `value`, `checked` and the tint are copied too, and a node already on screen
/// reads none of them again. Inside one window the edit travels as a push
/// (`GraphNode::broadcast`), which is dropped across a `RenderRoot` like every other
/// (§44.3) — and the model recorded nothing for the other window to collect, so the two
/// windows disagreed about a checkbox for good (§44.9).
#[test]
fn an_edit_inside_a_node_reaches_the_other_window() {
    let (screen_a, graph) = crate::ScreenSpec::new(1, 200).with_ops(true).build();
    let screen_b = crate::ScreenSpec::new(1, 200).with_ops(true).over(&graph);
    let mut a = harness_of(screen_a);
    let mut b = harness_of(screen_b);

    let before = node_state(&mut b, 0, 0).expect("node 0 is on screen in both windows");
    let (value, checked) = (before.0 + 0.25, !before.1);

    // What a node widget does when its slider moves: write the model, and record the
    // change for the views that did not see it. The record is the node's rather than the
    // model's because the node knows which view it is in — and a change owed to the view
    // being dragged would rebuild the slider under the pointer, once per frame.
    graph.borrow_mut().set_value(0, value);
    graph.borrow().views().note(Change::Contents { index: 0 });
    graph.borrow_mut().set_checked(0, checked);
    graph.borrow().views().note(Change::Contents { index: 0 });
    let _ = a.redraw();

    assert_eq!(
        node_state(&mut b, 0, 0),
        Some(before),
        "before the pull the other window still shows what it read when it built the node"
    );
    assert!(sync(&mut b, &graph) >= 2, "two edits are two changes to collect");
    let _ = b.redraw();
    assert_eq!(node_state(&mut b, 0, 0), Some((value, checked)));
}

/// What a node widget of one area is showing: the slider value and the checkbox.
fn node_state(harness: &mut TestHarness<Screen>, area: usize, index: usize) -> Option<(f64, bool)> {
    let id = editor_id(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            let mut state = None;
            CanvasLayer::update_child(&mut canvas, index, |mut node| {
                let node = node.downcast::<GraphNode>();
                state = Some((node.widget.value(), node.widget.checked()));
            });
            state
        })
    })
}

/// Detach: the area is rebuilt in another window and loses nothing (decision 1).
///
/// The widget cannot move — `RenderRoot` owns its arena — so what is carried is the
/// session, and the test is that everything a user would miss comes through it: the view,
/// the selection and the depth of the history. The areas that stayed keep their widgets,
/// which is §41.2's rule and the `builds` counter that holds it.
#[test]
fn a_detached_area_arrives_with_its_view_its_selection_and_its_history() {
    use blazy::masonry::kurbo::Vec2;
    use blazy::ops::keymap::Props;

    let (mut screen, graph) = ops_screen(4, 200);

    // Give area 1 something to lose.
    let id = editor_id(&screen, 1);
    screen.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 3));
        NodeEditor::exec(&mut editor, "node.move", &Props::new().with_float("dx", 15.0));
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::pan(&mut canvas, Vec2::new(-30.0, -12.0));
        });
    });
    let _ = screen.redraw();
    let (selection, depth, view) = {
        let editor = screen.get_widget_with_id(id).downcast::<NodeEditor>().expect("editor");
        (editor.selection(), editor.history_depth(), editor.stats().zoom)
    };
    assert!(!selection.is_empty() && depth > 0);

    let builds_before = screen.root_widget().stats().counters.builds;
    let taken = screen.edit_root_widget(|mut screen| crate::Screen::detach(&mut screen, 1));
    let _ = screen.redraw();
    let session = taken.expect("an area of four detaches");

    assert_eq!(screen.root_widget().stats().areas, 3, "the area left this screen");
    assert_eq!(
        screen.root_widget().stats().counters.builds,
        builds_before,
        "and the areas that stayed were not rebuilt (§41.2)"
    );

    // The other window: a new screen of one area over the session that was carried.
    let detached = crate::ScreenSpec::new(1, 200)
        .with_ops(true)
        .over_with(&graph, Some(session));
    let window = harness_of(detached);
    let arrived = editor_id(&window, 0);
    let editor = window
        .get_widget_with_id(arrived)
        .downcast::<NodeEditor>()
        .expect("editor");

    assert_eq!(editor.selection(), selection, "the selection arrived");
    assert_eq!(editor.history_depth(), depth, "and the history");
    assert_eq!(editor.stats().zoom, view, "and the view");
}

/// Selecting in one area is that area's business.
///
/// The other half of §30: the *graph* is shared and the *selection* is not, which is
/// what makes "one gesture repaints one area" possible at all (§36, §38.5). Blender
/// shares a selection between editors because it belongs to the scene; here it belongs
/// to the view, and the point of the test is that the two are told apart deliberately
/// rather than by accident.
#[test]
fn a_selection_stays_in_the_area_that_made_it() {
    use blazy::ops::keymap::Props;

    let (mut harness, _graph) = ops_screen(2, 200);
    let id = editor_id(&harness, 0);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::exec(&mut editor, "node.select", &Props::new().with_int("index", 3));
    });
    let _ = harness.redraw();

    let selected = |harness: &TestHarness<Screen>, area: usize| {
        harness
            .get_widget_with_id(editor_id(harness, area))
            .downcast::<NodeEditor>()
            .expect("an area holds an editor")
            .selection()
    };
    assert_eq!(selected(&harness, 0).len(), 1, "the area that selected has a selection");
    assert!(selected(&harness, 1).is_empty(), "the other area does not");
}

/// The pre-tree seat belongs to the layer root, and an editor inside an area is not
/// one (§38.1).
///
/// This is the reason the seat cannot be the whole answer for a real application: a
/// window of eight areas has exactly one widget that `Layer::capture_pointer_event`
/// reaches, and it is the screen, not any of the editors in it.
#[test]
fn an_editor_inside_an_area_has_no_pre_tree_seat() {
    let (mut harness, _graph) = ops_screen(2, 200);
    harness.mouse_move(Point::new(400.0, 400.0));
    harness.mouse_move(Point::new(420.0, 410.0));
    let seen = harness
        .get_widget_with_id(editor_id(&harness, 0))
        .downcast::<NodeEditor>()
        .expect("an area holds an editor")
        .op_counters()
        .seen_first;
    assert_eq!(seen, 0, "no layer hook reaches a widget nested inside an area");
}

// --- MARK: what a real frame asks the rasteriser for (§34)

/// A real frame is nowhere near either of the rasteriser's ceilings, and the guard in
/// front of it costs nothing to say so.
///
/// The point of measuring this here rather than in `blazy-shell`: the crate can only
/// build the scenes it invents, and what matters is what an *application* nests. Eight
/// areas over one graph, at a HiDPI scale, come out at one clip deep and no groups at
/// all — so `over_budget` answers from the command stream and never walks the geometry
/// (§34.3). An application that wraps widgets in opacity groups is the one that has to
/// watch the depth, and §34.2 says at what number.
#[test]
fn a_real_frame_is_far_from_both_ceilings() {
    use blazy::masonry::imaging::record::Command;
    use blazy::shell::Composition;

    let (mut harness, _graph) = screen_harness(8, 5000);
    let (plan, _tree) = harness.redraw();
    let composed = Composition::new(&plan, 2.0);
    let frame = PhysicalSize::new(2800, 1800);

    let groups = composed
        .scene
        .commands()
        .iter()
        .filter(|command| matches!(command, Command::PushGroup(_)))
        .count();
    let depth = blazy::shell::tiles::nesting_depth(&composed.scene);
    let demand = blazy::shell::tiles::demand(&composed.scene, frame);
    println!(
        "eight areas over 5000 nodes: {} commands, {groups} groups, {depth} deep, \
         {} tiles of {}, {} words of {}",
        composed.scene.commands().len(),
        demand.tiles,
        blazy::shell::TILE_BUDGET,
        demand.blend_words,
        blazy::shell::BLEND_BUDGET,
    );

    // One clip per area and nothing nested inside it: four levels are free, so the
    // frame asks for no blend scratch at all.
    assert!(depth <= 4, "a real frame nests {depth} deep");
    assert_eq!(demand.blend_words, 0);
    // And an order of magnitude of headroom in tiles, which is the §33.4 claim
    // measured on the real scene rather than on diagonals.
    assert!(
        demand.tiles * 10 < blazy::shell::TILE_BUDGET,
        "a real frame wants {} of {} tiles",
        demand.tiles,
        blazy::shell::TILE_BUDGET
    );
}

// --- MARK: layers per area (§36)

/// Asks every area to repaint, the way the shell does before a frame (§36.1).
///
/// Not a method on `AreaScreen`: keeping a layer alive is the host's job — the window
/// loop does it for the ids `ShellDriver::layers` returns — and a library method that
/// exists only so a harness can imitate the host is a second way to say one thing.
fn keep_layers(harness: &mut TestHarness<Screen>) {
    for id in harness.root_widget().area_ids() {
        harness.edit_widget_with_id(id, |mut widget| widget.ctx.request_paint_only());
    }
}
/// What a screen of areas looks like to a host that wants to cache them.
///
/// Two numbers the design rests on, measured rather than assumed: how many layers the
/// plan carries when every area declares one, and what happens on the frame after —
/// the frame a cache exists for. Asking the areas to repaint is what keeps the second
/// number equal to the first (§26.1).
#[test]
fn every_area_is_its_own_layer_while_it_is_asked_to_repaint() {
    let (screen, _graph) = crate::ScreenSpec::new(8, 400)
        .with_budget(Some(64))
        .with_isolated_layers(true)
        .build();
    let mut harness =
        TestHarness::create_with_size(property_set(), NewWidget::new(screen), PhysicalSize::new(1400, 900));
    let (plan, _) = harness.redraw();
    let first = plan.layers.len();

    // A frame in which nothing at all happened.
    let (plan, _) = harness.redraw();
    let idle = plan.layers.len();

    // The same frame, with the areas asked to repaint first.
    keep_layers(&mut harness);
    let (plan, _) = harness.redraw();
    let kept = plan.layers.len();

    println!("layers: first frame {first}, idle frame {idle}, idle frame kept {kept}");
    // The harness has already painted once by the time it hands the tree over, so the
    // frame this test asks for is already a clean one: one layer, every area's content
    // merged back into it.
    assert_eq!(first, 1, "a clean frame carries no area layers");
    assert_eq!(idle, 1, "and neither does the next one");
    assert_eq!(kept, 9, "asking the areas to repaint gives eight layers plus the root");
}

/// The picture a kept layer produces is the picture drawing it would have produced.
///
/// The claim the whole mechanism stands on, checked pixel for pixel rather than by
/// reasoning about copies: the same screen is drawn twice, once from scratch and once
/// with every area's pixels kept from the frame before, and the two frames have to be
/// identical. Skipped where there is no graphics device, like every GPU claim here
/// (§27.5).
#[test]
fn a_kept_layer_is_the_same_picture() {
    let size = PhysicalSize::new(1400, 900);
    // With a background, because that is what a window presents (§26.4) and because a
    // transparent pixel's colour channels are not part of the picture: without it the
    // two paths differ in the RGB of fully transparent pixels, which is nothing.
    let panel = Color::from_rgb8(0x14, 0x14, 0x18);
    let Ok(gpu) = blazy::shell::gpu::GpuFrames::offscreen(size) else {
        eprintln!("no graphics device: skipping");
        return;
    };
    // One device for both paths: the cache is turned on and off by what it is told to
    // keep, so a second device would only add a driver initialisation to the test.
    let mut gpu = gpu.with_background(panel);

    let (screen, _graph) = crate::ScreenSpec::new(8, 400)
        .with_budget(Some(64))
        .with_isolated_layers(true)
        .build();
    let mut harness = TestHarness::create_with_size(property_set(), NewWidget::new(screen), size);
    let ids = harness.root_widget().area_ids();

    let logical = Size::new(f64::from(size.width), f64::from(size.height));
    keep_layers(&mut harness);
    let (plan, _) = harness.redraw();

    // The *same* plan through both paths, and twice through the caching one: the
    // second time is the frame the cache answers from. One plan rather than one frame
    // each, because two consecutive frames of a live tree are not obliged to be the
    // same picture, and this test is about the cache and not about the tree.
    //
    // Cached first: turning the cache off drops what it kept, so the other order would
    // have to fill it twice.
    gpu.cache_layers(ids);
    gpu.draw(&plan, logical, 1.0).expect("the frame draws");
    gpu.wait();
    gpu.draw(&plan, logical, 1.0).expect("the frame draws");
    gpu.wait();
    let kept = gpu.read_pixels();
    let counters = gpu.layer_counters();

    gpu.cache_layers(Vec::new());
    gpu.draw(&plan, logical, 1.0).expect("the frame draws");
    gpu.wait();
    let reference = gpu.read_pixels();
    // The rasteriser is deterministic here — the same plan twice is the same bytes —
    // so any difference below belongs to the cache and not to the GPU.
    gpu.draw(&plan, logical, 1.0).expect("the frame draws");
    gpu.wait();
    let twice = gpu.read_pixels();
    assert_eq!(
        reference.iter().zip(twice.iter()).filter(|(a, b)| a != b).count(),
        0,
        "the rasteriser is not deterministic, so this test cannot say anything"
    );

    println!(
        "layer cache: offered {}, reused {}, drawn {}, {} KiB",
        counters.offered,
        counters.reused,
        counters.drawn,
        counters.bytes / 1024
    );
    assert_eq!(counters.reused, 8, "the second frame should have kept all eight areas");

    let differing = reference.iter().zip(kept.iter()).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "a kept frame differs from a drawn one in {differing} bytes"
    );
}

/// And the other direction of the same switch (§28.4): a layer that *did* change is
/// drawn again.
///
/// Without this the picture test above passes on a cache that never invalidates
/// anything — checked by breaking it that way, which is how this test came to exist.
#[test]
fn a_changed_layer_is_drawn_again() {
    let size = PhysicalSize::new(1400, 900);
    let panel = Color::from_rgb8(0x14, 0x14, 0x18);
    let Ok(gpu) = blazy::shell::gpu::GpuFrames::offscreen(size) else {
        eprintln!("no graphics device: skipping");
        return;
    };
    // One device, as in the test above: what turns the cache on and off is the list of
    // layers it is told to keep.
    let mut gpu = gpu.with_background(panel);

    let (screen, _graph) = crate::ScreenSpec::new(8, 400)
        .with_budget(Some(64))
        .with_isolated_layers(true)
        .build();
    let mut harness = TestHarness::create_with_size(property_set(), NewWidget::new(screen), size);
    let ids = harness.root_widget().area_ids();
    gpu.cache_layers(ids.clone());
    let logical = Size::new(f64::from(size.width), f64::from(size.height));

    // A frame to fill the cache.
    keep_layers(&mut harness);
    let (plan, _) = harness.redraw();
    gpu.draw(&plan, logical, 1.0).expect("the frame draws");
    gpu.wait();

    // Now move one area's canvas, so exactly one layer is different.
    let canvas = canvas_id(&harness, 0);
    harness.edit_widget_with_id(canvas, |mut widget| {
        let mut canvas = widget.downcast::<CanvasLayer>();
        CanvasLayer::pan(&mut canvas, Vec2::new(-40.0, -25.0));
    });
    keep_layers(&mut harness);
    let (plan, _) = harness.redraw();

    // The cached frame first, while the cache still holds the frame before the pan;
    // turning it off afterwards is what gives the reference.
    gpu.draw(&plan, logical, 1.0).expect("the frame draws");
    gpu.wait();
    let kept = gpu.read_pixels();
    let counters = gpu.layer_counters();

    gpu.cache_layers(Vec::new());
    gpu.draw(&plan, logical, 1.0).expect("the frame draws");
    gpu.wait();
    let reference = gpu.read_pixels();
    // The rasteriser is deterministic here — the same plan twice is the same bytes —
    // so any difference below belongs to the cache and not to the GPU.
    gpu.draw(&plan, logical, 1.0).expect("the frame draws");
    gpu.wait();
    let twice = gpu.read_pixels();
    assert_eq!(
        reference.iter().zip(twice.iter()).filter(|(a, b)| a != b).count(),
        0,
        "the rasteriser is not deterministic, so this test cannot say anything"
    );

    println!(
        "after a pan in one area: reused {}, drawn {}",
        counters.reused, counters.drawn
    );
    let mut worst = 0_u8;
    let mut differing = 0;
    for (a, b) in reference.iter().zip(kept.iter()) {
        if a != b {
            differing += 1;
            worst = worst.max(a.abs_diff(*b));
        }
    }
    println!("a redrawn area next to kept ones: {differing} bytes differ, worst by {worst}");

    // The moved area is really redrawn — a cache that never invalidated would differ by
    // hundreds of thousands of bytes here, which is how this test was checked.
    assert!(
        differing < 8,
        "the moved area was not redrawn: {differing} bytes differ"
    );
    // What is left is the seam: a pixel on the boundary between two areas is drawn by
    // both of them in a single-pass frame and copied from one of them here, so one
    // channel can land a level apart. Measured at one byte of five million on this
    // screen, and pinned rather than waved away.
    assert!(worst <= 1, "a kept frame differs by {worst} levels, not by rounding");
}
