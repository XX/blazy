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
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Rect, Size};
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
fn presentation_table(opts: &Options) {
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
    let frames = 10;

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
            let mut cpu_bytes = 0;
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
                cpu_bytes = u64::from(frame.image.width) * u64::from(frame.image.height) * 4;
            }
            rows.push(PresentRow {
                path: "blit",
                size: (width, height),
                scale,
                draw_ms: draw.as_secs_f64() * 1000.0 / f64::from(frames),
                show_ms: show.as_secs_f64() * 1000.0 / f64::from(frames),
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
                    draw_ms: draw.as_secs_f64() * 1000.0 / f64::from(frames),
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

    presentation_table(opts);
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
        criteria: evaluate(
            &reports[external],
            &backends,
            scale_layouts.get(),
            &scale_frames,
            transparent,
            gpu,
        ),
        scenarios: reports.iter().map(Report::record).collect(),
        sweep: Vec::new(),
    };
    outcome.report("blazy-shell criteria");
    outcome
}

/// The criteria, evaluated against what was just measured.
///
/// Three of the four the task set; the fourth — that the crates below the host do not
/// depend on a window — is a fact about the dependency graph rather than about a
/// frame, and is checked by `cargo make deps-rule` instead (§26.4).
fn evaluate(
    external: &Report,
    backends: &BackendReport,
    scale_layouts: u64,
    scale_frames: &[(f64, u32, f64)],
    transparent: usize,
    gpu: Option<blazy_shell::PresentCounters>,
) -> Vec<Criterion> {
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

    // --- The GPU frame path (§27). Absent where there is no device to measure it on,
    // which is the honest answer on a runner without one.
    if let Some(gpu) = gpu {
        let frames = gpu.frames.max(1) as f64;

        // What the task is for: the frame is drawn and shown without ever existing as
        // bytes in main memory. The blit path in the same table moves megabytes.
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
