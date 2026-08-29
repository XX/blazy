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
use blazy_areas::{AreaContent, AreaScreen, RegionKind, UiScale};
use blazy_canvas::CanvasLayer;
use blazy_shell::Host;
use image::RgbaImage;
use masonry::core::{NewWidget, WidgetId};
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Point, Size, Vec2};
use masonry::peniko::Color;
use masonry::testing::{TestHarness, TestHarnessParams, assert_render_snapshot};
use masonry::theme::default_property_set;
use masonry::ui_events::pointer::PointerButton;
use node_canvas::build_canvas;
use node_canvas::model::SharedGraph;
use node_canvas::node::GraphNode;

use crate::build_screen;
use crate::header::ScaledHeader;

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
    let header = NewWidget::new(ScaledHeader::new(TINT)).erased();
    let content = AreaContent::new(vec![(RegionKind::Main, 0.0, header)]).with_ui_scale(0, scale);
    TestHarness::create_with_size(
        default_property_set(),
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
        default_property_set(),
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
        default_property_set(),
        NewWidget::new(screen),
        TestHarnessParams::size_and_padding(PhysicalSize::new(240, 160), 0),
    );
    assert_render_snapshot!(harness, "screen_four_areas");
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
        default_property_set(),
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
fn screen_harness(areas: usize, nodes: usize) -> (TestHarness<AreaScreen>, SharedGraph) {
    let (screen, graph) = build_screen(areas, nodes, None);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(screen),
        PhysicalSize::new(1400, 900),
    );
    let _ = harness.redraw();
    (harness, graph)
}

/// The canvas region of one area.
fn canvas_id(harness: &TestHarness<AreaScreen>, area: usize) -> WidgetId {
    let area_id = harness.root_widget().area_ids()[area];
    *harness
        .get_widget_with_id(area_id)
        .downcast::<AreaContent>()
        .expect("every area holds a region stack")
        .region_ids()
        .last()
        .expect("an area has regions")
}

fn child_pos(harness: &mut TestHarness<AreaScreen>, area: usize, index: usize) -> Point {
    let id = canvas_id(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut canvas = widget.downcast::<CanvasLayer>();
        CanvasLayer::child_pos(&mut canvas, index).expect("the node exists")
    })
}

fn live_nodes(harness: &mut TestHarness<AreaScreen>, area: usize) -> Vec<(usize, WidgetId)> {
    let id = canvas_id(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut canvas = widget.downcast::<CanvasLayer>();
        CanvasLayer::live_children(&mut canvas)
    })
}

/// Where a canvas-space point of area `area` lands in the window.
fn to_window(harness: &TestHarness<AreaScreen>, area: usize, canvas_pos: Point) -> Point {
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
    let shows = |harness: &TestHarness<AreaScreen>, id: WidgetId| {
        harness
            .get_widget_with_id(id)
            .downcast::<GraphNode>()
            .expect("a canvas child is a GraphNode")
            .checked()
    };
    assert_eq!(shows(&harness, own), !before, "the node that was clicked shows it");
    assert_eq!(shows(&harness, peer), !before, "and so does the other area's copy");
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
    use blazy_shell::Composition;
    use masonry::imaging::record::Command;

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
    let depth = blazy_shell::tiles::nesting_depth(&composed.scene);
    let demand = blazy_shell::tiles::demand(&composed.scene, frame);
    println!(
        "eight areas over 5000 nodes: {} commands, {groups} groups, {depth} deep, \
         {} tiles of {}, {} words of {}",
        composed.scene.commands().len(),
        demand.tiles,
        blazy_shell::TILE_BUDGET,
        demand.blend_words,
        blazy_shell::BLEND_BUDGET,
    );

    // One clip per area and nothing nested inside it: four levels are free, so the
    // frame asks for no blend scratch at all.
    assert!(depth <= 4, "a real frame nests {depth} deep");
    assert_eq!(demand.blend_words, 0);
    // And an order of magnitude of headroom in tiles, which is the §33.4 claim
    // measured on the real scene rather than on diagonals.
    assert!(
        demand.tiles * 10 < blazy_shell::TILE_BUDGET,
        "a real frame wants {} of {} tiles",
        demand.tiles,
        blazy_shell::TILE_BUDGET
    );
}

// --- MARK: layers per area (§36)

/// Asks every area to repaint, the way the shell does before a frame (§36.1).
///
/// Not a method on `AreaScreen`: keeping a layer alive is the host's job — the window
/// loop does it for the ids `ShellDriver::layers` returns — and a library method that
/// exists only so a harness can imitate the host is a second way to say one thing.
fn keep_layers(harness: &mut TestHarness<AreaScreen>) {
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
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(screen),
        PhysicalSize::new(1400, 900),
    );
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
    let Ok(gpu) = blazy_shell::gpu::GpuFrames::offscreen(size) else {
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
    let mut harness = TestHarness::create_with_size(default_property_set(), NewWidget::new(screen), size);
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
    let Ok(gpu) = blazy_shell::gpu::GpuFrames::offscreen(size) else {
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
    let mut harness = TestHarness::create_with_size(default_property_set(), NewWidget::new(screen), size);
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
