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
use area_screen::{build_screen, build_screen_with};
use bench_utils::criteria::{Criterion, Kind, Outcome, ScenarioRecord, SweepRecord};
use blazy_areas::{AreaContent, AreaScreen, Bar, NodeId, ScreenStats};
use blazy_canvas::CanvasLayer;
use masonry::core::{NewWidget, WidgetId, WindowEvent};
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Axis, Point, Vec2};
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;

/// Viewport used for all scenarios. A working screen, not a demo window.
const VIEWPORT: (u32, u32) = (1400, 900);

/// Frames per scenario.
const FRAMES: usize = 120;

/// Frames per scenario in the quick set.
const QUICK_FRAMES: usize = 40;

/// One pan step inside an area, in viewport pixels.
const PAN_STEP: Vec2 = Vec2::new(-6.0, -2.0);

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
    fn frames(&self) -> usize {
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
        }
    }
}

/// Reads the screen's counters and sums its areas' and regions'.
fn snapshot(harness: &TestHarness<AreaScreen>) -> Snapshot {
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

fn area_ids(harness: &TestHarness<AreaScreen>) -> Vec<WidgetId> {
    harness.root_widget().area_ids()
}

/// The region stack filling one area.
fn content(harness: &TestHarness<AreaScreen>, id: WidgetId) -> masonry::core::WidgetRef<'_, AreaContent> {
    harness
        .get_widget_with_id(id)
        .downcast::<AreaContent>()
        .expect("every area holds a region stack")
}

/// The widget id of one region inside one area.
fn region_id(harness: &TestHarness<AreaScreen>, area: usize, region: usize) -> WidgetId {
    let area_id = area_ids(harness)[area];
    content(harness, area_id).region_ids()[region]
}

/// The canvas of an area: the last region, whatever else the area carries.
fn canvas_of(harness: &TestHarness<AreaScreen>, area: usize) -> masonry::core::WidgetRef<'_, CanvasLayer> {
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
fn header_seen(harness: &TestHarness<AreaScreen>, area: usize) -> f64 {
    let id = region_id(harness, area, 0);
    harness
        .get_widget_with_id(id)
        .downcast::<ScaledHeader>()
        .expect("region 0 is a header")
        .seen_scale()
}

/// Sets the interface scale of an area's header region.
fn set_header_scale(harness: &mut TestHarness<AreaScreen>, area: usize, scale: f64) {
    let id = area_ids(harness)[area];
    harness.edit_widget_with_id(id, |mut widget| {
        let mut content = widget.downcast::<AreaContent>();
        AreaContent::set_ui_scale(&mut content, 0, scale);
    });
}

fn new_harness(areas: usize, nodes: usize) -> TestHarness<AreaScreen> {
    let (screen, _graph) = build_screen(areas, nodes);
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
fn headerless_harness(areas: usize, nodes: usize) -> TestHarness<AreaScreen> {
    let (screen, _graph) = build_screen_with(areas, nodes, false);
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
    harness: &mut TestHarness<AreaScreen>,
    frames: usize,
    mut step: impl FnMut(&mut TestHarness<AreaScreen>, usize),
) -> Report {
    let before = snapshot(harness);
    let mut total = Duration::ZERO;
    let mut worst = Duration::ZERO;

    for i in 0..frames {
        let start = Instant::now();
        step(harness, i);
        let _ = harness.redraw();
        let elapsed = start.elapsed();
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
    }
}

/// The splitter dividing the smallest span, i.e. one between two leaf areas.
fn leaf_bar(harness: &TestHarness<AreaScreen>) -> Option<Bar> {
    harness
        .root_widget()
        .bars()
        .iter()
        .min_by(|a, b| span_area(a).total_cmp(&span_area(b)))
        .copied()
}

/// The splitter dividing the largest span, i.e. the root of the tree.
fn root_bar(harness: &TestHarness<AreaScreen>) -> Option<Bar> {
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
fn drag_step(harness: &mut TestHarness<AreaScreen>, split: NodeId, base: Point, axis: Axis, i: usize) {
    let offset = ((i % 40) as f64) - 20.0;
    let pos = match axis {
        Axis::Horizontal => Point::new(base.x + offset, base.y),
        Axis::Vertical => Point::new(base.x, base.y + offset),
    };
    harness.edit_root_widget(|mut screen| AreaScreen::drag_bar(&mut screen, split, pos));
}

/// Pans the canvas in area `area` by one step.
fn pan_area(harness: &mut TestHarness<AreaScreen>, area: usize, delta: Vec2) {
    let id = canvas_of(harness, area).ctx().widget_id();
    harness.edit_widget_with_id(id, |mut widget| {
        let mut canvas = widget.downcast::<CanvasLayer>();
        CanvasLayer::pan(&mut canvas, delta);
    });
}

/// Zooms the canvas in area `area` about its centre.
fn zoom_area(harness: &mut TestHarness<AreaScreen>, area: usize, factor: f64) {
    let id = canvas_of(harness, area).ctx().widget_id();
    harness.edit_widget_with_id(id, |mut widget| {
        let mut canvas = widget.downcast::<CanvasLayer>();
        CanvasLayer::zoom_around(&mut canvas, Point::new(200.0, 150.0), factor);
    });
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

    let outcome = Outcome {
        nodes: areas,
        viewport: VIEWPORT,
        quick: opts.quick,
        criteria: evaluate(&reports, &sweep, scale_misses, regions),
        scenarios: reports.iter().map(Report::record).collect(),
        sweep,
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
fn evaluate(reports: &[Report], sweep: &[SweepRecord], scale_misses: u64, regions: (f64, f64)) -> Vec<Criterion> {
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

    criteria
}
