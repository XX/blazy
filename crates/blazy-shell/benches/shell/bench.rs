//! Headless measurements for the host.
//!
//! No window is opened: everything here is a plan going through [`Host`] and the
//! counters that come back. What a window adds on top — a surface, a blit, a platform
//! — is not something a benchmark on a CI runner can say anything honest about, and
//! it is not where the architectural claims live.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bench_utils::criteria::{Criterion, Kind, Outcome, ScenarioRecord};
use blazy_shell::{COMPILED, ExternalContent, Host, HostCounters};
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PropertiesRef, RegisterCtx, Widget,
};
use masonry::dpi::PhysicalSize;
use masonry::imaging::{GroupRef, Painter};
use masonry::kurbo::{Axis, BezPath, CubicBez, Point, Rect, Size, Stroke};
use masonry::layout::{LenReq, Length};
use masonry::peniko::Color;
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;

/// Window size every scenario uses, in logical pixels.
const SIZE: (u32, u32) = (1100, 750);
/// Window sizes the presentation table walks, in logical pixels.
const PRESENT_SIZES: [(u32, u32); 3] = [(800, 600), (1400, 900), (1920, 1200)];
/// Device scales the presentation table walks.
const PRESENT_SCALES: [f64; 2] = [1.0, 2.0];
/// Scale factors the device-scale sweep walks.
const SCALES: [f64; 4] = [1.0, 1.5, 2.0, 4.0];

pub struct Options {
    pub quick: bool,
    pub frames: usize,
}

impl Options {
    fn frames(&self) -> usize {
        if self.quick { self.frames.min(40) } else { self.frames }
    }
}

/// A widget that leaves part of the window bare, like a real screen does.
///
/// `AreaScreen` paints its splitter bars and nothing else; whatever an area does not
/// paint stays transparent. A frame is presented as opaque pixels, so the host has to
/// composite over a base colour — and this is what makes the criterion measure
/// something rather than a tree that happened to cover everything.
struct Sparse;

impl Widget for Sparse {
    type Action = NoAction;

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(50.0),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {}

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        let box_rect = ctx.content_box();
        painter
            .fill(
                masonry::kurbo::Circle::new(box_rect.center(), box_rect.height() / 3.0),
                Color::from_rgb8(0x80, 0x60, 0xa0),
            )
            .draw();
    }

    fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

/// A widget that paints a few dozen shapes and counts its own layout passes.
///
/// Something for the rasteriser to do, and the counter the device-scale criterion is
/// decided on: if a change of scale ever reaches layout, this is what says so.
struct Panel {
    layouts: Rc<Cell<u64>>,
    shapes: usize,
}

impl Widget for Panel {
    type Action = NoAction;

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(200.0),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {
        self.layouts.set(self.layouts.get() + 1);
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        let box_rect = ctx.content_box();
        painter.fill(box_rect, Color::from_rgb8(0x1c, 0x1c, 0x20)).draw();
        for i in 0..self.shapes {
            let x = box_rect.x0 + (i % 12) as f64 * 88.0 + 8.0;
            let y = box_rect.y0 + (i / 12) as f64 * 64.0 + 8.0;
            let tint = Color::from_rgb8(0x40 + (i * 7 % 0x80) as u8, 0x50, 0x90);
            painter.fill(Rect::new(x, y, x + 72.0, y + 48.0), tint).draw();
        }
    }

    fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

/// A far-field scene: many curves, stroked in a chosen number of commands.
///
/// The scene §31 is about, reduced to what a rasteriser can see. The geometry does
/// not depend on `groups` at all — every row of the raster table draws the same ink —
/// which is what makes "how many commands" and "how many pixels" two axes rather than
/// one. Built here rather than imported from the canvas example for the reason the
/// crate layout gives: this measurement needs a rasteriser and a device, and from a
/// graph it needs nothing but a command count and a coverage.
struct FarField {
    /// Curves drawn.
    count: usize,
    /// Stroke commands the same curves are split into.
    ///
    /// 1 is the batch §31 landed, `count` is the command-per-link scene it replaced,
    /// and everything between is there to say whether the rasteriser cares.
    groups: usize,
    /// Stroke width in logical pixels: the coverage axis, at a fixed command count.
    width: f64,
    /// What the curves are stroked with.
    ///
    /// Two of these alternate frame by frame in the raster table, so that a frame the
    /// GPU never drew cannot pass for the frame before it (§32.4).
    tint: Color,
}

/// One curve of the far field, in the widget's own coordinates.
///
/// Deterministic in `i` — the same hash gives the same curve in every row — and
/// shaped like a link: a cubic with horizontal handles, which is what the canvas
/// draws (`blazy_canvas::links::link_curve`).
fn far_curve(i: usize, area: Rect) -> CubicBez {
    let unit = |salt: u64| {
        let mut hash = (i as u64)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(salt.wrapping_mul(0xbf58_476d_1ce4_e5b9));
        hash ^= hash >> 31;
        hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
        hash ^= hash >> 29;
        (hash >> 11) as f64 / (1_u64 << 53) as f64
    };

    let start = Point::new(area.x0 + unit(1) * area.width(), area.y0 + unit(2) * area.height());
    // Short, like a link at an overview zoom: a canvas grid of 220 units seen at 0.04
    // puts neighbouring nodes about nine pixels apart, and a viewport holding
    // thousands of links is holding small ones. A curve spanning the window instead
    // would cover the frame many times over and measure overdraw, not the far field.
    let end = Point::new(start.x + 8.0 + unit(3) * 16.0, start.y + (unit(4) - 0.5) * 40.0);
    let reach = ((end.x - start.x).abs() * 0.5).max(1.0);
    CubicBez::new(
        start,
        Point::new(start.x + reach, start.y),
        Point::new(end.x - reach, end.y),
        end,
    )
}

impl Widget for FarField {
    type Action = NoAction;

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(200.0),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {}

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        let box_rect = ctx.content_box();
        painter.fill(box_rect, Color::from_rgb8(0x1c, 0x1c, 0x20)).draw();

        // One path per command, curves dealt round-robin between them: the split is
        // by index and not by region, so no group gets a smaller bounding box than
        // another and the only thing that changes across the sweep is the count.
        let groups = self.groups.clamp(1, self.count.max(1));
        let mut paths = vec![BezPath::new(); groups];
        for i in 0..self.count {
            let curve = far_curve(i, box_rect);
            let path = &mut paths[i % groups];
            path.move_to(curve.p0);
            path.curve_to(curve.p1, curve.p2, curve.p3);
        }

        // One brush and one stroke for every group. Same style, different commands —
        // which is precisely the thing §31 collapsed, and the thing this measures.
        let stroke = Stroke::new(self.width);
        for path in &paths {
            painter.stroke(path, &stroke, self.tint).draw();
        }
    }

    fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

fn harness<W: Widget>(widget: W) -> TestHarness<W> {
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(widget),
        PhysicalSize::new(SIZE.0, SIZE.1),
    );
    let _ = harness.redraw();
    harness
}

fn logical_size() -> Size {
    Size::new(f64::from(SIZE.0), f64::from(SIZE.1))
}

/// One scenario's numbers.
struct Report {
    name: &'static str,
    frames: usize,
    total: Duration,
    worst: Duration,
    counters: HostCounters,
    /// Layout passes the widget tree ran while the scenario was measured.
    layouts: u64,
    /// Declarations the external widget made, where there is one.
    declared: u64,
}

impl Report {
    fn mean_ms(&self) -> f64 {
        self.total.as_secs_f64() * 1000.0 / self.frames as f64
    }

    fn per_frame(&self, count: u64) -> f64 {
        count as f64 / self.frames as f64
    }

    /// Holes a widget declared but the host never saw.
    fn holes_lost(&self) -> u64 {
        self.declared.saturating_sub(self.counters.holes)
    }

    /// Frames in which the scenario's external content was not a hole.
    ///
    /// The counter for §26.1, and it is this one rather than "declared minus
    /// delivered" because that difference is zero both when every hole arrives and
    /// when the widget quietly stopped declaring any — which is exactly the failure
    /// mode. Checked by breaking it: a widget that declares once and stops leaves
    /// this at one per frame while the difference stays at zero.
    fn frames_without_a_hole(&self) -> u64 {
        self.frames as u64 - self.counters.holes.min(self.frames as u64)
    }

    fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: self.name,
            frames: self.frames,
            mean_ms: self.mean_ms(),
            worst_ms: self.worst.as_secs_f64() * 1000.0,
            materialised: 0,
            detail: String::new(),
            child_layouts_per_frame: self.per_frame(self.layouts),
            builds_per_frame: 0.0,
            far_repaints_per_frame: 0.0,
            extra: vec![
                ("layers_per_frame", self.per_frame(self.counters.layers)),
                ("scenes_per_frame", self.per_frame(self.counters.scenes)),
                ("holes_per_frame", self.per_frame(self.counters.holes)),
                ("holes_declared", self.declared as f64),
                ("holes_lost", self.holes_lost() as f64),
                ("frames_without_a_hole", self.frames_without_a_hole() as f64),
            ],
        }
    }

    fn print(&self) {
        println!(
            "{:<24} {:>7.3} ms/frame  worst {:>7.3} ms  layers/frame {:>5.2}  \
             holes/frame {:>5.2}  layouts/frame {:>5.2}",
            self.name,
            self.mean_ms(),
            self.worst.as_secs_f64() * 1000.0,
            self.per_frame(self.counters.layers),
            self.per_frame(self.counters.holes),
            self.per_frame(self.layouts),
        );
    }
}

/// Runs `frames` frames of a harness through a host, timing each one.
fn measure<W: Widget>(
    name: &'static str,
    harness: &mut TestHarness<W>,
    host: &mut Host,
    frames: usize,
    layouts: &Rc<Cell<u64>>,
    mut declared: impl FnMut(&TestHarness<W>) -> u64,
) -> Report {
    let before = host.counters();
    let layouts_before = layouts.get();
    let declared_before = declared(harness);
    let (mut total, mut worst) = (Duration::ZERO, Duration::ZERO);

    for _ in 0..frames {
        let start = Instant::now();
        // The window loop's frame, minus the window: animate, draw, compose,
        // rasterise. See `blazy_shell::window`.
        harness.animate_ms(16);
        let (plan, _tree) = harness.redraw();
        host.render(&plan, logical_size()).expect("the host renders");
        let elapsed = start.elapsed();
        total += elapsed;
        worst = worst.max(elapsed);
    }

    let after = host.counters();
    Report {
        name,
        frames,
        total,
        worst,
        counters: HostCounters {
            frames: after.frames - before.frames,
            layers: after.layers - before.layers,
            scenes: after.scenes - before.scenes,
            holes: after.holes - before.holes,
            image_bytes: after.image_bytes - before.image_bytes,
        },
        layouts: layouts.get() - layouts_before,
        declared: declared(harness) - declared_before,
    }
}

/// What the registry offers and what actually opened.
struct BackendReport {
    compiled: usize,
    /// Backends that are listed but have no working constructor behind them.
    unwired: usize,
    opened: Vec<(&'static str, bool)>,
}

fn backend_report() -> BackendReport {
    let mut unwired = 0;
    let mut opened = Vec::new();
    for &backend in COMPILED {
        let ok = match backend.open() {
            Ok(_) => true,
            // A missing GPU is a fact about the machine, not a defect in the build,
            // and a criterion that counted it would fail on every runner without one.
            Err(_) if backend.needs_device() => false,
            Err(_) => {
                unwired += 1;
                false
            },
        };
        opened.push((backend.name(), ok));
    }
    BackendReport {
        compiled: COMPILED.len(),
        unwired,
        opened,
    }
}

/// One row of the presentation table.
struct PresentRow {
    path: &'static str,
    size: (u32, u32),
    scale: f64,
    /// Turning the scene into pixels, or into a texture.
    draw_ms: f64,
    /// Getting those pixels to the window. Zero where nothing has to be moved.
    show_ms: f64,
    cpu_bytes: u64,
}

/// Times drawing and showing separately, which is the comparison the task is about.
///
/// The blit path is measured in full: rasterise into a buffer, then swap the channels
/// into a presentation buffer — the platform's own copy is not included, so the number
/// is a floor rather than the whole cost. The GPU path is submitted and waited on,
/// because timing a submission alone would measure the driver's queue.
fn presentation_table(opts: &Options) -> Vec<PresentRow> {
    let sizes: &[(u32, u32)] = if opts.quick {
        &PRESENT_SIZES[..1]
    } else {
        &PRESENT_SIZES
    };
    let scales: &[f64] = if opts.quick {
        &PRESENT_SCALES[..1]
    } else {
        &PRESENT_SCALES
    };
    let frames = 10_u64;

    println!("\npresentation: drawing and showing, separately");
    let mut rows = Vec::new();

    for &(width, height) in sizes {
        let layouts = Rc::new(Cell::new(0));
        let mut harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new(Panel {
                layouts: layouts.clone(),
                shapes: 60,
            }),
            PhysicalSize::new(width, height),
        );
        let _ = harness.redraw();
        let (plan, _tree) = harness.redraw();
        let logical = Size::new(f64::from(width), f64::from(height));

        for &scale in scales {
            // --- The blit path.
            let mut host = Host::any()
                .expect("some backend opens")
                .with_background(Color::from_rgb8(0x14, 0x14, 0x18));
            host.set_device_scale(scale);
            let mut draw = Duration::ZERO;
            let mut show = Duration::ZERO;
            let before = host.counters().image_bytes;
            for _ in 0..frames {
                let start = Instant::now();
                let frame = host.render(&plan, logical).expect("the host renders");
                draw += start.elapsed();

                let mut buffer = vec![0_u32; (frame.image.width * frame.image.height) as usize];
                let start = Instant::now();
                for (out, pixel) in buffer.iter_mut().zip(frame.image.data.chunks_exact(4)) {
                    *out = (u32::from(pixel[0]) << 16) | (u32::from(pixel[1]) << 8) | u32::from(pixel[2]);
                }
                show += start.elapsed();
            }
            // The host's own count of what it turned into pixels, not a second sum of
            // the same thing: that is what makes the criterion below check the code
            // rather than this loop's arithmetic.
            let cpu_bytes = (host.counters().image_bytes - before) / frames;
            rows.push(PresentRow {
                path: "blit",
                size: (width, height),
                scale,
                draw_ms: draw.as_secs_f64() * 1000.0 / frames as f64,
                show_ms: show.as_secs_f64() * 1000.0 / frames as f64,
                cpu_bytes,
            });

            // --- The GPU path, where there is a device for it.
            #[cfg(feature = "vello")]
            if let Ok(gpu) = blazy_shell::gpu::GpuFrames::offscreen(PhysicalSize::new(width, height)) {
                let mut gpu = gpu.with_background(Color::from_rgb8(0x14, 0x14, 0x18));
                // One frame first: the first one pays for pipelines and allocations.
                gpu.draw(&plan, logical, scale).expect("the GPU draws");
                gpu.wait();

                let start = Instant::now();
                for _ in 0..frames {
                    gpu.draw(&plan, logical, scale).expect("the GPU draws");
                }
                gpu.wait();
                let draw = start.elapsed();

                rows.push(PresentRow {
                    path: "swapchain",
                    size: (width, height),
                    scale,
                    draw_ms: draw.as_secs_f64() * 1000.0 / frames as f64,
                    // The blit into the swapchain is a GPU copy inside the same
                    // submission; there is no separate cost to attribute here, and
                    // the honest thing is to say so rather than to invent one.
                    show_ms: 0.0,
                    cpu_bytes: gpu.counters().cpu_bytes,
                });
            }
        }
    }

    for row in &rows {
        println!(
            "  {:<10} {:>4}x{:<4} x{:<4}  draw {:>7.3} ms  show {:>7.3} ms  through memory {:>8} bytes",
            row.path, row.size.0, row.size.1, row.scale, row.draw_ms, row.show_ms, row.cpu_bytes,
        );
    }
    rows
}

/// Curves in the far-field scene the raster table draws.
///
/// The order of magnitude §31.5 measured on 5000 nodes: ~8500 link curves recorded
/// for one canvas at an overview zoom.
const RASTER_CURVES: usize = 8000;
/// Commands the same curves are split into, from the batch to one per curve.
const RASTER_GROUPS: [usize; 6] = [1, 8, 64, 512, 2048, RASTER_CURVES];
/// Stroke widths, in logical pixels: coverage at a fixed command count.
///
/// `LinkStyle::width` is 2 **canvas** units, so a link at the far-field zoom of 0.04
/// is 0.08 px wide and at 0.2 it is 0.4 — the far field lives at the thin end of this
/// sweep, and the thick end is there to say what ink costs when there is a lot of it.
const RASTER_WIDTHS: [f64; 4] = [0.1, 0.5, 2.0, 8.0];
/// Device scales the raster table walks. x2 is x4 the pixels (§27.4).
const RASTER_SCALES: [f64; 2] = [1.0, 2.0];
/// Widths the command sweep is run at, so the answer is not read off one ink level.
const RASTER_SWEEP_WIDTHS: [f64; 2] = [0.5, 2.0];
/// The two tints the raster table alternates between, frame by frame.
///
/// Far apart in luma on purpose: the check below compares a frame against both of
/// them, and the further apart they are the less room there is for a stale frame to
/// look like a fresh one.
const RASTER_TINTS: [Color; 2] = [Color::from_rgb8(0x28, 0x30, 0x78), Color::from_rgb8(0xf0, 0xe4, 0xb0)];
/// Rows of the frame read back to check that a frame was drawn.
const RASTER_STRIP: u32 = 32;
/// Ink a row has to have before its time means anything, as a fraction of the frame.
///
/// The blit path is the reference every other row is checked against, so nothing
/// checks *it* — except this: a rasteriser that drew nothing leaves an empty frame,
/// and an empty frame is very fast.
const RASTER_MIN_INK: f64 = 0.01;

/// One row of the raster table: a scene, a path to the screen, and both halves of it.
struct RasterRow {
    path: &'static str,
    groups: usize,
    width: f64,
    scale: f64,
    /// Draw commands in the layer plan (`bench_utils::plan::commands`) — §31's counter.
    commands: usize,
    /// Draw objects in the encoding vello would rasterise, where it can be built.
    objects: usize,
    /// Path segments in the same encoding.
    segments: u64,
    /// Fraction of the frame that is not the panel's own fill: the ink.
    coverage: f64,
    /// The worst a frame's strip differed from what the CPU rasteriser drew.
    ///
    /// A timing row for a frame nobody looked at is worth nothing: a frame that
    /// failed, was skipped or overflowed an allocator is *fast*, and no clock can
    /// tell that from a frame that was drawn (§32.4).
    difference: f64,
    /// Frames of this row that did not come out as the picture that was asked for.
    unverified: usize,
    /// Whether the rasteriser refused this scene before drawing it (§33).
    refused: bool,
    /// Whether a refused scene, drawn anyway, came out right after all.
    ///
    /// The other half of the check: a guard that refuses frames the rasteriser could
    /// have drawn is as wrong as one that lets a silent failure through, and only
    /// forcing the frame can tell the two apart.
    false_refusal: bool,
    /// What the tile check costs before the frame is sent (§33.3).
    check_ms: f64,
    /// Passes and plan assembly — the half §31 measured.
    plan_ms: f64,
    /// Turning that plan into pixels, or into a texture — the half it did not.
    raster_ms: f64,
}

/// What vello is asked to draw, counted before anything draws it.
///
/// The answer to the task's first question, and it is better than the task hoped for:
/// a scene can be **encoded without a device** and the encoding read out. The counting
/// lives in `blazy_shell::encode` (§35.1) because the canvas benchmark needs the same
/// number and cannot reach `imaging_vello` from where it sits; here it is one call.
///
/// The CPU rasteriser has no equivalent: `imaging_vello_cpu` exposes a renderer and
/// nothing about the work inside it, so on that path only the clock can answer.
#[cfg(feature = "vello")]
fn encoded(plan: &masonry::app::VisualLayerPlan, scale: f64, frame: PhysicalSize<u32>) -> (usize, u64) {
    let composition = blazy_shell::Composition::new(plan, scale);
    let counts = blazy_shell::encode::encoded(&composition.scene, frame);
    (counts.objects, counts.segments)
}

#[cfg(not(feature = "vello"))]
fn encoded(_plan: &masonry::app::VisualLayerPlan, _scale: f64, _frame: PhysicalSize<u32>) -> (usize, u64) {
    (0, 0)
}

/// Ink in a frame: the fraction of pixels that are not the background.
fn ink(pixels: &[u8], panel: Color) -> f64 {
    let base = panel.to_rgba8();
    let count = pixels
        .chunks_exact(4)
        .filter(|pixel| pixel[0] != base.r || pixel[1] != base.g || pixel[2] != base.b)
        .count();
    count as f64 / (pixels.len() / 4) as f64
}

/// `rows` rows of an RGBA image, starting at `first`.
fn strip(pixels: &[u8], width: u32, first: u32, rows: u32) -> &[u8] {
    let row_bytes = (width * 4) as usize;
    let start = first as usize * row_bytes;
    &pixels[start..start + rows as usize * row_bytes]
}

/// Mean absolute luma difference between two frames of the same size, over 255.
///
/// Not `bench_utils::render::differing_fraction`, which asks whether *any* pixel
/// changed: two rasterisers antialias differently and disagree about nearly every
/// edge pixel by one or two levels, so that fraction is near 1 for two frames that
/// look identical. What matters here is whether the ink is in the same places.
fn frame_difference(a: &[u8], b: &[u8]) -> f64 {
    if a.len() != b.len() {
        return 1.0;
    }
    let luma = |pixel: &[u8]| 0.299 * f64::from(pixel[0]) + 0.587 * f64::from(pixel[1]) + 0.114 * f64::from(pixel[2]);
    let total: f64 = a
        .chunks_exact(4)
        .zip(b.chunks_exact(4))
        .map(|(p, q)| (luma(p) - luma(q)).abs())
        .sum();
    total / (a.len() / 4) as f64 / 255.0
}

/// What the guard in front of the GPU path costs per frame (§33.3, §34.3).
///
/// Measured rather than asserted to be small: two cheap counts, and behind them a walk
/// over the composed scene — which for a deeply nested one also builds a map of the
/// frame's tiles.
#[cfg(feature = "vello")]
fn tile_check_cost(plan: &masonry::app::VisualLayerPlan, scale: f64, frame: PhysicalSize<u32>, frames: usize) -> f64 {
    let composed = blazy_shell::Composition::new(plan, scale).scene;
    let start = Instant::now();
    for _ in 0..frames {
        // What the frame path really pays, cheap answer included — not the exact walk,
        // which it reaches only for a scene that could plausibly be over budget.
        let _ = blazy_shell::tiles::over_budget(&composed, frame);
    }
    start.elapsed().as_secs_f64() * 1000.0 / frames as f64
}

#[cfg(not(feature = "vello"))]
fn tile_check_cost(
    _plan: &masonry::app::VisualLayerPlan,
    _scale: f64,
    _frame: PhysicalSize<u32>,
    _frames: usize,
) -> f64 {
    0.0
}

/// A plan with nothing in it, which composes to the background and nothing else.
///
/// What the texture holds after a frame that was not drawn — the baseline a forced
/// draw of a refused scene is judged against.
fn blank() -> masonry::app::VisualLayerPlan {
    masonry::app::VisualLayerPlan { layers: Vec::new() }
}

/// The GPU path, opened once for the whole table.
///
/// One device rather than one per row. `GpuFrames::draw` resizes its own texture, so
/// a single one serves every size here — and a device per row would put twenty driver
/// initialisations inside a measurement, which is a variable nobody asked for.
struct GpuPath {
    #[cfg(feature = "vello")]
    frames: Option<blazy_shell::gpu::GpuFrames>,
}

impl GpuPath {
    #[cfg(feature = "vello")]
    fn open(panel: Color) -> Self {
        Self {
            frames: blazy_shell::gpu::GpuFrames::offscreen(PhysicalSize::new(SIZE.0, SIZE.1))
                .ok()
                .map(|gpu| gpu.with_background(panel)),
        }
    }

    #[cfg(not(feature = "vello"))]
    fn open(_panel: Color) -> Self {
        Self {}
    }

    /// Frames this path copied back out of the texture, for the counter's own check.
    fn readbacks(&self) -> u64 {
        #[cfg(feature = "vello")]
        {
            self.frames.as_ref().map_or(0, |gpu| gpu.counters().readbacks)
        }
        #[cfg(not(feature = "vello"))]
        {
            0
        }
    }
}

/// One scene at one device scale, on both paths.
///
/// `groups` and `width` are the two axes: the first changes how many commands the
/// same ink arrives in, the second how much ink there is at the same command count.
///
/// The scene is drawn in **two tints, alternating frame by frame**, and that is not
/// decoration. vello can fail to draw a frame and report nothing (§32.4); the target
/// then keeps whatever was in it, which is the *previous* frame — so a check that only
/// asks "is there ink in the texture" passes on a frame that was never drawn. Two
/// tints make the last frame identifiable, and the check runs after every frame
/// rather than once at the end.
fn raster_case(gpu: &mut GpuPath, groups: usize, width: f64, scale: f64, frames: usize) -> Vec<RasterRow> {
    let panel = Color::from_rgb8(0x1c, 0x1c, 0x20);
    let mut harnesses: Vec<_> = RASTER_TINTS
        .iter()
        .map(|&tint| {
            let mut harness = TestHarness::create_with_size(
                default_property_set(),
                NewWidget::new(FarField {
                    count: RASTER_CURVES,
                    groups,
                    width,
                    tint,
                }),
                PhysicalSize::new(SIZE.0, SIZE.1),
            );
            let _ = harness.redraw();
            harness
        })
        .collect();

    // The CPU half, in the same shape as `measure`: animate, then rebuild the plan.
    let mut plan_total = Duration::ZERO;
    for frame in 0..frames {
        let harness = &mut harnesses[frame % RASTER_TINTS.len()];
        harness.animate_ms(16);
        let start = Instant::now();
        let _ = harness.redraw();
        plan_total += start.elapsed();
    }
    let plan_ms = plan_total.as_secs_f64() * 1000.0 / frames as f64;

    let plans: Vec<_> = harnesses.iter_mut().map(|harness| harness.redraw().0).collect();
    let commands = bench_utils::plan::commands(&plans[0]);
    let frame_size = {
        let (w, h) = (f64::from(SIZE.0) * scale, f64::from(SIZE.1) * scale);
        PhysicalSize::new(w.ceil() as u32, h.ceil() as u32)
    };
    let (objects, segments) = encoded(&plans[0], scale, frame_size);
    let check_ms = tile_check_cost(&plans[0], scale, frame_size, frames);
    let mut rows = Vec::new();

    // --- The blit path: compose and rasterise on the CPU. The channel swap that
    // follows it is measured in §27.4 and does not depend on what is drawn, so it is
    // left out here rather than counted twice.
    let mut host = Host::any().expect("some backend opens").with_background(panel);
    host.set_device_scale(scale);
    let references: Vec<_> = plans
        .iter()
        .map(|plan| host.render(plan, logical_size()).expect("the host renders").image)
        .collect();
    let coverage = ink(&references[0].data, panel);

    let start = Instant::now();
    for frame in 0..frames {
        let _ = host
            .render(&plans[frame % plans.len()], logical_size())
            .expect("the host renders");
    }
    let raster_ms = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    rows.push(RasterRow {
        path: "blit",
        groups,
        width,
        scale,
        commands,
        objects,
        segments,
        coverage,
        difference: 0.0,
        unverified: 0,
        refused: false,
        false_refusal: false,
        check_ms,
        plan_ms,
        raster_ms,
    });

    // --- The GPU path, where there is a device for it.
    #[cfg(feature = "vello")]
    if let Some(gpu) = gpu.frames.as_mut() {
        let rows_read = RASTER_STRIP.min(frame_size.height);
        let first_row = frame_size.height.saturating_sub(rows_read) / 2;
        let strips: Vec<_> = references
            .iter()
            .map(|image| strip(&image.data, image.width, first_row, rows_read).to_vec())
            .collect();

        // The first frame pays for pipelines, allocations and any resize; nobody
        // wants it in the average. It is also where a scene over the tile budget is
        // refused (§33), and a refused scene has no frame time to report — what it
        // has instead is a question: would it have drawn?
        if let Err(error) = gpu.draw(&plans[0], logical_size(), scale) {
            assert!(
                matches!(error, blazy_shell::PresentError::SceneTooLarge { .. }),
                "the GPU path failed for a reason this table does not handle: {error}"
            );

            // Forced through the check, and judged against a texture that is known
            // not to hold the picture already. The two tints are not enough here:
            // every row of this sweep draws the *same ink* — only the grouping
            // changes — so the frame left behind by the previous row would pass for a
            // successful forced draw. So: put a blank frame in the texture, force the
            // scene, and ask which of the two the result is closer to.
            gpu.draw_unchecked(&blank(), frame_size, scale)
                .expect("a blank frame draws");
            gpu.wait();
            let blank_strip = gpu.read_rows(first_row, rows_read);

            gpu.draw_unchecked(&plans[0], frame_size, scale)
                .expect("the GPU draws anyway");
            gpu.wait();
            let drawn = gpu.read_rows(first_row, rows_read);
            let false_refusal = frame_difference(&strips[0], &drawn) < frame_difference(&blank_strip, &drawn);

            rows.push(RasterRow {
                path: "swapchain",
                groups,
                width,
                scale,
                commands,
                objects,
                segments,
                coverage: ink(&gpu.read_pixels(), panel),
                difference: 0.0,
                unverified: 0,
                refused: true,
                false_refusal,
                check_ms,
                plan_ms,
                raster_ms: 0.0,
            });
            return rows;
        }
        gpu.wait();

        let (mut total, mut worst, mut unverified) = (Duration::ZERO, 0.0_f64, 0);
        for frame in 0..frames {
            let tint = frame % plans.len();
            let start = Instant::now();
            gpu.draw(&plans[tint], logical_size(), scale)
                .expect("the GPU draws: the first frame already passed the check");
            // Waiting per frame rather than submitting the run and waiting once:
            // this is a claim about what one frame costs, and a window submits one
            // frame and shows it. §27.4 batched deliberately — it was asking about
            // throughput.
            gpu.wait();
            total += start.elapsed();

            // Untimed, and half a megabyte rather than thirteen.
            //
            // No tolerance to choose: the frame is compared against *both* tints and
            // has to be closer to the one that was asked for. A frame that was never
            // drawn holds the frame before it, which was the other tint — and a
            // threshold on "close enough" would have had to be looser than the
            // difference between two rasterisers' antialiasing and tighter than the
            // difference between two tints, which at a half-pixel stroke width is not
            // a gap you can put a number in (it was tried; it missed three rows).
            let drawn = gpu.read_rows(first_row, rows_read);
            let difference = frame_difference(&strips[tint], &drawn);
            let other = frame_difference(&strips[(tint + 1) % strips.len()], &drawn);
            worst = worst.max(difference);
            if difference >= other {
                unverified += 1;
            }
        }

        rows.push(RasterRow {
            path: "swapchain",
            groups,
            width,
            scale,
            commands,
            objects,
            segments,
            coverage: ink(&gpu.read_pixels(), panel),
            difference: worst,
            unverified,
            refused: false,
            false_refusal: false,
            check_ms,
            plan_ms,
            raster_ms: total.as_secs_f64() * 1000.0 / frames as f64,
        });
    }
    #[cfg(not(feature = "vello"))]
    let _ = gpu;

    rows
}

/// Both halves of a far-field frame, over both axes and both paths.
///
/// The table §31.6 asked for: §31 measured the plan and this measures what a
/// rasteriser then does with it, in one row so that neither half can be quoted
/// without the other.
///
/// The device scale is the **outer** loop, so the frame size changes twice in a run
/// rather than forty times. That is not tidiness: alternating the target size makes
/// the GPU path hand back empty frames on this machine (§32.4), and a benchmark that
/// provokes a defect it is not measuring reports noise instead of an answer.
struct RasterReport {
    rows: Vec<RasterRow>,
    /// Readbacks the table made, which is what checks the readback counter itself.
    readbacks: u64,
}

fn raster_table(opts: &Options) -> RasterReport {
    let frames = if opts.quick { 6 } else { 12 };
    // Both scales even in the quick set: one of the criteria compares a scene's
    // encoding across scales, and with a single scale it would have nothing to
    // compare and would pass by having nothing to say.
    let scales: &[f64] = &RASTER_SCALES;
    // 512 is in the quick set on purpose: at the larger scale it is the scene the
    // rasteriser refuses (§33), and without it the criteria about the refusal would
    // pass by having nothing to look at.
    let groups: &[usize] = if opts.quick {
        &[RASTER_GROUPS[0], RASTER_GROUPS[3], RASTER_GROUPS[5]]
    } else {
        &RASTER_GROUPS
    };
    let sweep_widths: &[f64] = if opts.quick {
        &RASTER_SWEEP_WIDTHS[..1]
    } else {
        &RASTER_SWEEP_WIDTHS
    };
    let widths: &[f64] = if opts.quick {
        &RASTER_WIDTHS[1..2]
    } else {
        &RASTER_WIDTHS
    };

    let mut gpu = GpuPath::open(Color::from_rgb8(0x1c, 0x1c, 0x20));
    println!(
        "\nrasterisation: {RASTER_CURVES} curves, the same ink in a different number of commands\n  \
         (plan = passes and plan assembly, raster = pixels or texture)"
    );
    let mut rows = Vec::new();
    for &scale in scales {
        for &width in sweep_widths {
            for &count in groups {
                rows.extend(raster_case(&mut gpu, count, width, scale, frames));
            }
        }
    }
    print_raster(&rows);

    println!("\nrasterisation: one command, the same curves, more ink");
    let mut coverage_rows = Vec::new();
    for &width in widths {
        coverage_rows.extend(raster_case(&mut gpu, 1, width, scales[0], frames));
    }
    print_raster(&coverage_rows);
    rows.extend(coverage_rows);

    let undrawn: usize = rows.iter().map(|row| row.unverified).sum();
    if undrawn > 0 {
        println!(
            "\n  {undrawn} frames marked `!` were never drawn: the GPU path reported success and left\n  \
             the previous frame in the texture (§32.4). Their times are not frame times."
        );
    }
    if let Some(worst) = rows
        .iter()
        .max_by(|a, b| a.check_ms.total_cmp(&b.check_ms))
        .filter(|row| row.check_ms > 0.0)
    {
        let batched = rows
            .iter()
            .filter(|row| row.groups == 1)
            .max_by(|a, b| a.check_ms.total_cmp(&b.check_ms));
        println!(
            "\n  the tile check (§33.3) costs at most {:.3} ms here, on the scene of {} commands\n  \
             whose frame takes {:.1} ms to rasterise{}",
            worst.check_ms,
            worst.commands,
            worst.raster_ms,
            match batched {
                Some(row) => format!("; on the batched far field, {:.3} ms.", row.check_ms),
                None => ".".to_string(),
            },
        );
    }
    let refused = rows.iter().filter(|row| row.refused).count();
    if refused > 0 {
        let paranoid = rows.iter().filter(|row| row.false_refusal).count();
        println!(
            "\n  {refused} scenes marked `ref` need more tiles than the rasteriser can allocate and were\n  \
             refused before being drawn (§33). Forced through, {paranoid} of them produced the picture."
        );
    }

    RasterReport {
        readbacks: gpu.readbacks(),
        rows,
    }
}

/// The rows the JSON report archives, so the two halves can be diffed across commits.
///
/// One scene at one scale on each path, at both ends of the command sweep — the four
/// numbers §32 is argued from. The rest of the table is printed and not archived: a
/// report is for diffing a claim, not for keeping every row ever measured.
fn raster_records(rows: &[RasterRow]) -> Vec<ScenarioRecord> {
    let pick = |path: &str, groups: usize| {
        rows.iter().find(|row| {
            row.path == path && row.groups == groups && row.scale == 1.0 && (row.width - 0.5).abs() < f64::EPSILON
        })
    };

    [
        ("far field batched, cpu", "blit", 1),
        ("far field per curve, cpu", "blit", RASTER_CURVES),
        ("far field batched, gpu", "swapchain", 1),
        ("far field per curve, gpu", "swapchain", RASTER_CURVES),
    ]
    .into_iter()
    .filter_map(|(name, path, groups)| {
        let row = pick(path, groups)?;
        Some(ScenarioRecord {
            name,
            frames: 1,
            mean_ms: row.plan_ms + row.raster_ms,
            worst_ms: 0.0,
            materialised: 0,
            detail: String::new(),
            child_layouts_per_frame: 0.0,
            builds_per_frame: 0.0,
            far_repaints_per_frame: 0.0,
            extra: vec![
                ("commands", row.commands as f64),
                ("draw_objects", row.objects as f64),
                ("path_segments", row.segments as f64),
                ("coverage", row.coverage),
                ("plan_ms", row.plan_ms),
                ("raster_ms", row.raster_ms),
                ("frames_not_drawn", row.unverified as f64),
            ],
        })
    })
    .collect()
}

fn print_raster(rows: &[RasterRow]) {
    println!(
        "  {:<10} {:>8} {:>6} {:>5}  {:>8} {:>8} {:>9} {:>7}  {:>8}  {:>9} {:>9}",
        "path",
        "commands",
        "width",
        "scale",
        "objects",
        "segments",
        "coverage",
        "differs",
        "plan ms",
        "raster ms",
        "frame ms",
    );
    for row in rows {
        println!(
            "  {:<10} {:>8} {:>6.1} {:>5.1}  {:>8} {:>8} {:>8.1}% {:>6.3}{:<5}  {:>8.3}  {:>9.3} {:>9.3}",
            row.path,
            row.commands,
            row.width,
            row.scale,
            row.objects,
            row.segments,
            row.coverage * 100.0,
            row.difference,
            match (row.refused, row.false_refusal, row.unverified) {
                (true, true, _) => "REF!".to_string(),
                (true, false, _) => "ref ".to_string(),
                (false, _, 0) => "    ".to_string(),
                (false, _, undrawn) => format!("!{undrawn}  "),
            },
            row.plan_ms,
            row.raster_ms,
            row.plan_ms + row.raster_ms,
        );
    }
}

// --- MARK: nesting, and the second buffer (§34)

/// Nesting depths the sweep walks.
///
/// The switch has to be crossed in both directions to be tested (§28.4), and the two
/// device scales put the crossing in two places: a 1100x750 frame survives five nested
/// groups and is lost at six, a 2200x1500 one is lost at five. So the same four depths
/// measure both sides at one scale and only the far side at the other.
const NEST_DEPTHS: [usize; 4] = [1, 4, 5, 6];

/// A frame whose only load is nesting: `depth` groups that change nothing at all.
///
/// The scene an application builds without meaning to — a stack of opacity groups
/// around a panel — and the one vello's blend scratch runs out on long before its tile
/// buffer does (§34). Every group here is a visual no-op, so any difference between
/// two depths is the rasteriser losing the frame rather than the picture changing.
struct Nested {
    depth: usize,
    /// What the content is drawn in; two of these alternate frame by frame, so a frame
    /// the GPU never drew cannot pass for the frame before it (§32.4).
    tint: Color,
}

impl Widget for Nested {
    type Action = NoAction;

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(200.0),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {}

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        let box_rect = ctx.content_box();
        painter.fill(box_rect, Color::from_rgb8(0x1c, 0x1c, 0x20)).draw();

        for _ in 0..self.depth {
            painter.push_group(GroupRef::new());
        }
        // Enough ink to tell one tint from the other in a strip of the frame, and
        // spread over the frame so that no crop of it is blank.
        for i in 0..12 {
            let x = box_rect.x0 + box_rect.width() * f64::from(i) / 12.0;
            painter
                .fill(
                    Rect::new(
                        x + 4.0,
                        box_rect.y0 + 8.0,
                        x + box_rect.width() / 14.0,
                        box_rect.y1 - 8.0,
                    ),
                    self.tint,
                )
                .draw();
        }
        for _ in 0..self.depth {
            painter.pop_group();
        }
    }

    fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

/// One nesting depth at one device scale, on the GPU path.
struct NestRow {
    depth: usize,
    scale: f64,
    /// Words of blend scratch the check says this frame needs.
    words: u64,
    /// Whether a device was there to draw it: without one there is nothing to check.
    on_gpu: bool,
    /// The check refused the scene before anything was submitted.
    refused: bool,
    /// It was refused and would have drawn anyway — the paranoid failure.
    false_refusal: bool,
    /// Frames whose scene was accepted and whose picture never arrived — the blind
    /// failure, and the one §33 was written to make impossible.
    unverified: usize,
    /// What asking the question costs, per frame.
    check_ms: f64,
}

fn nest_case(gpu: &mut GpuPath, depth: usize, scale: f64, frames: usize) -> NestRow {
    let panel = Color::from_rgb8(0x1c, 0x1c, 0x20);
    let mut harnesses: Vec<_> = RASTER_TINTS
        .iter()
        .map(|&tint| {
            let mut harness = TestHarness::create_with_size(
                default_property_set(),
                NewWidget::new(Nested { depth, tint }),
                PhysicalSize::new(SIZE.0, SIZE.1),
            );
            let _ = harness.redraw();
            harness
        })
        .collect();
    let plans: Vec<_> = harnesses.iter_mut().map(|harness| harness.redraw().0).collect();
    let frame_size = {
        let (w, h) = (f64::from(SIZE.0) * scale, f64::from(SIZE.1) * scale);
        PhysicalSize::new(w.ceil() as u32, h.ceil() as u32)
    };
    let words = blazy_shell::tiles::blend_demand(&blazy_shell::Composition::new(&plans[0], scale).scene, frame_size);

    let mut row = NestRow {
        depth,
        scale,
        words,
        on_gpu: false,
        refused: false,
        false_refusal: false,
        unverified: 0,
        check_ms: tile_check_cost(&plans[0], scale, frame_size, frames.max(1)),
    };

    // The pictures the frames are judged against, from the CPU rasteriser, which has
    // no such ceiling (§33.5).
    let mut host = Host::any().expect("some backend opens").with_background(panel);
    host.set_device_scale(scale);
    let references: Vec<_> = plans
        .iter()
        .map(|plan| host.render(plan, logical_size()).expect("the host renders").image)
        .collect();

    #[cfg(feature = "vello")]
    if let Some(gpu) = gpu.frames.as_mut() {
        row.on_gpu = true;
        let rows_read = RASTER_STRIP.min(frame_size.height);
        let first_row = frame_size.height.saturating_sub(rows_read) / 2;
        let strips: Vec<_> = references
            .iter()
            .map(|image| strip(&image.data, image.width, first_row, rows_read).to_vec())
            .collect();

        if let Err(error) = gpu.draw(&plans[0], logical_size(), scale) {
            assert!(
                matches!(error, blazy_shell::PresentError::SceneTooDeep { .. }),
                "the GPU path refused a nested scene for a reason this table does not handle: {error}"
            );
            row.refused = true;

            // The same question the tile sweep asks (§33.4): forced past the check,
            // does the frame arrive? Judged against a blank frame put in the texture
            // first, because every row of this sweep draws the same ink and the row
            // before would otherwise pass for a successful draw.
            gpu.draw_unchecked(&blank(), frame_size, scale)
                .expect("a blank frame draws");
            gpu.wait();
            let blank_strip = gpu.read_rows(first_row, rows_read);

            gpu.draw_unchecked(&plans[0], frame_size, scale)
                .expect("the GPU draws anyway");
            gpu.wait();
            let drawn = gpu.read_rows(first_row, rows_read);
            row.false_refusal = frame_difference(&strips[0], &drawn) < frame_difference(&blank_strip, &drawn);
            return row;
        }
        gpu.wait();

        for frame in 0..frames {
            let tint = frame % plans.len();
            gpu.draw(&plans[tint], logical_size(), scale)
                .expect("the GPU draws: the first frame already passed the check");
            gpu.wait();
            let drawn = gpu.read_rows(first_row, rows_read);
            if frame_difference(&strips[tint], &drawn) >= frame_difference(&strips[(tint + 1) % strips.len()], &drawn) {
                row.unverified += 1;
            }
        }
    }
    #[cfg(not(feature = "vello"))]
    let _ = (gpu, frames);

    row
}

/// Nesting depth against the blend scratch, at two frame sizes.
///
/// The device scale is the outer loop for the reason `raster_table` gives: alternating
/// the target size provokes the defect of §32.4, and a benchmark that provokes a defect
/// it is not measuring reports noise.
fn nest_table(opts: &Options) -> Vec<NestRow> {
    let frames = if opts.quick { 4 } else { 8 };
    let mut gpu = GpuPath::open(Color::from_rgb8(0x1c, 0x1c, 0x20));
    println!(
        "\nnesting: groups that change nothing, against the rasteriser's blend scratch\n  \
         (budget {} words; a clip is charged along its edges, a group over its whole box)",
        blazy_shell::BLEND_BUDGET
    );
    let mut rows = Vec::new();
    for &scale in &RASTER_SCALES {
        for &depth in &NEST_DEPTHS {
            rows.push(nest_case(&mut gpu, depth, scale, frames));
        }
    }
    print_nest(&rows);
    rows
}

fn print_nest(rows: &[NestRow]) {
    println!(
        "  {:>5}  {:>5}  {:>12}  {:>9}  {:>8}  note",
        "depth", "scale", "words", "check ms", "state"
    );
    for row in rows {
        let state = if !row.on_gpu {
            "-"
        } else if row.refused {
            "refused"
        } else if row.unverified > 0 {
            "!"
        } else {
            "drawn"
        };
        let note = if row.false_refusal {
            "would have drawn"
        } else if row.unverified > 0 {
            "frames that never arrived"
        } else {
            ""
        };
        println!(
            "  {:>5}  x{:<4}  {:>12}  {:>9.3}  {state:>8}  {note}",
            row.depth, row.scale, row.words, row.check_ms
        );
    }
}

/// What the GPU path did over a run of frames, where a device exists.
#[cfg(feature = "vello")]
fn gpu_counters(frames: usize) -> Option<blazy_shell::PresentCounters> {
    let mut gpu = blazy_shell::gpu::GpuFrames::offscreen(PhysicalSize::new(SIZE.0, SIZE.1))
        .ok()?
        .with_background(Color::from_rgb8(0x14, 0x14, 0x18));

    let mut harness = harness(ExternalContent::new(Size::new(400.0, 300.0)));
    for _ in 0..frames {
        harness.animate_ms(16);
        let (plan, _tree) = harness.redraw();
        gpu.draw(&plan, logical_size(), 1.0).expect("the GPU draws");
    }
    gpu.wait();
    Some(gpu.counters())
}

pub fn run(opts: &Options) -> Outcome {
    let frames = opts.frames();
    println!(
        "blazy-shell - the host: composition, device scale, external content\n\
         window {}x{}, {frames} frames per scenario{}\n",
        SIZE.0,
        SIZE.1,
        if opts.quick { " (quick set)" } else { "" }
    );

    let backends = backend_report();
    println!("backends compiled in: {}", backends.compiled);
    for (name, opened) in &backends.opened {
        println!("  {name:<12} {}", if *opened { "opens" } else { "not available here" });
    }

    let mut reports = Vec::new();

    // --- Scenario 1: an ordinary frame.
    {
        let layouts = Rc::new(Cell::new(0));
        let mut harness = harness(Panel {
            layouts: layouts.clone(),
            shapes: 60,
        });
        let mut host = Host::any().expect("some backend opens");
        reports.push(measure("frame", &mut harness, &mut host, frames, &layouts, |_| 0));
    }

    // --- Scenario 2: external content, frame after frame.
    //
    // The claim of §4.3 and §14, and the trap of §26.1 in one: a hole has to survive
    // the frames in which nothing about it changed.
    let external = {
        let layouts = Rc::new(Cell::new(0));
        let mut harness = harness(ExternalContent::new(Size::new(400.0, 300.0)));
        let mut host = Host::any().expect("some backend opens");
        let report = measure("external content", &mut harness, &mut host, frames, &layouts, |h| {
            h.root_widget().declarations()
        });
        reports.push(report);
        reports.len() - 1
    };

    // --- Scenario 3: the device scale sweep.
    //
    // §9's third multiplier: composition, not layout. The counter is the tree's
    // layout passes while the same plan is composed at four different scales.
    let scale_layouts = Rc::new(Cell::new(0));
    let mut scale_frames = Vec::new();
    {
        let mut harness = harness(Panel {
            layouts: scale_layouts.clone(),
            shapes: 60,
        });
        let mut host = Host::any().expect("some backend opens");
        let (plan, _tree) = harness.redraw();
        let before = scale_layouts.get();

        println!("\ndevice scale: the same plan, composed at four scales");
        for scale in SCALES {
            host.set_device_scale(scale);
            let start = Instant::now();
            let frame = host.render(&plan, logical_size()).expect("the host renders");
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            println!(
                "  x{scale:<4}  frame {:>5}x{:<5}  {ms:>7.3} ms  layouts {:>3}",
                frame.image.width,
                frame.image.height,
                scale_layouts.get() - before,
            );
            scale_frames.push((scale, frame.image.width, ms));
        }
        scale_layouts.set(scale_layouts.get() - before);
    }

    // --- The frame the window would present.
    //
    // Configured the way `window::run` configures it, over a tree that does not cover
    // the window: presentation flattens alpha away, so the frame has to be opaque
    // before it gets there (§26.4).
    let transparent = {
        let mut harness = harness(Sparse);
        let mut host = Host::any()
            .expect("some backend opens")
            .with_background(Color::from_rgb8(0x14, 0x14, 0x18));
        let (plan, _tree) = harness.redraw();
        let frame = host.render(&plan, logical_size()).expect("the host renders");
        let count = frame.image.data.chunks_exact(4).filter(|pixel| pixel[3] != 255).count();
        println!("\nframe as presented: {count} pixels not fully opaque");
        count
    };

    let present_rows = presentation_table(opts);
    let raster = raster_table(opts);
    let nest_rows = nest_table(opts);
    #[cfg(feature = "vello")]
    let gpu = gpu_counters(frames);
    #[cfg(not(feature = "vello"))]
    let gpu: Option<blazy_shell::PresentCounters> = None;
    if gpu.is_none() {
        println!(
            "\nno GPU frame path measured here ({}); its criteria are absent from this run",
            if cfg!(feature = "vello") {
                "no graphics device"
            } else {
                "built without the vello feature"
            }
        );
    }

    println!();
    for report in &reports {
        report.print();
    }

    let outcome = Outcome {
        nodes: 0,
        viewport: SIZE,
        quick: opts.quick,
        criteria: evaluate(&Measured {
            external: &reports[external],
            backends: &backends,
            scale_layouts: scale_layouts.get(),
            scale_frames: &scale_frames,
            transparent,
            gpu,
            present_rows: &present_rows,
            raster_rows: &raster.rows,
            raster_readbacks: raster.readbacks,
            nest_rows: &nest_rows,
        }),
        scenarios: reports
            .iter()
            .map(Report::record)
            .chain(raster_records(&raster.rows))
            .collect(),
        sweep: Vec::new(),
        zoom_sweep: Vec::new(),
    };
    outcome.report("blazy-shell criteria");
    outcome
}

/// The criteria, evaluated against what was just measured.
///
/// Three of the four the task set; the fourth — that the crates below the host do not
/// depend on a window — is a fact about the dependency graph rather than about a
/// frame, and is checked by `cargo make deps-rule` instead (§26.4).
struct Measured<'a> {
    external: &'a Report,
    backends: &'a BackendReport,
    /// Layout passes the tree ran while the same plan was composed at four scales.
    scale_layouts: u64,
    /// Scale, frame width, milliseconds — one entry per scale composed.
    scale_frames: &'a [(f64, u32, f64)],
    /// Pixels of a frame that were left translucent.
    transparent: usize,
    /// The GPU path's own counters, absent where there is no device.
    gpu: Option<blazy_shell::PresentCounters>,
    present_rows: &'a [PresentRow],
    raster_rows: &'a [RasterRow],
    /// Readbacks the raster table made, which is what checks the readback counter.
    raster_readbacks: u64,
    /// The nesting sweep: what the blend-scratch guard did, row by row (§34).
    nest_rows: &'a [NestRow],
}

fn evaluate(measured: &Measured<'_>) -> Vec<Criterion> {
    let Measured {
        external,
        backends,
        scale_layouts,
        scale_frames,
        transparent,
        gpu,
        present_rows,
        raster_rows,
        raster_readbacks,
        nest_rows,
    } = *measured;
    let mut criteria = Vec::new();

    // The registry is the whole of "choose the backend at startup": a backend that is
    // offered and cannot be opened is the failure that would otherwise reach a user.
    criteria.push(Criterion {
        name: "compiled_backends_are_reachable",
        claim: "every backend in this build can be opened by name",
        kind: Kind::Counter,
        measured: backends.unwired as f64,
        bound: 1.0,
        unit: "backends unreachable",
    });

    // §9, §23.4: the device scale is a transform at composition. If it ever reaches
    // layout, a display change costs a relayout of the whole tree.
    criteria.push(Criterion {
        name: "device_scale_does_not_relayout",
        claim: "composing at a new device scale does not relayout",
        kind: Kind::Counter,
        measured: scale_layouts as f64,
        bound: 0.5,
        unit: "layout passes",
    });

    // And it has to actually do something: a scale that changed nothing would satisfy
    // the criterion above by doing no work at all.
    if let (Some(first), Some(last)) = (scale_frames.first(), scale_frames.last()) {
        let expected = (last.0 / first.0) * f64::from(first.1);
        criteria.push(Criterion {
            name: "device_scale_changes_the_frame",
            claim: "the frame follows the device scale",
            kind: Kind::Counter,
            measured: (f64::from(last.1) - expected).abs(),
            bound: 1.0,
            unit: "pixels off",
        });
    }

    // §4.3: a hole that does not reach the host is a 3D viewport with the UI's stale
    // pixels in it. Counted per frame rather than against the declarations, because
    // a widget that stopped declaring would otherwise satisfy the criterion by
    // producing nothing to lose.
    criteria.push(Criterion {
        name: "external_holes_reach_the_host",
        claim: "external content is a hole in every frame",
        kind: Kind::Counter,
        measured: external.frames_without_a_hole() as f64,
        bound: 1.0,
        unit: "frames with no hole",
    });

    // Presentation drops alpha, so anything left translucent loses the anti-aliasing
    // the rasteriser computed — a curve becomes a staircase and low-alpha pixels run
    // to white. Found by eye on the area screen, and now counted.
    criteria.push(Criterion {
        name: "the_presented_frame_is_opaque",
        claim: "the frame a window would present has no translucent pixels",
        kind: Kind::Counter,
        measured: transparent as f64,
        bound: 1.0,
        unit: "pixels not opaque",
    });

    // --- The rasteriser (§32). What §31 counted in the plan, counted again in the
    // units the rasteriser actually works in.

    // The bridge between the two halves: `plan::commands` is worth counting only if a
    // command is what the rasteriser is handed. It is — exactly one draw object each.
    // Absent without the `vello` feature, where nothing can be encoded.
    if raster_rows.iter().any(|row| row.objects > 0) {
        criteria.push(Criterion {
            name: "a_command_is_a_draw_object",
            claim: "every draw command in the plan is one draw object for the rasteriser",
            kind: Kind::Counter,
            measured: raster_rows.iter().filter(|row| row.objects != row.commands).count() as f64,
            bound: 1.0,
            unit: "rows where the two disagree",
        });

        // §31 in the rasteriser's units: the far field arrives as a handful of draw
        // objects however many curves are in it. Broken by taking the batch apart,
        // which is what the sweep's other end measures: 8001.
        let batched = raster_rows.iter().filter(|row| row.groups == 1).map(|row| row.objects);
        criteria.push(Criterion {
            name: "the_far_field_is_a_handful_of_draw_objects",
            claim: "a batched far field is a few draw objects, not one per curve",
            kind: Kind::Counter,
            measured: batched.max().unwrap_or(0) as f64,
            bound: 64.0,
            unit: "draw objects",
        });

        // §9 and §23.4 in the same units: the device scale is a transform at
        // composition, so the rasteriser is asked to draw the same thing at every
        // scale. A host that re-encoded per scale would show up here as a different
        // number of objects or segments for the same scene.
        let mut re_encoded = 0;
        for row in raster_rows {
            let same_scene = raster_rows
                .iter()
                .find(|other| other.groups == row.groups && (other.width - row.width).abs() < f64::EPSILON);
            if let Some(first) = same_scene
                && (first.objects != row.objects || first.segments != row.segments)
            {
                re_encoded += 1;
            }
        }
        criteria.push(Criterion {
            name: "the_device_scale_does_not_re_encode",
            claim: "the rasteriser is asked to draw the same scene at every device scale",
            kind: Kind::Counter,
            measured: f64::from(re_encoded),
            bound: 1.0,
            unit: "rows re-encoded",
        });
    }

    // The whole table is timings, and a rasteriser that drew nothing is fast. The
    // CPU path is the reference every GPU row is checked against, so this is what
    // checks the reference. A refused row is exempt: it has no frame by construction,
    // and the empty picture it reports is the forced draw that proved the refusal
    // right (§33.3).
    if !raster_rows.is_empty() {
        criteria.push(Criterion {
            name: "every_measured_frame_has_ink",
            claim: "every frame the raster table timed has something in it",
            kind: Kind::Counter,
            measured: raster_rows
                .iter()
                .filter(|row| !row.refused && row.coverage < RASTER_MIN_INK)
                .count() as f64,
            bound: 1.0,
            unit: "rows with an empty frame",
        });

        // §33, both halves. The silent failure this replaced: a frame vello reports
        // as drawn and did not draw. After the check there should be none, because
        // the scenes that would have failed are refused before they are timed.
        criteria.push(Criterion {
            name: "no_frame_is_reported_as_drawn_when_it_was_not",
            claim: "no timing here was measured over a frame the rasteriser skipped",
            kind: Kind::Counter,
            measured: raster_rows.iter().filter(|row| row.unverified > 0).count() as f64,
            bound: 1.0,
            unit: "rows with an undrawn frame",
        });

        // And the other half: a guard that refuses frames the rasteriser could have
        // drawn is as wrong as one that lets the silent failure through. Every
        // refusal in the sweep is forced through and looked at.
        criteria.push(Criterion {
            name: "the_tile_budget_check_is_not_paranoid",
            claim: "every scene the check refused really does not draw",
            kind: Kind::Counter,
            measured: raster_rows.iter().filter(|row| row.false_refusal).count() as f64,
            bound: 1.0,
            unit: "scenes refused for nothing",
        });

        // Both of the above pass trivially on a sweep that never crosses the budget,
        // so the sweep has to cross it. Counted from the failing side, as always.
        if raster_rows.iter().any(|row| row.path == "swapchain") {
            criteria.push(Criterion {
                name: "the_tile_budget_check_is_exercised",
                claim: "the sweep contains a scene over the rasteriser's tile budget",
                kind: Kind::Counter,
                measured: f64::from(u8::from(!raster_rows.iter().any(|row| row.refused))),
                bound: 1.0,
                unit: "sweeps that never reach the budget",
            });
        }

        // The claim §32 rests on, and the one that decides what is worth optimising
        // next: a far-field frame is the rasteriser's time, not the plan's. Timing,
        // because there is no counter for "what a rasteriser did" (§32.1) — and with
        // the margin that kind demands: measured at 0.002 against a bound of 0.5.
        if let Some(row) = raster_rows
            .iter()
            .find(|row| row.path == "blit" && row.groups == 1 && row.raster_ms > 0.0)
        {
            criteria.push(Criterion {
                name: "the_far_field_frame_is_rasterisation",
                claim: "assembling a far-field plan costs a fraction of drawing it",
                kind: Kind::Timing,
                measured: row.plan_ms / row.raster_ms,
                bound: 0.5,
                unit: "plan per raster",
            });
        }
    }

    // --- The blend scratch (§34). The same three criteria as the tile budget, on the
    // buffer a user interface reaches first, and absent where there is no device.
    if nest_rows.iter().any(|row| row.on_gpu) {
        // The blind failure: a scene the guard let through whose frame never arrived.
        criteria.push(Criterion {
            name: "no_nested_frame_is_lost_in_silence",
            claim: "every nested scene the check accepted was drawn",
            kind: Kind::Counter,
            measured: nest_rows.iter().map(|row| row.unverified).sum::<usize>() as f64,
            bound: 1.0,
            unit: "frames that never arrived",
        });

        // The paranoid failure: a frame refused that the rasteriser would have drawn.
        // This is the one that costs a user something the silent version did not — a
        // frame that could have been shown and was not.
        criteria.push(Criterion {
            name: "the_blend_budget_check_is_not_paranoid",
            claim: "every nested scene the check refused really does not draw",
            kind: Kind::Counter,
            measured: nest_rows.iter().filter(|row| row.false_refusal).count() as f64,
            bound: 1.0,
            unit: "scenes refused for nothing",
        });

        // Both of the above pass on a sweep that never nests deep enough to matter, so
        // the sweep has to reach the ceiling. Counted from the failing side (§20.9).
        criteria.push(Criterion {
            name: "the_blend_budget_check_is_exercised",
            claim: "the sweep contains a scene over the rasteriser's blend budget",
            kind: Kind::Counter,
            measured: f64::from(u8::from(!nest_rows.iter().any(|row| row.refused))),
            bound: 1.0,
            unit: "sweeps that never reach the budget",
        });
    }

    // --- The GPU frame path (§27). Absent where there is no device to measure it on,
    // which is the honest answer on a runner without one.
    if let Some(gpu) = gpu {
        let frames = gpu.frames.max(1) as f64;

        // What the task is for: the frame is drawn and shown without ever existing as
        // bytes in main memory.
        //
        // Zero on this path is structural — nothing in `GpuFrames` can produce frame
        // bytes — so on its own this criterion could never fail, which is the one
        // thing a criterion must not be. It is paired with
        // `the_frame_byte_counter_is_not_a_stub` below: that one fails if the counter
        // stops reporting the bytes the blit path really does move, and together they
        // say "the number works, and on this path it is zero".
        criteria.push(Criterion {
            name: "swapchain_frame_stays_on_the_gpu",
            claim: "the GPU frame path moves no frame data through memory",
            kind: Kind::Counter,
            measured: gpu.cpu_bytes as f64 / frames,
            bound: 1.0,
            unit: "bytes/frame",
        });

        // The absurdity §27 removes: a GPU rasteriser that has to hand back pixels
        // copies them out of video memory every frame.
        criteria.push(Criterion {
            name: "gpu_frames_are_not_read_back",
            claim: "the GPU frame path does not read the frame back",
            kind: Kind::Counter,
            measured: gpu.readbacks as f64 / frames,
            bound: 1.0,
            unit: "readbacks/frame",
        });

        // The other half of the pair above: the same counter, on the path that has to
        // report megabytes. Counted from the failing side, as always — rows that
        // reported nothing.
        criteria.push(Criterion {
            name: "the_frame_byte_counter_is_not_a_stub",
            claim: "the blit path reports the frame bytes it moves",
            kind: Kind::Counter,
            measured: present_rows
                .iter()
                .filter(|row| row.path == "blit" && row.cpu_bytes == 0)
                .count() as f64,
            bound: 1.0,
            unit: "rows reporting no bytes",
        });

        // The partner of `gpu_frames_are_not_read_back`, in the shape of
        // `the_frame_byte_counter_is_not_a_stub`: zero readbacks on the frame path
        // means something only if the counter reports one when a readback does
        // happen. The raster table reads frames back on purpose (§32.4), so a
        // benchmark run that reports none of them has a counter that is not looking.
        criteria.push(Criterion {
            name: "the_readback_counter_is_not_a_stub",
            claim: "reading a frame back is counted as a readback",
            kind: Kind::Counter,
            measured: f64::from(u8::from(raster_readbacks == 0)),
            bound: 1.0,
            unit: "counters not reporting",
        });

        // And the holes still arrive: a faster path that lost them would be a
        // regression against §26.
        criteria.push(Criterion {
            name: "the_gpu_path_keeps_the_holes",
            claim: "external content is still a hole on the GPU path",
            kind: Kind::Counter,
            measured: frames - gpu.holes.min(gpu.frames) as f64,
            bound: 1.0,
            unit: "frames with no hole",
        });
    }

    criteria
}
