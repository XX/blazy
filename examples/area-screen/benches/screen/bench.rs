//! Headless measurements for Phase 0.5.
//!
//! Same method as Phase 0: drive a `TestHarness` through a scripted gesture, time
//! the rewrite passes, and read the counters that say whether a fast frame was fast
//! for the right reason. What is new is the second set of counters — how many areas
//! the screen actually resized — because that is the number a tiling layout can get
//! catastrophically wrong while still looking correct.

use std::cell::Cell;
use std::time::{Duration, Instant};

use area_screen::header::ScaledHeader;
use area_screen::{Screen, ScreenSpec, build_screen};
use bench_utils::criteria::{Criterion, Kind, Outcome, ScenarioRecord, SweepRecord};
use bench_utils::plan;
use blazy::areas::{AreaContent, AreaScreen, Bar, NodeId, ScreenStats};
use blazy::canvas::CanvasLayer;
use blazy::masonry::core::{NewWidget, WidgetId, WindowEvent};
use blazy::masonry::dpi::PhysicalSize;
use blazy::masonry::kurbo::{Axis, Point, Vec2};
use blazy::masonry::testing::TestHarness;
use blazy::masonry::theme::default_property_set;

/// Viewport used for all scenarios. A working screen, not a demo window.
pub(crate) const VIEWPORT: (u32, u32) = (1400, 900);

/// Frames per scenario.
const FRAMES: usize = 120;

/// Frames per scenario in the quick set.
const QUICK_FRAMES: usize = 40;

/// One pan step inside an area, in viewport pixels.
pub(crate) const PAN_STEP: Vec2 = Vec2::new(-6.0, -2.0);

/// Interface scales the scale scenario cycles through.
///
/// Never repeats a value on consecutive steps: `set_ui_scale` ignores a scale equal
/// to the current one, so a cycle with a repeat would quietly measure idle frames.
const SCALES: [f64; 4] = [1.0, 1.25, 1.5, 1.25];

/// How the benchmark was asked to run.
pub struct Options {
    pub areas: usize,
    pub nodes: usize,
    /// Run only the scenarios the criteria are decided on.
    pub quick: bool,
}

impl Options {
    pub(crate) fn frames(&self) -> usize {
        if self.quick { QUICK_FRAMES } else { FRAMES }
    }
}

/// Everything one scenario is judged on.
#[derive(Clone, Copy, Debug, Default)]
struct Snapshot {
    screen: ScreenStats,
    /// Widgets alive across every area's canvas.
    ///
    /// The number that decides claim 1. Summed rather than averaged: what a pass
    /// walks is the total, and an area holding nothing still costs its own visit.
    live: usize,
    /// Nodes laid out across every area, summed over all passes.
    child_layouts: u64,
    /// Regions handed a new size, summed over every area.
    region_resizes: u64,
    /// Regions in areas other than area 0 handed a new size.
    ///
    /// Every scale change in these scenarios is made in area 0, so this is the
    /// leak counter: work that a change should not have been able to reach.
    other_area_region_resizes: u64,
}

/// Result of one scenario.
struct Report {
    name: &'static str,
    frames: usize,
    total: Duration,
    worst: Duration,
    before: Snapshot,
    after: Snapshot,
    /// Most draw commands the window's layer plan held in any frame (§31).
    ///
    /// The window's total, which is the unit this cost belongs in: the paint pass
    /// rebuilds one plan for the whole window every frame, so an area holding a huge
    /// scene charges its neighbours for it exactly as an area holding widgets does —
    /// and no single canvas can see the sum.
    commands: usize,
}

impl Report {
    fn mean_ms(&self) -> f64 {
        self.total.as_secs_f64() * 1000.0 / self.frames as f64
    }

    fn worst_ms(&self) -> f64 {
        self.worst.as_secs_f64() * 1000.0
    }

    fn per_frame(&self, delta: u64) -> f64 {
        delta as f64 / self.frames as f64
    }

    fn area_resizes_per_frame(&self) -> f64 {
        self.per_frame(self.after.screen.counters.area_resizes - self.before.screen.counters.area_resizes)
    }

    fn region_resizes_per_frame(&self) -> f64 {
        self.per_frame(self.after.region_resizes - self.before.region_resizes)
    }

    fn other_area_region_resizes_per_frame(&self) -> f64 {
        self.per_frame(self.after.other_area_region_resizes - self.before.other_area_region_resizes)
    }

    fn child_layouts_per_frame(&self) -> f64 {
        self.per_frame(self.after.child_layouts - self.before.child_layouts)
    }

    fn print(&self) {
        println!(
            "{:<26} {:>7.3} ms/frame  worst {:>7.3} ms  area-resizes/frame {:>5.2}  \
             region-resizes/frame {:>5.2}  live {:>4}  node-layouts/frame {:>5.1}",
            self.name,
            self.mean_ms(),
            self.worst_ms(),
            self.area_resizes_per_frame(),
            self.region_resizes_per_frame(),
            self.after.live,
            self.child_layouts_per_frame(),
        );
    }

    fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: self.name,
            frames: self.frames,
            mean_ms: self.mean_ms(),
            worst_ms: self.worst_ms(),
            materialised: self.after.live,
            detail: format!("{} areas", self.after.screen.areas),
            child_layouts_per_frame: self.child_layouts_per_frame(),
            builds_per_frame: self.area_resizes_per_frame(),
            far_repaints_per_frame: self.region_resizes_per_frame(),
            extra: vec![(
                "other_area_region_resizes_per_frame",
                self.other_area_region_resizes_per_frame(),
            )],
        }
    }
}

/// Reads the screen's counters and sums its areas' and regions'.
fn snapshot(harness: &TestHarness<Screen>) -> Snapshot {
    let screen = harness.root_widget().stats();
    let mut live = 0;
    let mut child_layouts = 0;
    let mut region_resizes = 0;
    let mut other_area_region_resizes = 0;
    for (area, id) in area_ids(harness).into_iter().enumerate() {
        let stats = canvas_of(harness, area).stats();
        live += stats.materialised;
        child_layouts += stats.counters.child_layouts;

        let resizes = content(harness, id).counters().resizes;
        region_resizes += resizes;
        if area != 0 {
            other_area_region_resizes += resizes;
        }
    }
    Snapshot {
        screen,
        live,
        child_layouts,
        region_resizes,
        other_area_region_resizes,
    }
}

fn area_ids(harness: &TestHarness<Screen>) -> Vec<WidgetId> {
    harness.root_widget().area_ids()
}

/// The region stack filling one area.
fn content(harness: &TestHarness<Screen>, id: WidgetId) -> blazy::masonry::core::WidgetRef<'_, AreaContent> {
    harness
        .get_widget_with_id(id)
        .downcast::<AreaContent>()
        .expect("every area holds a region stack")
}

/// The widget id of one region inside one area.
fn region_id(harness: &TestHarness<Screen>, area: usize, region: usize) -> WidgetId {
    let area_id = area_ids(harness)[area];
    content(harness, area_id).region_ids()[region]
}

/// The canvas of an area: the last region, whatever else the area carries.
fn canvas_of(harness: &TestHarness<Screen>, area: usize) -> blazy::masonry::core::WidgetRef<'_, CanvasLayer> {
    let area_id = area_ids(harness)[area];
    let id = *content(harness, area_id)
        .region_ids()
        .last()
        .expect("an area has regions");
    harness
        .get_widget_with_id(id)
        .downcast::<CanvasLayer>()
        .expect("the main region is a canvas")
}

/// The scale an area's header last laid itself out at.
fn header_seen(harness: &TestHarness<Screen>, area: usize) -> f64 {
    let id = region_id(harness, area, 0);
    harness
        .get_widget_with_id(id)
        .downcast::<ScaledHeader>()
        .expect("region 0 is a header")
        .seen_scale()
}

/// Sets the interface scale of an area's header region.
fn set_header_scale(harness: &mut TestHarness<Screen>, area: usize, scale: f64) {
    let id = area_ids(harness)[area];
    harness.edit_widget_with_id(id, |mut widget| {
        let mut content = widget.downcast::<AreaContent>();
        AreaContent::set_ui_scale(&mut content, 0, scale);
    });
}

fn new_harness(areas: usize, nodes: usize) -> TestHarness<Screen> {
    let (screen, _graph) = build_screen(areas, nodes, None);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(screen),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    // Settle the first layout and paint, so the initial sizing of every area is not
    // counted as a resize caused by the gesture under test.
    let _ = harness.redraw();
    harness
}

/// A screen of one region per area: the canvas, with no header above it.
fn headerless_harness(areas: usize, nodes: usize) -> TestHarness<Screen> {
    let (screen, _graph) = ScreenSpec::new(areas, nodes).without_header().build();
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(screen),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    harness
}

/// Times `frames` iterations of `step`, each followed by a full redraw.
fn measure(
    name: &'static str,
    harness: &mut TestHarness<Screen>,
    frames: usize,
    mut step: impl FnMut(&mut TestHarness<Screen>, usize),
) -> Report {
    let before = snapshot(harness);
    let mut total = Duration::ZERO;
    let mut worst = Duration::ZERO;

    let mut commands = 0;
    for i in 0..frames {
        let start = Instant::now();
        step(harness, i);
        let (layers, _) = harness.redraw();
        let elapsed = start.elapsed();
        // Outside the clock: reading the plan is the measurement, not the frame.
        commands = commands.max(plan::commands(&layers));
        total += elapsed;
        worst = worst.max(elapsed);
    }

    Report {
        name,
        frames,
        total,
        worst,
        before,
        after: snapshot(harness),
        commands,
    }
}

/// The splitter dividing the smallest span, i.e. one between two leaf areas.
fn leaf_bar(harness: &TestHarness<Screen>) -> Option<Bar> {
    harness
        .root_widget()
        .bars()
        .iter()
        .min_by(|a, b| span_area(a).total_cmp(&span_area(b)))
        .copied()
}

/// The splitter dividing the largest span, i.e. the root of the tree.
fn root_bar(harness: &TestHarness<Screen>) -> Option<Bar> {
    harness
        .root_widget()
        .bars()
        .iter()
        .max_by(|a, b| span_area(a).total_cmp(&span_area(b)))
        .copied()
}

fn span_area(bar: &Bar) -> f64 {
    bar.span.width() * bar.span.height()
}

/// Drags `split` back and forth about `base`, one pixel per frame.
///
/// One pixel because the split tree rounds a ratio to whole pixels: a sub-pixel
/// step would leave every rect unchanged and the scenario would measure an idle
/// screen while looking like a drag.
fn drag_step(harness: &mut TestHarness<Screen>, split: NodeId, base: Point, axis: Axis, i: usize) {
    let offset = ((i % 40) as f64) - 20.0;
    let pos = match axis {
        Axis::Horizontal => Point::new(base.x + offset, base.y),
        Axis::Vertical => Point::new(base.x, base.y + offset),
    };
    harness.edit_root_widget(|mut screen| AreaScreen::drag_bar(&mut screen, split, pos));
}

/// Pans the canvas in area `area` by one step.
pub(crate) fn pan_area(harness: &mut TestHarness<Screen>, area: usize, delta: Vec2) {
    let id = canvas_of(harness, area).ctx().widget_id();
    harness.edit_widget_with_id(id, |mut widget| {
        let mut canvas = widget.downcast::<CanvasLayer>();
        CanvasLayer::pan(&mut canvas, delta);
    });
}

/// Zooms the canvas in area `area` about its centre.
pub(crate) fn zoom_area(harness: &mut TestHarness<Screen>, area: usize, factor: f64) {
    let id = canvas_of(harness, area).ctx().widget_id();
    harness.edit_widget_with_id(id, |mut widget| {
        let mut canvas = widget.downcast::<CanvasLayer>();
        CanvasLayer::zoom_around(&mut canvas, Point::new(200.0, 150.0), factor);
    });
}

/// Widgets in the whole window — what a frame walks (§20.2).
///
/// Counted from the tree rather than summed from the canvases' own numbers, because
/// the claim is about the window and every area contributes its own furniture to it.
fn widgets_in_window(harness: &mut TestHarness<Screen>) -> usize {
    let mut widgets = 0;
    harness.inspect_widgets(|_| widgets += 1);
    widgets
}

/// Every area zoomed out to an overview, and work going on in one of them.
///
/// The scenario the reconnaissance in §29.1 was written from, at its worst point. An
/// area zoomed out holds its largest tree and pays nothing for it — no layout, no
/// rebuild, no repaint — while every frame the *other* areas cause walks it anyway.
/// Without a shared ceiling this is the arithmetic that breaks the screen: eight areas
/// each honestly inside a canvas-sized budget put eight of them in one window.
///
/// Returns the widgets held, the draw commands in the window's plan, and the mean and
/// worst frame while panning in area 0.
fn overview_screen(opts: &Options, areas: usize, nodes: usize) -> (usize, usize, f64, f64) {
    let frames = if opts.quick { 20 } else { 40 };
    let mut harness = new_harness(areas, nodes);
    for area in 0..areas {
        zoom_area(&mut harness, area, 0.06);
    }
    let _ = harness.redraw();

    let report = measure("overview in every area", &mut harness, frames, |h, _| {
        pan_area(h, 0, PAN_STEP);
    });
    let widgets = widgets_in_window(&mut harness);
    println!(
        "\nwindow budget: every area at zoom 0.06, panning in one\n  \
         {widgets} widgets and {} draw commands in the window, {:.3} ms/frame, \
         worst {:.3} ms  (live nodes {})",
        report.commands,
        report.mean_ms(),
        report.worst_ms(),
        report.after.live,
    );
    (widgets, report.commands, report.mean_ms(), report.worst_ms())
}

/// Runs the scenarios, prints the numbers, and returns the evaluated criteria.
pub fn run(opts: &Options) -> Outcome {
    let areas = opts.areas.max(1);
    let nodes = opts.nodes;
    let frames = opts.frames();
    println!(
        "blazy Phase 0.5/0.6 - area screen on masonry_core@main\n\
         {areas} areas, {nodes} nodes shared between them, viewport {}x{}, \
         {frames} frames per scenario{}\n",
        VIEWPORT.0,
        VIEWPORT.1,
        if opts.quick { " (quick set)" } else { "" }
    );

    let mut reports = Vec::new();
    // Assigned by the scale scenario below, which always runs: the criterion it feeds
    // is one of the two Phase 0.6 exists to check, so there is no quick set without it.
    let scale_misses: u64;

    // --- Scenario 1: idle.
    //
    // Nothing changes. Areas exist as data whether or not anything happens to them,
    // and this is where a screen that recomputes rects into real layout work would
    // show it.
    {
        let mut harness = new_harness(areas, nodes);
        reports.push(measure("idle", &mut harness, frames, |_, _| {}));
    }

    // --- Scenario 2: drag a splitter between two leaf areas.
    //
    // The claim under test, and the common gesture: nudging the boundary between
    // two panes. Only those two rects change, so only those two areas may re-run
    // layout, no matter how many areas the screen holds.
    {
        let mut harness = new_harness(areas, nodes);
        if let Some(bar) = leaf_bar(&harness) {
            let base = bar.rect.center();
            let (split, axis) = (bar.split, bar.axis);
            reports.push(measure("drag leaf splitter", &mut harness, frames, |h, i| {
                drag_step(h, split, base, axis, i);
            }));
        }
    }

    // --- Scenario 3: drag the root splitter.
    //
    // The worst case, and not a defect: moving the boundary between the two halves
    // of the screen changes the rect of every area in both halves, so every one of
    // them has to be re-laid-out. Measured so the cost of the worst case is a number
    // rather than an assumption, and so the gap to the leaf case is visible.
    {
        let mut harness = new_harness(areas, nodes);
        if let Some(bar) = root_bar(&harness) {
            let base = bar.rect.center();
            let (split, axis) = (bar.split, bar.axis);
            reports.push(measure("drag root splitter", &mut harness, frames, |h, i| {
                drag_step(h, split, base, axis, i);
            }));
        }
    }

    // --- Scenario 4: pan inside one area.
    //
    // An area is a viewport onto its own content. Panning in one must not touch the
    // others — if it does, areas are not independent and the whole subsystem is a
    // shared mutable surface pretending to be a tiling.
    {
        let mut harness = new_harness(areas, nodes);
        reports.push(measure("pan in one area", &mut harness, frames, |h, _| {
            pan_area(h, 0, PAN_STEP);
        }));
    }

    // --- Scenario 5: change the interface scale of one region.
    //
    // §9's first rule: `ui_scale` is a layout input. The scale is changed in area 0's
    // header only, so everything this scenario counts outside area 0 is a leak, and
    // the header is asked afterwards what scale it actually laid out at — a property
    // that reaches the widget but not its layout would otherwise look like success.
    {
        let mut harness = new_harness(areas, nodes);
        let missed = Cell::new(0u64);
        let expected = Cell::new(f64::NAN);
        reports.push(measure("change region ui_scale", &mut harness, frames, |h, i| {
            let want = expected.get();
            if want.is_finite() && header_seen(h, 0) != want {
                missed.set(missed.get() + 1);
            }
            let next = SCALES[i % SCALES.len()];
            set_header_scale(h, 0, next);
            expected.set(next);
        }));
        if header_seen(&harness, 0) != expected.get() {
            missed.set(missed.get() + 1);
        }
        scale_misses = missed.get();
    }

    // --- Scenario 6: zoom the content of one region.
    //
    // §9's second rule, and the one that decides whether the two knobs stayed apart:
    // `view` is a transform at composition time and must cost no layout at all.
    // Measured at the region level rather than inside the canvas, because the claim
    // being checked here is that nothing in the region stack was tempted to treat a
    // zoom as a resize.
    {
        let mut harness = new_harness(areas, nodes);
        reports.push(measure("zoom content in one region", &mut harness, frames, |h, i| {
            // Oscillate inside one detail level: crossing an LOD threshold is
            // supposed to cost a re-layout, and that is the canvas's business, not
            // the region's.
            let factor = if (i / 20).is_multiple_of(2) { 0.995 } else { 1.0 / 0.995 };
            zoom_area(h, 0, factor);
        }));
    }

    // --- Scenario 7: resize the window. Informational.
    //
    // Every rect changes, so every area re-lays-out; there is no way around that and
    // no criterion to attach. What the number is worth is knowing whether a window
    // drag stays interactive with a screen full of editors.
    if !opts.quick {
        let mut harness = new_harness(areas, nodes);
        reports.push(measure("resize the window", &mut harness, frames.min(40), |h, i| {
            let w = VIEWPORT.0 - (i % 40) as u32;
            h.process_window_event(WindowEvent::Resize(PhysicalSize::new(w, VIEWPORT.1)));
        }));
    }

    println!();
    for report in &reports {
        report.print();
    }

    let sweep = area_sweep(opts, nodes);
    let regions = region_cost(opts, areas, nodes);
    let (overview_widgets, overview_commands, ..) = overview_screen(opts, areas, nodes);
    let cache = crate::cache::cache_table(opts, areas, nodes);
    let operations = crate::ops::ops_table(opts, areas, nodes);
    // Two windows over one graph: what they cost each other, and what one changes in the
    // other (the detach task, phase 1).
    let windows = crate::windows::window_table(opts, areas.min(4), nodes);
    let cross = crate::windows::cross_window(areas.min(4), nodes);
    let detach = crate::windows::detach_row(areas.min(4), nodes);

    let outcome = Outcome {
        nodes: areas,
        viewport: VIEWPORT,
        quick: opts.quick,
        criteria: {
            let mut criteria = crate::windows::criteria(&windows, &cross);
            criteria.extend(crate::windows::detach_criteria(&detach));
            criteria.extend(evaluate(&Measured {
                reports: &reports,
                sweep: &sweep,
                scale_misses,
                regions,
                overview_widgets,
                overview_commands,
                areas,
                cache: &cache,
                operations: &operations,
            }));
            criteria
        },
        scenarios: reports
            .iter()
            .map(Report::record)
            .chain(windows.iter().map(crate::windows::WindowRow::record))
            .chain(std::iter::once(cross.record()))
            .chain(std::iter::once(detach.record()))
            .collect(),
        sweep,
        zoom_sweep: Vec::new(),
    };
    outcome.report("Phase 0.5/0.6 criteria");
    outcome
}

/// Measures frame cost and live widget count against the number of areas.
///
/// The sweep that decides claim 1. If splitting a window merely divides one
/// viewport, the live count stays close to flat; if each area is a viewport of its
/// own that materialises its own share, the count climbs with the tiling.
///
/// Deliberately headerless, unlike every other scenario here. With headers, more
/// areas means more header strips means less canvas, and the live count would fall
/// for a reason that has nothing to do with the claim — a confound that would make
/// the sweep look like better evidence than it is. What a region costs is measured
/// separately, by [`region_cost`].
fn area_sweep(opts: &Options, nodes: usize) -> Vec<SweepRecord> {
    const COUNTS: [usize; 5] = [1, 2, 4, 8, 16];
    const QUICK_COUNTS: [usize; 2] = [1, 16];

    let counts: &[usize] = if opts.quick { &QUICK_COUNTS } else { &COUNTS };
    let sweep_frames = if opts.quick { 25 } else { 40 };

    println!("\nscaling: frame cost vs area count (one window, one graph)");
    let mut points = Vec::new();

    for &areas in counts {
        let mut harness = headerless_harness(areas, nodes);
        let idle = measure("idle", &mut harness, sweep_frames, |_, _| {});

        let mut harness = headerless_harness(areas, nodes);
        let pan = measure("pan", &mut harness, sweep_frames, |h, _| pan_area(h, 0, PAN_STEP));

        let point = SweepRecord {
            nodes: areas,
            visible: pan.after.live,
            idle_ms: idle.mean_ms(),
            pan_ms: pan.mean_ms(),
        };
        println!(
            "  {:>3} areas ({:>4} live widgets)  idle {:>7.3} ms  pan in one area {:>7.3} ms",
            point.nodes, point.visible, point.idle_ms, point.pan_ms,
        );
        points.push(point);
    }

    points
}

/// What a second region per area costs, at idle.
///
/// Returns (one region per area, two regions per area) in milliseconds. A region is a
/// widget like any other, so it cannot be free; the question is whether it is priced
/// like a widget or like a viewport.
fn region_cost(opts: &Options, areas: usize, nodes: usize) -> (f64, f64) {
    let frames = if opts.quick { 25 } else { 40 };
    let mut bare = headerless_harness(areas, nodes);
    let without = measure("idle", &mut bare, frames, |_, _| {}).mean_ms();

    let mut full = new_harness(areas, nodes);
    let with = measure("idle", &mut full, frames, |_, _| {}).mean_ms();

    println!(
        "\nregions: idle with {areas} areas   1 region each {without:>7.3} ms   \
         2 regions each {with:>7.3} ms"
    );
    (without, with)
}

/// The Phase 0.5 and 0.6 criteria, evaluated against the numbers just measured.
/// Everything the criteria are decided from.
///
/// A struct rather than eight arguments, for the reason the host benchmark has one
/// (§26.4): a criterion list that grows an argument per measurement stops being
/// readable long before the compiler complains.
struct Measured<'a> {
    reports: &'a [Report],
    sweep: &'a [SweepRecord],
    /// `ui_scale` changes a region's layout never saw.
    scale_misses: u64,
    /// Idle milliseconds with one region per area and with two.
    regions: (f64, f64),
    /// Widgets and draw commands in the window at an overview zoom.
    overview_widgets: usize,
    overview_commands: usize,
    areas: usize,
    /// What the layer cache did, row by row (§36).
    cache: &'a [crate::cache::CacheRow],
    /// What each operation on the areas cost (§41.2).
    operations: &'a [crate::ops::OpsRow],
}

fn evaluate(measured: &Measured<'_>) -> Vec<Criterion> {
    let Measured {
        reports,
        sweep,
        scale_misses,
        regions,
        overview_widgets,
        overview_commands,
        areas,
        cache,
        operations,
    } = *measured;
    let find = |name: &str| reports.iter().find(|r| r.name == name);
    let mut criteria = Vec::new();

    if let Some(idle) = find("idle") {
        criteria.push(Criterion {
            name: "idle_screen_resizes_nothing",
            claim: "an idle screen resizes no area",
            kind: Kind::Counter,
            measured: idle.area_resizes_per_frame(),
            bound: 0.05,
            unit: "area resizes/frame",
        });
    }

    if let Some(drag) = find("drag leaf splitter") {
        // Two areas share the bar; a third would mean the screen re-lays-out things
        // the drag did not move.
        criteria.push(Criterion {
            name: "leaf_splitter_drag_resizes_two_areas",
            claim: "dragging a leaf splitter resizes two areas",
            kind: Kind::Counter,
            measured: drag.area_resizes_per_frame(),
            bound: 2.5,
            unit: "area resizes/frame",
        });
    }

    if let Some(pan) = find("pan in one area") {
        criteria.push(Criterion {
            name: "pan_in_one_area_resizes_no_area",
            claim: "panning inside an area resizes no area",
            kind: Kind::Counter,
            measured: pan.area_resizes_per_frame(),
            bound: 0.05,
            unit: "area resizes/frame",
        });
    }

    if let (Some(first), Some(last)) = (sweep.first(), sweep.last())
        && first.nodes != last.nodes
    {
        let area_ratio = last.nodes as f64 / first.nodes as f64;

        // Claim 1 as a counter. More areas means smaller areas, so the live set is
        // bounded by the window. It does grow a little: a node straddling a boundary
        // is materialised on both sides of it, and every area carries a margin of
        // its own — which is why the bound is a small multiple rather than equality.
        criteria.push(Criterion {
            name: "live_widgets_bounded_by_window_not_area_count",
            claim: "live widgets do not grow with the area count",
            kind: Kind::Counter,
            measured: last.visible as f64,
            bound: first.visible as f64 * 4.0,
            unit: "widgets in tree",
        });

        // The timing form of the same claim. Gated with a wide margin for the reason
        // the criteria module gives: it compares two times from one process, and the
        // counter above would catch the same regression first.
        criteria.push(Criterion {
            name: "frame_cost_independent_of_area_count",
            claim: "frame cost does not follow the area count",
            kind: Kind::Timing,
            measured: last.idle_ms / first.idle_ms,
            bound: area_ratio * 0.5,
            unit: "x slower",
        });
    }

    // --- Phase 0.6: regions and ui_scale.

    if let Some(scale) = find("change region ui_scale") {
        // The positive claim, counted from the failing side so it can be bounded from
        // above like everything else: a scale the header did not lay out at is a
        // scale that never reached layout.
        criteria.push(Criterion {
            name: "ui_scale_reaches_the_regions_layout",
            claim: "every ui_scale change reaches the region's layout",
            kind: Kind::Counter,
            measured: scale_misses as f64,
            bound: 0.5,
            unit: "changes not seen",
        });

        // Containment upwards: a region resizing itself must not push the area around.
        criteria.push(Criterion {
            name: "ui_scale_change_does_not_resize_areas",
            claim: "changing ui_scale resizes no area",
            kind: Kind::Counter,
            measured: scale.area_resizes_per_frame(),
            bound: 0.05,
            unit: "area resizes/frame",
        });

        // Containment sideways: the change is made in area 0 and nowhere else.
        criteria.push(Criterion {
            name: "ui_scale_change_stays_in_its_area",
            claim: "changing ui_scale does not reach other areas",
            kind: Kind::Counter,
            measured: scale.other_area_region_resizes_per_frame(),
            bound: 0.05,
            unit: "foreign region resizes/frame",
        });
    }

    if let Some(zoom) = find("zoom content in one region") {
        // §9's second rule. A zoom that resizes a region is a zoom that has been
        // confused with a scale, and it would cost a re-layout on every frame.
        criteria.push(Criterion {
            name: "content_zoom_resizes_no_region",
            claim: "zooming content resizes no region",
            kind: Kind::Counter,
            measured: zoom.region_resizes_per_frame(),
            bound: 0.05,
            unit: "region resizes/frame",
        });
    }

    let (without, with) = regions;
    if without > 0.0 {
        criteria.push(Criterion {
            name: "a_second_region_is_priced_like_a_widget",
            claim: "a second region per area does not double idle cost",
            kind: Kind::Timing,
            measured: with / without,
            bound: 2.0,
            unit: "x slower",
        });
    }

    // --- The window's share of the widget budget (§29).
    //
    // A canvas budget is a per-canvas number, and a screen of areas is where that
    // stops adding up: what a frame walks is the window's tree. Every area is at the
    // zoom that holds the most, so this is the worst the screen can be asked for.
    // The bound is the window budget with room for the furniture — headers, region
    // stacks and the split tree itself, which is a handful of widgets per area.
    criteria.push(Criterion {
        name: "the_window_stays_within_one_widget_budget",
        claim: "areas share one widget budget rather than one each",
        kind: Kind::Counter,
        measured: overview_widgets as f64,
        bound: blazy::canvas::DEFAULT_WIDGET_BUDGET as f64 + 10.0 * areas as f64,
        unit: "widgets in the window",
    });

    // --- The other half of the same frame (§31).
    //
    // Widgets are not what an overview area holds: below the far-field threshold it
    // holds none and still fills the plan with the scene it painted. Before batching,
    // eight areas at this zoom put 9384 rectangles and 18 768 curves into the window's
    // plan and cost 13.12 ms a frame with 33 widgets in the tree — a ceiling in widgets
    // cannot see that, and this is the counter that can.
    //
    // The bound is per area rather than absolute: what an area draws is a handful of
    // commands per colour it uses, so the honest claim is that the window costs the
    // number of *styles* on screen and not the number of nodes.
    criteria.push(Criterion {
        name: "the_window_plan_costs_styles_not_nodes",
        claim: "an overview area fills the plan with colours, not with nodes",
        kind: Kind::Counter,
        measured: overview_commands as f64,
        bound: 64.0 * areas as f64,
        unit: "draw commands in the window",
    });

    // --- §36: keeping the pixels of areas that did not change.
    //
    // Counted in layers rather than in milliseconds, for the usual reason (§20.9): a
    // layer either was rasterised or was not, on any machine. Absent where there is no
    // graphics device, like every GPU claim here (§27.5).
    let cached_row = |what: &str| cache.iter().find(|row| row.cached && row.what == what);
    if let Some(idle) = cached_row("nothing changes") {
        criteria.push(Criterion {
            name: "an_idle_screen_rasterises_no_area",
            claim: "a frame in which nothing changed draws no area",
            kind: Kind::Counter,
            measured: idle.drawn,
            bound: 0.5,
            unit: "areas drawn/frame",
        });
    }
    if let Some(one) = cached_row("one area pans") {
        // The claim §7.3 wrote down and §36 measured: dragging in one editor leaves the
        // other seven alone. Two rather than one as the bound, because the number that
        // matters is "not eight".
        criteria.push(Criterion {
            name: "a_working_area_does_not_redraw_its_neighbours",
            claim: "panning one area of eight draws one area",
            kind: Kind::Counter,
            measured: one.drawn,
            bound: 2.0,
            unit: "areas drawn/frame",
        });

        // And nothing falls between the two stools: every area is either kept or drawn.
        criteria.push(Criterion {
            name: "every_area_is_accounted_for",
            claim: "each area is either kept or drawn, every frame",
            kind: Kind::Counter,
            measured: (areas as f64 - (one.reused + one.drawn)).abs(),
            bound: 0.5,
            unit: "areas unaccounted for",
        });
    }
    if let Some(idle) = cached_row("nothing changes") {
        // Why the cache pays for itself (§36.3), as a counter: comparing scenes is
        // cheap, finding out where a layer sits walks its whole scene, and a layer that
        // did not change already has a rectangle. Compute the rectangle first and this
        // is eight per frame on an idle screen.
        criteria.push(Criterion {
            name: "an_idle_screen_walks_no_layer",
            claim: "a frame in which nothing changed touches no layer's geometry",
            kind: Kind::Counter,
            measured: idle.walks,
            bound: 0.5,
            unit: "layers walked/frame",
        });
    }
    if !cache.is_empty() {
        // The partner of the one above, from the failing side: a sweep where the cache
        // never walks anything cannot say the walk is being avoided.
        criteria.push(Criterion {
            name: "the_layer_walk_is_exercised",
            claim: "the sweep contains a frame in which layers are walked",
            kind: Kind::Counter,
            measured: f64::from(u8::from(!cache.iter().any(|row| row.cached && row.walks > 0.5))),
            bound: 1.0,
            unit: "sweeps that never walk",
        });
    }

    if !cache.is_empty() {
        // §37.2: the cache is bounded. Cached layers tile the window, so what they hold
        // adds up to about one frame however many of them there are — but a caller that
        // registers more than that, or a window that has just resized, must not be able
        // to grow it without limit.
        criteria.push(Criterion {
            name: "the_layer_cache_stays_inside_its_ceiling",
            claim: "the cache never holds more texture than its ceiling",
            kind: Kind::Counter,
            measured: cache
                .iter()
                .filter(|row| row.cached)
                .map(|row| row.kib.saturating_sub(row.budget_kib))
                .max()
                .unwrap_or(0) as f64,
            bound: 1.0,
            unit: "KiB over the ceiling",
        });

        // The cache promises a rectangle and then copies it; between the two it may
        // evict. What it must never evict is a layer *this frame* is copying, whose
        // pixels are in the cache and nowhere else — the copy then finds nothing, the
        // area is empty for a frame, and `reused` has already been counted, so every
        // other number here says the cache did its job (§44.9).
        criteria.push(Criterion {
            name: "the_cache_keeps_the_pixels_it_promised",
            claim: "a layer the cache said it would copy was there to copy",
            kind: Kind::Counter,
            measured: cache.iter().map(|row| row.dropped).fold(0.0, f64::max),
            bound: 1.0,
            unit: "kept layers with no pixels/frame",
        });

        // The precondition §36 asks a caller for, in the only half a host can check:
        // layers registered as disjoint have to *be* disjoint. They stopped being so
        // the moment an area painted outside its own box — a selection outline and a
        // status line — and overlapping rectangles also add up to more pixels than the
        // window they tile, which is how the ceiling above came to be exceeded at all.
        criteria.push(Criterion {
            name: "cached_layers_do_not_overlap",
            claim: "layers registered as tiling the window claim disjoint pixels",
            kind: Kind::Counter,
            measured: cache.iter().map(|row| row.overlaps).fold(0.0, f64::max),
            bound: 1.0,
            unit: "layers overlapping another/frame",
        });

        // And the partner: a sweep that never fills the cache says nothing about what
        // happens when it does.
        criteria.push(Criterion {
            name: "the_cache_ceiling_is_exercised",
            claim: "the sweep contains a frame in which the cache had to evict",
            kind: Kind::Counter,
            measured: f64::from(u8::from(!cache.iter().any(|row| row.cached && row.evictions > 0.5))),
            bound: 1.0,
            unit: "sweeps that never evict",
        });
    }
    if let Some(selects) = cache.iter().find(|row| row.cached && row.what == "one area selects") {
        // Where a gesture's pixels land (§38.5). A selection is drawn by the driver
        // inside its own area, so the frame redraws that area and copies the rest —
        // the shape §36 gave this criterion, in the unit §36 counts in. An overlay
        // spanning the window would put every area here instead, and would break the
        // condition the cache cannot check: that a cached layer owns its rectangle.
        criteria.push(Criterion {
            name: "a_selection_repaints_one_area",
            claim: "a selection in one area redraws that area and copies the rest",
            kind: Kind::Counter,
            measured: selects.drawn,
            bound: 2.0,
            unit: "areas drawn/frame",
        });
    }

    if !cache.is_empty() {
        // Both criteria above pass on a sweep where nothing ever changes, so the sweep
        // has to contain a frame in which everything does. Counted from the failing
        // side, as always.
        let redrew_everything = cache.iter().any(|row| row.cached && row.drawn >= areas as f64 - 0.5);
        criteria.push(Criterion {
            name: "the_layer_cache_is_exercised",
            claim: "the sweep contains a frame in which every area is drawn",
            kind: Kind::Counter,
            measured: f64::from(u8::from(!redrew_everything)),
            bound: 1.0,
            unit: "sweeps that never redraw",
        });
    }

    let op = |what: &str| operations.iter().find(|row| row.what == what);

    if let Some(join) = op("join") {
        // The counter the splitter drag is judged on, asked of a join: the parent split
        // disappears and the survivor takes the rectangle the pair shared, so exactly one
        // area changes size however many the screen holds.
        criteria.push(Criterion {
            name: "join_resizes_only_the_survivor",
            claim: "joining two areas resizes one",
            kind: Kind::Counter,
            measured: join.resizes as f64,
            bound: 2.0,
            unit: "area resizes",
        });
    }

    if !operations.is_empty() {
        // The requirement of §41.2, and not an optimisation: an area that survived an
        // operation has to come out of it as the same widget, because its view, its
        // selection and its materialised nodes live nowhere else (§30). Split is the one
        // operation that may build, and it is excluded here and guarded below.
        let rebuilt: u64 = operations
            .iter()
            .filter(|row| row.what != "split")
            .map(|row| row.builds)
            .sum();
        criteria.push(Criterion {
            name: "no_operation_rebuilds_a_surviving_area",
            claim: "join, swap, maximize, restore and a workspace load build nothing",
            kind: Kind::Counter,
            measured: rebuilt as f64,
            bound: 1.0,
            unit: "area widgets built",
        });

        // Vacuity: a screen where nothing is ever built passes the criterion above by
        // doing nothing at all.
        criteria.push(Criterion {
            name: "the_table_contains_an_operation_that_builds",
            claim: "splitting an area builds a widget for it",
            kind: Kind::Counter,
            measured: f64::from(u8::from(op("split").is_none_or(|row| row.builds == 0))),
            bound: 1.0,
            unit: "tables where a split built nothing",
        });
    }

    if let Some(restore) = op("maximize and restore") {
        // A return has to be exact, not approximate (§28.4 on sweeps that only go one
        // way): the screen comes back to the rectangles it left, or the flag is not a
        // flag but a rebuild.
        criteria.push(Criterion {
            name: "maximize_and_restore_return_the_same_screen",
            claim: "restoring gives back the rectangles maximizing hid",
            kind: Kind::Counter,
            measured: restore.mismatched as f64,
            bound: 1.0,
            unit: "areas back at the wrong size",
        });

        // Vacuity: restoring from a maximize that changed nothing proves nothing.
        criteria.push(Criterion {
            name: "the_table_contains_a_maximize_that_changed_the_screen",
            claim: "maximizing an area resizes it",
            kind: Kind::Counter,
            measured: f64::from(u8::from(op("maximize").is_none_or(|row| row.resizes == 0))),
            bound: 1.0,
            unit: "tables where maximizing changed nothing",
        });
    }

    if let Some(trip) = op("workspace round trip") {
        // Tree → file → tree, over a screen that moved on in between (see `ops.rs`): the
        // areas have to arrive at the rectangles the file was written from, which means
        // the same ids as well as the same geometry — the widgets are keyed by the ids.
        criteria.push(Criterion {
            name: "a_workspace_round_trip_puts_every_area_back",
            claim: "a workspace read back puts every area where it was written",
            kind: Kind::Counter,
            measured: trip.mismatched as f64,
            bound: 1.0,
            unit: "areas in the wrong place",
        });
    }

    // The set of layers is the set of areas, so an operation that changes the areas
    // changes what the cache is holding. That it stays inside its ceiling while doing so
    // is *not* a criterion of its own: `the_layer_cache_stays_inside_its_ceiling` takes
    // the worst of every cached row, and the "join, small ceiling" row exists to put a
    // changing area set among them — a second criterion over the same rows would only
    // look like more evidence. What is new is the other half.
    if let Some(joined) = cache.iter().find(|row| row.what == "join" && row.cached) {
        criteria.push(Criterion {
            name: "a_join_does_not_stop_the_layer_cache_reusing",
            claim: "the areas a join did not touch keep their pixels",
            kind: Kind::Counter,
            measured: joined.layers as f64 - joined.reused,
            bound: 3.0,
            unit: "layers drawn rather than kept/frame",
        });
    }

    criteria
}
