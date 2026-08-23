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
use blazy_areas::{AreaContent, RegionKind, UiScale};
use blazy_canvas::CanvasLayer;
use blazy_shell::Host;
use image::RgbaImage;
use masonry::core::NewWidget;
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Point, Size};
use masonry::peniko::Color;
use masonry::testing::{TestHarness, TestHarnessParams, assert_render_snapshot};
use masonry::theme::default_property_set;
use node_canvas::build_canvas;

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
    let (screen, _graph) = build_screen(4, 200);
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
