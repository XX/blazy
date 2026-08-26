//! Headless measurements for Phase 0.
//!
//! Runs the scenarios from `rnd/architecture.md` §7.4 against a `TestHarness`, so
//! the numbers are reproducible and do not depend on a GPU, a compositor or a
//! window manager. What is timed is Masonry's own work — event routing, the rewrite
//! passes, and encoding the `VisualLayerPlan` — which is exactly the part this
//! architecture is a bet on. GPU submission is deliberately out of scope: no
//! backend choice can rescue a design that re-lays-out 5000 nodes per pan.
//!
//! Each scenario reports wall time per frame and the delta in the canvas counters,
//! because a fast frame that quietly relaid out everything is not a pass.

use std::time::{Duration, Instant};

use bench_utils::criteria::{Criterion, Kind, Outcome, ScenarioRecord, SweepRecord, ZoomRecord};
use bench_utils::plan;
use blazy_canvas::{CanvasHit, CanvasLayer, CanvasStats, DetailBudget};
use masonry::core::NewWidget;
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Affine, Point, Vec2};
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;
use node_canvas::build_canvas_with;
use node_canvas::editor::NodeEditor;
use node_canvas::model::{GRID_STEP, GraphModel, NODE_SIZE, share};

/// Viewport used for all scenarios.
const VIEWPORT: (u32, u32) = (1100, 750);

/// Frames per scenario. Enough to see a trend, short enough to stay interactive.
const FRAMES: usize = 120;

/// Frames per scenario in the quick set.
///
/// Every gated criterion is a per-frame average of a counter that is either zero or
/// a small constant, so it converges in a few frames; the extra hundred exist to
/// steady the *timings*, which the quick set does not gate on anyway.
const QUICK_FRAMES: usize = 40;

/// Centre of the viewport, used as the zoom anchor everywhere.
const VIEWPORT_CENTRE: Point = Point::new(VIEWPORT.0 as f64 / 2.0, VIEWPORT.1 as f64 / 2.0);

/// One pan step, in viewport pixels.
const PAN_STEP: Vec2 = Vec2::new(-6.0, -2.0);

/// How the benchmark was asked to run.
pub struct Options {
    /// Nodes in the graph under test.
    pub count: usize,
    /// Run only the scenarios the pass criteria are decided on.
    ///
    /// A fast inner loop while working on the canvas: it drops the scenarios that
    /// exist to price design decisions for a human reader — zoom, hover, the
    /// intermediate LOD levels — and keeps every scenario a criterion is computed
    /// from, so the verdict is the same one CI would reach.
    ///
    /// CI runs the *full* set regardless. The whole benchmark is under a second, so
    /// there is nothing to save by archiving fewer numbers.
    pub quick: bool,
}

impl Options {
    /// Frames per scenario for this run.
    fn frames(&self) -> usize {
        if self.quick { QUICK_FRAMES } else { FRAMES }
    }
}

/// Result of one scenario.
struct Report {
    name: &'static str,
    frames: usize,
    total: Duration,
    worst: Duration,
    before: CanvasStats,
    after: CanvasStats,
    /// Most draw commands the window's layer plan held in any frame of the scenario.
    ///
    /// The other half of what a frame costs (§31): the paint pass rebuilds the plan
    /// every frame and re-appends every widget's cached scene into it, so a scene of
    /// ten thousand commands is charged ten thousand times a frame whether or not
    /// anything moved. Widgets cannot see it — in the far field there are none.
    ///
    /// The largest rather than the last, because it is a ceiling.
    commands: usize,
}

impl Report {
    fn mean_ms(&self) -> f64 {
        self.total.as_secs_f64() * 1000.0 / self.frames as f64
    }

    fn worst_ms(&self) -> f64 {
        self.worst.as_secs_f64() * 1000.0
    }

    /// Layout passes run on the canvas content.
    ///
    /// Coarser than [`child_layouts_per_frame`](Self::child_layouts_per_frame) and
    /// harder to satisfy by accident: a pass that visits only clean children still
    /// counts here, and that is exactly the failure a hover has to avoid.
    fn content_layouts_per_frame(&self) -> f64 {
        self.per_frame(self.after.counters.content_layouts - self.before.counters.content_layouts)
    }

    fn child_layouts_per_frame(&self) -> f64 {
        self.per_frame(self.after.counters.child_layouts - self.before.counters.child_layouts)
    }

    fn level_switches_per_frame(&self) -> f64 {
        self.per_frame(self.after.counters.level_switches - self.before.counters.level_switches)
    }

    fn builds_per_frame(&self) -> f64 {
        self.per_frame(self.after.counters.builds - self.before.counters.builds)
    }

    fn far_repaints_per_frame(&self) -> f64 {
        self.per_frame(self.after.counters.far_repaints - self.before.counters.far_repaints)
    }

    fn link_repaints_per_frame(&self) -> f64 {
        self.per_frame(self.after.counters.link_repaints - self.before.counters.link_repaints)
    }

    fn link_reselects_per_frame(&self) -> f64 {
        self.per_frame(self.after.counters.link_reselects - self.before.counters.link_reselects)
    }

    fn slot_visits_per_frame(&self) -> f64 {
        self.per_frame(self.after.counters.slot_visits - self.before.counters.slot_visits)
    }

    /// Picks answered during the scenario.
    fn picks(&self) -> u64 {
        self.after.counters.hit_queries - self.before.counters.hit_queries
    }

    fn node_tests_per_pick(&self) -> f64 {
        per_pick(
            self.after.counters.hit_node_tests - self.before.counters.hit_node_tests,
            self.picks(),
        )
    }

    fn curve_tests_per_pick(&self) -> f64 {
        per_pick(
            self.after.counters.hit_curve_tests - self.before.counters.hit_curve_tests,
            self.picks(),
        )
    }

    fn per_frame(&self, delta: u64) -> f64 {
        delta as f64 / self.frames as f64
    }

    /// Link curves recorded at the end of the scenario, and drawn on every repaint.
    fn recorded_links(&self) -> f64 {
        self.after.recorded_links as f64
    }

    /// Link curves the last selection dropped as too short to see.
    fn hidden_links(&self) -> f64 {
        self.after.hidden_links as f64
    }

    /// The plain-data form the report is built from.
    fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: self.name,
            frames: self.frames,
            mean_ms: self.mean_ms(),
            worst_ms: self.worst_ms(),
            materialised: self.after.materialised,
            detail: format!("{:?}", self.after.detail),
            child_layouts_per_frame: self.child_layouts_per_frame(),
            builds_per_frame: self.builds_per_frame(),
            far_repaints_per_frame: self.far_repaints_per_frame(),
            extra: vec![
                ("content_layouts_per_frame", self.content_layouts_per_frame()),
                ("link_repaints_per_frame", self.link_repaints_per_frame()),
                ("link_reselects_per_frame", self.link_reselects_per_frame()),
                ("slot_visits_per_frame", self.slot_visits_per_frame()),
                ("recorded_links", self.recorded_links()),
                ("hidden_links", self.hidden_links()),
                ("draw_commands", self.commands as f64),
                ("level_switches_per_frame", self.level_switches_per_frame()),
                ("picks", self.picks() as f64),
                ("node_tests_per_pick", self.node_tests_per_pick()),
                ("curve_tests_per_pick", self.curve_tests_per_pick()),
            ],
        }
    }

    fn print(&self) {
        println!(
            "{:<26} {:>7.3} ms/frame  worst {:>7.3} ms  child-layouts/frame {:>7.1}  \
             live {:>4}  builds/frame {:>6.1}",
            self.name,
            self.mean_ms(),
            self.worst_ms(),
            self.child_layouts_per_frame(),
            self.after.materialised,
            self.builds_per_frame(),
        );
        println!(
            "{:<26}   detail {:<18} far repaints/frame {:>6.2}  \
             link repaints/frame {:>6.2}  reselects/frame {:>5.2}  slot visits/frame {:>7.1}",
            "",
            format!("{:?}", self.after.detail),
            self.far_repaints_per_frame(),
            self.link_repaints_per_frame(),
            self.link_reselects_per_frame(),
            self.slot_visits_per_frame(),
        );
        println!(
            "{:<26}   draw commands {:<10} links recorded {:>6}  hidden {:>6}",
            "", self.commands, self.after.recorded_links, self.after.hidden_links,
        );
        // Only the scenarios that move the pointer have anything to say here, and a
        // row of zeroes under every other one would bury the numbers that matter.
        if self.picks() > 0 {
            println!(
                "{:<26}   picks {:<18} node geometries/pick {:>6.1}  curves/pick {:>6.1}",
                "",
                self.picks(),
                self.node_tests_per_pick(),
                self.curve_tests_per_pick(),
            );
        }
    }
}

/// Pans the canvas by one step, as a scenario body.
fn pan_step(harness: &mut TestHarness<NodeEditor>, delta: Vec2) {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::pan(&mut canvas, delta));
    });
}

/// Sets an absolute zoom about the centre of the viewport.
fn zoom_to(harness: &mut TestHarness<NodeEditor>, target: f64) {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            let zoom = canvas.widget.zoom();
            CanvasLayer::zoom_around(&mut canvas, VIEWPORT_CENTRE, target / zoom);
        });
    });
    let _ = harness.redraw();
}

/// A harness zoomed to `factor` about the centre of the viewport, already settled.
fn zoomed_harness(count: usize, factor: f64, controls_on_hover: bool) -> TestHarness<NodeEditor> {
    let mut harness = new_harness_with(count, controls_on_hover);
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::zoom_around(&mut canvas, VIEWPORT_CENTRE, factor);
        });
    });
    let _ = harness.redraw();
    harness
}

/// A count per pick, or zero when nothing was picked.
fn per_pick(count: u64, picks: u64) -> f64 {
    if picks == 0 { 0.0 } else { count as f64 / picks as f64 }
}

/// Reads the canvas counters out of the live widget tree.
///
/// From the canvas itself rather than from the editor's cached copy: the copy is
/// refreshed during layout, and a pick deliberately does not run one (§25.4), so a
/// hover scenario read that way would report having picked nothing.
fn stats(harness: &mut TestHarness<NodeEditor>) -> CanvasStats {
    harness.edit_root_widget(|mut editor| NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.stats()))
}

/// The canvas-space to viewport-space transform.
fn view(harness: &mut TestHarness<NodeEditor>) -> Affine {
    harness.edit_root_widget(|mut editor| NodeEditor::with_canvas(&mut editor, |canvas| canvas.widget.view()))
}

/// Asks the canvas what is under a viewport-space point.
fn pick(harness: &mut TestHarness<NodeEditor>, pos: Point) -> Option<CanvasHit> {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::hit_test(&mut canvas, pos))
    })
}

/// The canvas-space rectangle of a node.
fn node_rect(harness: &mut TestHarness<NodeEditor>, index: usize) -> masonry::kurbo::Rect {
    let pos = harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::child_pos(&mut canvas, index).unwrap_or(Point::ORIGIN)
        })
    });
    masonry::kurbo::Rect::from_origin_size(pos, NODE_SIZE)
}

fn new_harness(count: usize) -> TestHarness<NodeEditor> {
    new_harness_with(count, false)
}

/// A harness over a graph with exactly `links` edges.
fn linked_harness(count: usize, links: usize) -> TestHarness<NodeEditor> {
    let mut edges = node_canvas::generated_links(count);
    edges.truncate(links);
    let (canvas, _graph) = node_canvas::build_canvas_linked(count, edges);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::new(canvas)),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    harness
}

fn new_harness_with(count: usize, controls_on_hover: bool) -> TestHarness<NodeEditor> {
    let (canvas, _graph) = build_canvas_with(count, controls_on_hover);
    let editor = NodeEditor::new(canvas);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(editor),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    // Settle the first layout and paint so the measurements do not include startup.
    let _ = harness.redraw();
    harness
}

/// A harness over a graph of the same size packed `times` more densely.
///
/// The variable that actually decides what a zoom materialises. A larger graph does
/// not put more nodes under the viewport — the generated grid has one density, so
/// beyond the point where the viewport is full the extra nodes are simply off screen
/// (§29.1). Halving the spacing does: four times the nodes at the same zoom, the same
/// node count, nothing else changed.
fn dense_harness(count: usize, times: f64) -> TestHarness<NodeEditor> {
    let graph = share(GraphModel::generated_with_step(count, GRID_STEP / times.sqrt()));
    let canvas = node_canvas::canvas_over(&graph, count, false);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::new(canvas)),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    harness
}

/// A harness over a canvas with an explicit cost ceiling.
fn budgeted_harness(count: usize, budget: DetailBudget) -> TestHarness<NodeEditor> {
    let graph = share(GraphModel::generated(count));
    let canvas = node_canvas::canvas_over(&graph, count, false).with_budget(budget);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::new(canvas)),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    harness
}

/// Widgets in the whole tree.
///
/// The quantity §20.2 says a frame costs, and the one the budget is written in — so
/// the criterion is bounded on the tree itself rather than on the canvas's estimate
/// of it. Deterministic, which is what lets the bound be tight (§20.9).
fn widgets_in_tree(harness: &mut TestHarness<NodeEditor>) -> usize {
    let mut widgets = 0;
    harness.inspect_widgets(|_| widgets += 1);
    widgets
}

/// Puts a canvas-space point under the centre of the viewport at a given zoom.
///
/// The sweep sets the view outright rather than zooming by a factor and panning,
/// because the pan inside each point would otherwise accumulate: at zoom 0.02 a pan
/// of six pixels is three hundred canvas units, and after a few points the viewport
/// has left the graph and the rest of the sweep measures empty space. That mistake
/// was made once, and it made the return leg look free.
fn look_at(harness: &mut TestHarness<NodeEditor>, anchor: Point, zoom: f64) {
    let view = Affine::translate(VIEWPORT_CENTRE - (Affine::scale(zoom) * anchor)) * Affine::scale(zoom);
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::set_view(&mut canvas, view));
    });
    let _ = harness.redraw();
}

/// Times `frames` iterations of `step`, each followed by a full redraw.
fn measure(
    name: &'static str,
    harness: &mut TestHarness<NodeEditor>,
    frames: usize,
    mut step: impl FnMut(&mut TestHarness<NodeEditor>, usize),
) -> Report {
    let before = stats(harness);
    let mut total = Duration::ZERO;
    let mut worst = Duration::ZERO;
    let mut commands = 0;

    for i in 0..frames {
        let start = Instant::now();
        step(harness, i);
        // `redraw` runs the rewrite passes and encodes the visual layer plan. That
        // is the frame, minus GPU submission.
        let (layers, _) = harness.redraw();
        let elapsed = start.elapsed();
        // Counted outside the clock: reading the plan is the measurement, not the
        // frame. It is exact and machine-independent, which is what lets a criterion
        // be bounded on it (§20.9).
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
        after: stats(harness),
        commands,
    }
}

// --- MARK: the far field, in segments (§35)

/// Zooms the far-field measurements are taken at.
///
/// Two, because they answer different questions. At 0.02 the whole graph is on screen
/// and the recorded region is not the binding constraint — the graph is. At 0.10 the
/// canvas is still in the far field but the graph continues past the region, which is
/// the only situation in which the recorded margin costs anything (§35.2).
const FAR_ZOOMS: [f64; 2] = [0.02, 0.10];

/// Frames the far-field rasterisation is timed over.
///
/// Few, because each one is tens of milliseconds — that being the finding (§32.3).
const FAR_RASTER_FRAMES: usize = 6;

/// One far-field configuration: what it records, and what that costs.
struct FarRow {
    what: &'static str,
    zoom: f64,
    nodes: usize,
    /// Nodes and link curves in the recording — what the scene is made of.
    recorded_nodes: usize,
    recorded_links: usize,
    /// Link curves the short-link rule dropped as too small to see (§31.4).
    hidden_links: usize,
    /// What vello would be asked to draw (§35.1).
    objects: usize,
    segments: u64,
    /// Decisions per frame: re-choosing the recorded sets is what a margin buys off.
    far_records: f64,
    link_reselects: f64,
    /// Masonry's half of the frame — passes and plan assembly.
    plan_ms: f64,
    /// The other half, on the blit path, and on the GPU path where there is a device.
    cpu_ms: f64,
    gpu_ms: f64,
}

/// Path segments in the frame the canvas would hand the rasteriser.
///
/// Outside the clock, like every other counter here: encoding the scene is the
/// measurement, not the frame (§35.1).
fn segments_of(harness: &mut TestHarness<NodeEditor>) -> u64 {
    let (plan, _) = harness.redraw();
    let composed = blazy_shell::Composition::new(&plan, 1.0).scene;
    blazy_shell::segments(&composed, PhysicalSize::new(VIEWPORT.0, VIEWPORT.1))
}

/// Measures one far-field configuration over a pan.
fn far_case(
    what: &'static str,
    count: usize,
    zoom: f64,
    links: Vec<blazy_canvas::Link>,
    tuning: node_canvas::FarTuning,
    frames: usize,
    raster: bool,
) -> FarRow {
    let (canvas, _graph) = node_canvas::build_canvas_tuned(count, links, tuning);
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(NodeEditor::new(canvas)),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    let anchor = node_rect(&mut harness, count / 2).origin();
    look_at(&mut harness, anchor, zoom);

    let before = stats(&mut harness).counters;
    let report = measure("far field", &mut harness, frames, |h, i| {
        pan_step(h, if i < frames / 2 { PAN_STEP } else { -PAN_STEP });
    });
    let after = stats(&mut harness);

    let (plan, _) = harness.redraw();
    let frame = PhysicalSize::new(VIEWPORT.0, VIEWPORT.1);
    let composed = blazy_shell::Composition::new(&plan, 1.0).scene;
    let encoded = blazy_shell::encoded(&composed, frame);

    let (mut cpu_ms, mut gpu_ms) = (0.0, 0.0);
    if raster {
        cpu_ms = time_raster(&plan);
        gpu_ms = time_gpu(&plan);
    }

    FarRow {
        what,
        zoom,
        nodes: count,
        recorded_nodes: after.recorded_far,
        recorded_links: after.recorded_links,
        hidden_links: after.hidden_links,
        objects: encoded.objects,
        segments: encoded.segments,
        far_records: (after.counters.far_records - before.far_records) as f64 / frames as f64,
        link_reselects: (after.counters.link_reselects - before.link_reselects) as f64 / frames as f64,
        plan_ms: report.mean_ms(),
        cpu_ms,
        gpu_ms,
    }
}

/// Rasterises the plan on the blit path and returns milliseconds per frame.
fn time_raster(plan: &masonry::app::VisualLayerPlan) -> f64 {
    let Ok(mut host) = blazy_shell::Host::any() else {
        return 0.0;
    };
    let size = masonry::kurbo::Size::new(f64::from(VIEWPORT.0), f64::from(VIEWPORT.1));
    let _ = host.render(plan, size);
    let start = Instant::now();
    for _ in 0..FAR_RASTER_FRAMES {
        let _ = host.render(plan, size);
    }
    start.elapsed().as_secs_f64() * 1000.0 / FAR_RASTER_FRAMES as f64
}

/// The same on the GPU path, where a device opens. Zero where none does.
fn time_gpu(plan: &masonry::app::VisualLayerPlan) -> f64 {
    let Ok(mut gpu) = blazy_shell::gpu::GpuFrames::offscreen(PhysicalSize::new(VIEWPORT.0, VIEWPORT.1)) else {
        return 0.0;
    };
    let size = masonry::kurbo::Size::new(f64::from(VIEWPORT.0), f64::from(VIEWPORT.1));
    if gpu.draw(plan, size, 1.0).is_err() {
        return 0.0;
    }
    gpu.wait();
    let start = Instant::now();
    for _ in 0..FAR_RASTER_FRAMES {
        if gpu.draw(plan, size, 1.0).is_err() {
            return 0.0;
        }
        gpu.wait();
    }
    start.elapsed().as_secs_f64() * 1000.0 / FAR_RASTER_FRAMES as f64
}

/// What a far-field frame is made of, and what each lever takes off it.
///
/// The table §35 is argued from. Every row is the same graph at the same zoom; what
/// changes is one decision at a time.
fn far_table(opts: &Options, count: usize, zoom: f64) -> Vec<FarRow> {
    let frames = opts.frames();
    let raster = !opts.quick;
    let links = node_canvas::generated_links(count);
    let default = node_canvas::FarTuning::default();
    let with = |overscan: f64| node_canvas::FarTuning { overscan, ..default };
    let links_at = |min_link_px: f64| node_canvas::FarTuning { min_link_px, ..default };

    println!("\nfar field: what the frame is made of at zoom {zoom} ({count} nodes)");
    let rows = vec![
        far_case(
            "rounded nodes",
            count,
            zoom,
            links.clone(),
            node_canvas::FarTuning {
                min_radius_px: 0.0,
                ..default
            },
            frames,
            raster,
        ),
        far_case("plain nodes", count, zoom, links.clone(), default, frames, raster),
        far_case("plain, no links", count, zoom, Vec::new(), default, frames, raster),
        far_case("overscan 0.50", count, zoom, links.clone(), with(0.50), frames, raster),
        far_case("overscan 0.10", count, zoom, links.clone(), with(0.10), frames, raster),
        far_case("overscan 0.00", count, zoom, links.clone(), with(0.0), frames, raster),
        far_case(
            "links >= 4 px",
            count,
            zoom,
            links.clone(),
            links_at(4.0),
            frames,
            raster,
        ),
        far_case("links >= 8 px", count, zoom, links, links_at(8.0), frames, raster),
    ];
    print_far(&rows);
    rows
}

impl FarRow {
    /// The plain-data form the report archives, so the levers can be diffed across
    /// commits rather than re-argued.
    fn record(&self) -> ScenarioRecord {
        ScenarioRecord {
            name: "far field",
            frames: 0,
            mean_ms: self.plan_ms,
            worst_ms: self.cpu_ms,
            materialised: 0,
            detail: format!("{} @ {} on {} nodes", self.what, self.zoom, self.nodes),
            child_layouts_per_frame: 0.0,
            builds_per_frame: 0.0,
            far_repaints_per_frame: 0.0,
            extra: vec![
                ("recorded_nodes", self.recorded_nodes as f64),
                ("recorded_links", self.recorded_links as f64),
                ("hidden_links", self.hidden_links as f64),
                ("draw_objects", self.objects as f64),
                ("path_segments", self.segments as f64),
                ("far_records_per_frame", self.far_records),
                ("link_reselects_per_frame", self.link_reselects),
                ("cpu_raster_ms", self.cpu_ms),
                ("gpu_raster_ms", self.gpu_ms),
            ],
        }
    }
}

fn print_far(rows: &[FarRow]) {
    println!(
        "  {:<16} {:>8} {:>8} {:>8} {:>9} {:>10} {:>10} {:>9} {:>9} {:>9}",
        "what", "nodes", "links", "hidden", "segments", "records/f", "resel/f", "plan ms", "cpu ms", "gpu ms"
    );
    for row in rows {
        println!(
            "  {:<16} {:>8} {:>8} {:>8} {:>9} {:>10.2} {:>10.2} {:>9.3} {:>9.2} {:>9.2}",
            row.what,
            row.recorded_nodes,
            row.recorded_links,
            row.hidden_links,
            row.segments,
            row.far_records,
            row.link_reselects,
            row.plan_ms,
            row.cpu_ms,
            row.gpu_ms,
        );
    }
}

/// Runs the scenarios, prints the numbers, and returns the evaluated criteria.
///
/// Scenarios split in two. The ones a criterion is decided on always run; the ones
/// that only inform a reader are skipped under [`Options::quick`], and are marked
/// as such below.
pub fn run(opts: &Options) -> Outcome {
    let count = opts.count;
    let frames = opts.frames();
    println!(
        "blazy Phase 0 - node canvas on masonry_core@main\n\
         nodes {count}, viewport {}x{}, {frames} frames per scenario{}\n",
        VIEWPORT.0,
        VIEWPORT.1,
        if opts.quick { " (quick set)" } else { "" }
    );

    let mut reports = Vec::new();

    // --- Scenario 1: idle.
    //
    // Nothing changes. The baseline cost of a frame in which no widget is dirty.
    {
        let mut harness = new_harness(count);
        reports.push(measure("idle", &mut harness, frames, |_, _| {}));
    }

    // --- Scenario 2: pan.
    //
    // The claim under test. Panning changes one `Affine`; culling reruns, but every
    // child that was already laid out must early-return in `run_layout_on`. If
    // child-layouts/frame is roughly the number of nodes entering the viewport
    // rather than the number of visible nodes, the design holds.
    {
        let mut harness = new_harness(count);
        reports.push(measure("pan", &mut harness, frames, |h, _| pan_step(h, PAN_STEP)));
    }

    // --- Scenario 3: zoom. Informational.
    //
    // Same as pan, plus LOD threshold crossings. Those *should* cost a relayout —
    // that is what LOD is for — so the spikes are expected and worth seeing in the
    // worst-frame column.
    if !opts.quick {
        let mut harness = new_harness(count);
        reports.push(measure("zoom", &mut harness, frames, |h, i| {
            // Oscillate so the run passes through the LOD thresholds repeatedly.
            let factor = if (i / 20) % 2 == 0 { 0.97 } else { 1.0 / 0.97 };
            h.edit_root_widget(|mut editor| {
                NodeEditor::with_canvas(&mut editor, |mut canvas| {
                    CanvasLayer::zoom_around(&mut canvas, Point::new(550.0, 375.0), factor);
                });
            });
        }));
    }

    // --- Scenario 4: drag one node.
    //
    // The second pass criterion: moving one node must not rebuild the window. Only
    // the dragged node's position changes, so only it should need layout.
    {
        let mut harness = new_harness(count);
        reports.push(measure("drag one node", &mut harness, frames, |h, i| {
            let dx = ((i % 40) as f64 - 20.0) * 0.5;
            h.edit_root_widget(|mut editor| {
                NodeEditor::with_canvas(&mut editor, |mut canvas| {
                    let base = CanvasLayer::child_pos(&mut canvas, 0).unwrap_or(Point::ORIGIN);
                    CanvasLayer::move_child(&mut canvas, 0, Point::new(base.x + dx, base.y));
                });
            });
        }));
    }

    // --- Scenario 4b: drag a node that has links.
    //
    // The question the link layer exists to answer. A dragged node moves the ends of
    // its own curves, so the link scene has to be redrawn — but redrawing it must not
    // mean re-choosing which curves are on screen, or a drag would walk the region
    // once per frame instead of touching one node's edges.
    {
        let mut harness = new_harness(count);
        reports.push(measure("drag a linked node", &mut harness, frames, |h, i| {
            let dx = ((i % 40) as f64 - 20.0) * 0.5;
            h.edit_root_widget(|mut editor| {
                NodeEditor::with_canvas(&mut editor, |mut canvas| {
                    let base = CanvasLayer::child_pos(&mut canvas, 41).unwrap_or(Point::ORIGIN);
                    CanvasLayer::move_child(&mut canvas, 41, Point::new(base.x + dx, base.y));
                });
            });
        }));
    }

    // --- Scenario 5: pointer movement over the canvas.
    //
    // Every move is a pick: what is under the pointer, node or link, answered from
    // the model through the grid and the recorded curve set (§25.3). The criterion
    // this decides is that picking does not drag a layout pass behind it — a pointer
    // crossing a graph is the most common thing that happens to a canvas, and it has
    // to cost a repaint at most.
    {
        let mut harness = new_harness(count);
        reports.push(measure("hover", &mut harness, frames, |h, i| {
            let x = 200.0 + (i % 60) as f64 * 8.0;
            h.mouse_move(Point::new(x, 300.0));
        }));
    }

    // --- Scenario 6: zoomed out to Box LOD. Informational.
    //
    // At Box detail every node stashes its slider and checkbox, so the number of
    // laid-out widgets drops by roughly two thirds. This is the measurement that
    // says whether LOD is worth its complexity.
    if !opts.quick {
        let mut harness = zoomed_harness(count, 0.15, false);
        reports.push(measure("pan, zoom 0.15", &mut harness, frames, |h, _| {
            pan_step(h, PAN_STEP)
        }));
    }

    // --- Scenario 6b: pan at Simplified detail. Informational.
    //
    // Between Full and the far field: nodes still have widgets, but the checkbox is
    // stashed. Worth measuring separately because stashing is exactly the halfway
    // measure that section 20.2 showed does not pay.
    if !opts.quick {
        let mut harness = zoomed_harness(count, 0.4, false);
        reports.push(measure("pan, zoom 0.40", &mut harness, frames, |h, _| {
            pan_step(h, PAN_STEP)
        }));
    }

    // --- Scenario 5b: the same view with controls materialised only on hover.
    //
    // Measured but not enabled by default: the painted stand-in does not match
    // Masonry's themed controls closely enough for the swap to go unnoticed. Kept as
    // a scenario so the price of that decision stays a number rather than a memory.
    if !opts.quick {
        let mut harness = zoomed_harness(count, 0.4, true);
        reports.push(measure("pan, zoom 0.40, on hover", &mut harness, frames, |h, _| {
            pan_step(h, PAN_STEP)
        }));
    }

    // --- Scenario 6c: just above the far-field threshold. Informational.
    //
    // The worst point on the curve: nodes are still materialised, but the viewport
    // covers most of the graph.
    if !opts.quick {
        let mut harness = zoomed_harness(count, 0.11, false);
        reports.push(measure("pan, zoom 0.11", &mut harness, 40, |h, _| {
            pan_step(h, PAN_STEP)
        }));
    }

    // --- Scenario 6d: panning after a look at the whole graph.
    //
    // The same view as scenario 2, reached the way a user reaches it: zoom out far
    // enough to see everything, then come back. A recorded region that could only
    // grow made this permanently slower than the pan it should be identical to
    // (§28), so the two are measured against each other.
    {
        let mut harness = new_harness(count);
        zoom_to(&mut harness, 0.05);
        zoom_to(&mut harness, 1.0);
        reports.push(measure("pan after an overview", &mut harness, frames, |h, _| {
            pan_step(h, PAN_STEP)
        }));
    }

    // --- Scenario 7: the whole graph on screen.
    //
    // Virtualisation bounds cost by the *visible* set, so zooming out far enough
    // that every node is visible removes the bound by definition. This scenario
    // exists to measure what is left when it does.
    {
        let mut harness = zoomed_harness(count, 0.04, false);
        reports.push(measure("pan, whole graph shown", &mut harness, 30, |h, _| {
            pan_step(h, PAN_STEP)
        }));
    }

    println!();
    for report in &reports {
        report.print();
    }

    let sweep = scaling_sweep(opts);
    let links = link_sweep(opts, count);
    let picks = pick_sweep(opts);
    let zoom_picks = zoom_pick_sweep(opts);
    // Two graph sizes, because a ceiling that quietly follows the graph is exactly
    // what the zoom thresholds did (§29.1) and the criterion has to be able to see it.
    let frames = if opts.quick { 8 } else { 16 };
    let mut zooms = zoom_sweep(opts, count, frames, None);
    // The same sweep over a graph four times as dense. This is the comparison the
    // budget exists for: a zoom threshold is a constant tuned for one node size, and
    // the denser graph is the same interface under a different one.
    let dense = zoom_sweep(opts, count, frames, Some(4.0));
    let wobble = boundary_wobble(opts, count);
    // The far field in the unit the second half of a frame is charged in (§35). Two
    // graph sizes for the same reason the zoom sweep uses two: a ceiling that quietly
    // follows the graph is what has to be visible.
    let mut far = Vec::new();
    for zoom in FAR_ZOOMS {
        far.extend(far_table(opts, count, zoom));
        if !opts.quick {
            far.extend(far_table(opts, count * 4, zoom));
        }
    }

    let criteria = evaluate(
        &reports, count, &sweep, &links, &picks, zoom_picks, &zooms, &dense, wobble, &far,
    );
    zooms.extend(dense);
    let outcome = Outcome {
        nodes: count,
        viewport: VIEWPORT,
        quick: opts.quick,
        criteria,
        scenarios: reports
            .iter()
            .map(Report::record)
            .chain(far.iter().map(FarRow::record))
            .collect(),
        sweep,
        zoom_sweep: zooms,
    };
    outcome.report("Phase 0 criteria");
    outcome
}

/// Zooms the range end to end and back, recording what the tree holds at each stop.
///
/// The sweep the widget budget is judged on (§29.3). Three things about its shape are
/// load-bearing:
///
/// * **It returns.** A sweep that only zooms out enters every state and leaves none, which tests half a switch (§28.4).
///   It is also the only way to see the hysteresis: the same zoom legitimately carries a different level depending on
///   which side it was reached from.
/// * **It looks at a fixed point of the graph.** Each stop sets the view outright, so a stop measures a zoom rather
///   than however far the previous stops have drifted.
/// * **The endpoints are part of the criterion.** 0.02 is where the far field holds the whole graph and 4.0 is where it
///   holds one node; a sweep that stopped at 0.1 would report a tree that never misbehaves because it never got large
///   (§24.1).
fn zoom_sweep(opts: &Options, count: usize, frames: usize, density: Option<f64>) -> Vec<ZoomRecord> {
    const ZOOMS: [f64; 9] = [4.0, 1.0, 0.3, 0.21, 0.15, 0.1, 0.06, 0.04, 0.02];
    const QUICK_ZOOMS: [f64; 5] = [1.0, 0.21, 0.1, 0.04, 0.02];

    let zooms: &[f64] = if opts.quick { &QUICK_ZOOMS } else { &ZOOMS };

    let mut harness = match density {
        Some(times) => dense_harness(count, times),
        None => new_harness(count),
    };
    let anchor = node_rect(&mut harness, count / 2).origin();
    let mut points = Vec::new();

    let what = match density {
        Some(times) => format!("{count} nodes packed {times}x denser"),
        None => format!("{count} nodes"),
    };
    println!("\nzoom: what the tree holds at each zoom ({what}, out and back)");
    for returning in [false, true] {
        let order: Vec<f64> = if returning {
            zooms.iter().rev().copied().collect()
        } else {
            zooms.to_vec()
        };
        for zoom in order {
            look_at(&mut harness, anchor, zoom);
            let before = stats(&mut harness).counters.level_switches;
            let report = measure("zoom point", &mut harness, frames, |h, i| {
                // Oscillates about the stop rather than drifting away from it.
                pan_step(h, if i < frames / 2 { PAN_STEP } else { -PAN_STEP });
            });
            let after = stats(&mut harness);
            let point = ZoomRecord {
                zoom: after.zoom,
                returning,
                nodes: count,
                visible: after.visible,
                widgets: widgets_in_tree(&mut harness),
                detail: format!("{:?}", after.detail),
                level_switches_per_frame: (after.counters.level_switches - before) as f64 / frames as f64,
                commands: report.commands,
                hidden_links: after.hidden_links,
                segments: segments_of(&mut harness),
                mean_ms: report.mean_ms(),
                worst_ms: report.worst_ms(),
            };
            println!(
                "  {:>6.3}x {:<3} {:<11} visible {:>5}  widgets {:>5}  commands {:>6}  \
                 segments {:>7}  hidden links {:>5}  {:>7.3} ms/frame  worst {:>7.3} ms  switches/frame {:>4.2}",
                point.zoom,
                if returning { "up" } else { "out" },
                point.detail,
                point.visible,
                point.widgets,
                point.commands,
                point.segments,
                point.hidden_links,
                point.mean_ms,
                point.worst_ms,
                point.level_switches_per_frame,
            );
            points.push(point);
        }
    }
    points
}

/// How often the level changes when the visible set wobbles about the ceiling.
///
/// The stimulus is a zoom oscillating by 2%, which moves the visible count by about
/// 3% — a wheel nudged back and forth. The budget is *calibrated* from the graph
/// rather than assumed: it is set to the number of nodes visible at the test zoom, so
/// the boundary is guaranteed to sit inside the wobble. A criterion that missed the
/// boundary would pass by never being near it.
///
/// Returns level switches per frame. Without hysteresis this is 1.00 — every frame
/// rebuilds every visible node; with the default margin it is one switch in the whole
/// run.
fn boundary_wobble(opts: &Options, count: usize) -> f64 {
    let frames = if opts.quick { 20 } else { 40 };

    let mut probe = budgeted_harness(count, DetailBudget::unlimited());
    zoom_to(&mut probe, 0.1);
    let at_boundary = stats(&mut probe).visible;

    let mut harness = budgeted_harness(count, DetailBudget {
        widgets: at_boundary,
        ..DetailBudget::default()
    });
    zoom_to(&mut harness, 0.1);
    let before = stats(&mut harness).counters.level_switches;
    for i in 0..frames {
        harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| {
                let factor = if i % 2 == 0 { 1.0 / 1.02 } else { 1.02 };
                CanvasLayer::zoom_around(&mut canvas, VIEWPORT_CENTRE, factor);
            });
        });
        let _ = harness.redraw();
    }
    let switches = stats(&mut harness).counters.level_switches - before;
    println!(
        "\nhysteresis: a 2% zoom wobble with the ceiling set to the {at_boundary} nodes on screen\n  \
         {switches} level switches in {frames} frames"
    );
    switches as f64 / frames as f64
}

/// Measures frame cost against total node count, with the visible count held fixed.
///
/// This is the sweep that decides whether culling is sufficient. If frame cost
/// tracks the *visible* set, stashing off-screen nodes is enough. If it tracks the
/// *total*, then the widget tree itself is the cost and the nodes have to leave the
/// tree entirely — which is virtualisation, not culling.
///
/// The quick set keeps the two endpoints and drops the two in between. The endpoints
/// are what both sweep criteria are computed from; the middle points only show that
/// the curve between them is not doing something strange, and building a 16 000-node
/// graph is the single most expensive thing in the whole benchmark.
fn scaling_sweep(opts: &Options) -> Vec<SweepRecord> {
    const COUNTS: [usize; 4] = [250, 1000, 4000, 16000];
    const QUICK_COUNTS: [usize; 2] = [250, 4000];

    let counts: &[usize] = if opts.quick { &QUICK_COUNTS } else { &COUNTS };
    let sweep_frames = if opts.quick { 25 } else { 40 };

    println!("\nscaling: frame cost vs total nodes (visible set stays ~constant)");
    let mut points = Vec::new();

    for &nodes in counts {
        let mut harness = new_harness(nodes);
        let idle = measure("idle", &mut harness, sweep_frames, |_, _| {});

        let mut harness = new_harness(nodes);
        let pan = measure("pan", &mut harness, sweep_frames, |h, _| pan_step(h, PAN_STEP));

        let point = SweepRecord {
            nodes,
            visible: pan.after.materialised,
            idle_ms: idle.mean_ms(),
            pan_ms: pan.mean_ms(),
        };
        println!(
            "  {:>6} nodes ({:>3} visible)  idle {:>7.3} ms  pan {:>7.3} ms               = {:>6.2} us/node/frame while panning",
            point.nodes,
            point.visible,
            point.idle_ms,
            point.pan_ms,
            point.pan_ms * 1000.0 / point.nodes as f64,
        );
        points.push(point);
    }

    points
}

/// Measures frame cost against the number of edges, at a fixed node count.
///
/// The claim under test is the one virtualisation could not settle: nodes escaped
/// linearity by leaving the widget tree, and links cannot leave it the same way. If
/// the curve layer is doing its job, panning a graph with no edges and one with two
/// per node cost about the same, because only the edges near the viewport are ever
/// recorded.
fn link_sweep(opts: &Options, count: usize) -> Vec<SweepRecord> {
    let all = node_canvas::generated_links(count).len();
    let counts: Vec<usize> = if opts.quick {
        vec![0, all]
    } else {
        vec![0, all / 4, all / 2, all]
    };
    let frames = if opts.quick { 25 } else { 40 };

    println!("\nlinks: frame cost vs edge count ({count} nodes, same viewport)");
    let mut points = Vec::new();
    for links in counts {
        let mut harness = linked_harness(count, links);
        let pan = measure("pan", &mut harness, frames, |h, _| pan_step(h, PAN_STEP));
        let point = SweepRecord {
            nodes: links,
            visible: pan.after.materialised,
            idle_ms: pan.link_reselects_per_frame(),
            pan_ms: pan.mean_ms(),
        };
        println!(
            "  {:>6} edges  pan {:>7.3} ms  link reselects/frame {:>5.2}  live {:>4}",
            point.nodes, point.pan_ms, point.idle_ms, point.visible,
        );
        points.push(point);
    }
    points
}

/// One point of the picking sweep.
struct PickRecord {
    nodes: usize,
    /// Node geometries examined per pick.
    node_tests: f64,
    /// Link curves examined per pick.
    curve_tests: f64,
    /// Corners sampled just inside a node's bounding box but outside its body.
    corners: usize,
    /// How many of those still reported a node — the whole point of a precise phase.
    corner_hits: usize,
}

/// Measures the cost of one pick against the size of the graph.
///
/// A pick asks two questions: which nodes are near the point (the grid answers), and
/// which recorded curves pass through it (the link layer answers). Neither may follow
/// the size of the graph, or a pointer moving across a large canvas costs what the
/// canvas costs. The sweep also samples node corners, because a pick that is cheap
/// and wrong is not an improvement over a rectangle.
fn pick_sweep(opts: &Options) -> Vec<PickRecord> {
    // The sweep starts at 1000 rather than at 250, and the endpoint is part of the
    // criterion just as it was in §24.1 — in the other direction. A 250-node graph
    // is three rows tall, which is *smaller* than the region the canvas records
    // curves for, so a pick there examines fewer curves for a reason that has
    // nothing to do with the index: the graph runs out. Measured 46 curves/pick at
    // 250 against 80 at both 4000 and 16 000. Bounding the large end against an
    // unsaturated small end would be comparing the graph's extent, not its density.
    const COUNTS: [usize; 3] = [1_000, 4_000, 16_000];
    const QUICK_COUNTS: [usize; 2] = [1_000, 4_000];
    /// Nodes whose corners are sampled. Four points each.
    const CORNER_NODES: usize = 20;
    /// How far inside the bounding-box corner to sample, in canvas units.
    ///
    /// Inside the box and outside a 6-unit rounded corner: the diagonal distance from
    /// the corner is 2.1, against a radius that cuts 6 off each side.
    const CORNER_INSET: f64 = 1.5;

    let counts: &[usize] = if opts.quick { &QUICK_COUNTS } else { &COUNTS };
    println!("\npicking: cost of one pick vs graph size");
    let mut points = Vec::new();

    for &nodes in counts {
        let mut harness = linked_harness(nodes, node_canvas::generated_links(nodes).len());
        let before = stats(&mut harness).counters;
        let view = view(&mut harness);

        // A grid of picks over the viewport, so the sample includes points on nodes,
        // on curves and on empty canvas in whatever proportion the graph has them.
        let mut picks = 0_u64;
        for row in 0..20 {
            for col in 0..20 {
                let pos = Point::new(40.0 + col as f64 * 52.0, 30.0 + row as f64 * 36.0);
                pick(&mut harness, pos);
                picks += 1;
            }
        }

        // The precise phase: a point inside the bounding box but outside the rounded
        // body must not report the node.
        let mut corners = 0;
        let mut corner_hits = 0;
        for index in 0..CORNER_NODES.min(nodes) {
            let rect = node_rect(&mut harness, index);
            for (x, y) in [
                (rect.x0 + CORNER_INSET, rect.y0 + CORNER_INSET),
                (rect.x1 - CORNER_INSET, rect.y0 + CORNER_INSET),
                (rect.x0 + CORNER_INSET, rect.y1 - CORNER_INSET),
                (rect.x1 - CORNER_INSET, rect.y1 - CORNER_INSET),
            ] {
                corners += 1;
                if pick(&mut harness, view * Point::new(x, y)).and_then(CanvasHit::node) == Some(index) {
                    corner_hits += 1;
                }
            }
        }

        let after = stats(&mut harness).counters;
        let point = PickRecord {
            nodes,
            node_tests: per_pick(after.hit_node_tests - before.hit_node_tests, picks),
            curve_tests: per_pick(after.hit_curve_tests - before.hit_curve_tests, picks),
            corners,
            corner_hits,
        };
        println!(
            "  {:>6} nodes  node geometries/pick {:>6.1}  curves/pick {:>7.1}  \
             corners sampled {:>3}, wrongly on a node {:>3}",
            point.nodes, point.node_tests, point.curve_tests, point.corners, point.corner_hits,
        );
        points.push(point);
    }

    points
}

/// Checks that a pick means the same thing at every zoom.
///
/// The tolerance is in screen pixels and the test is in canvas units, so the two are
/// separated by the zoom — a factor of 400 across the range the canvas allows
/// (§25.2). Sampled on a graph of two nodes and one link, so that "some other link"
/// cannot be the answer: three points at fixed *screen* offsets from the curve, whose
/// verdicts must not depend on the zoom at all.
///
/// Returns how many verdicts changed, and how many were taken.
fn zoom_pick_sweep(opts: &Options) -> (usize, usize) {
    const ZOOMS: [f64; 4] = [0.25, 1.0, 4.0, 8.0];
    const QUICK_ZOOMS: [f64; 2] = [0.25, 8.0];
    /// Screen-pixel offsets from the curve, and whether the link should be picked.
    ///
    /// The far one clears the stroke as well as the tolerance: the stroke is two
    /// canvas units wide, which at the top of the zoom range is sixteen screen
    /// pixels, and a sample inside the drawn curve would be a hit for the right
    /// reason rather than a bug.
    const SAMPLES: [(f64, bool); 3] = [(0.0, true), (3.0, true), (16.0, false)];

    let zooms: &[f64] = if opts.quick { &QUICK_ZOOMS } else { &ZOOMS };
    let mut harness = linked_harness(2, 1);

    // Put the middle of the only link under the centre of the viewport, so that
    // zooming about the centre leaves it exactly where it is.
    let (from, to) = (node_rect(&mut harness, 0), node_rect(&mut harness, 1));
    let midpoint = Point::new((from.x1 + to.x0) / 2.0, (from.center().y + to.center().y) / 2.0);
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::pan(&mut canvas, VIEWPORT_CENTRE - midpoint);
        });
    });
    let _ = harness.redraw();

    println!("\nzoom: does a pick mean the same thing at every scale?");
    let (mut changes, mut samples) = (0, 0);
    for &zoom in zooms {
        let factor = zoom / stats(&mut harness).zoom;
        harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| {
                CanvasLayer::zoom_around(&mut canvas, VIEWPORT_CENTRE, factor);
            });
        });
        let _ = harness.redraw();

        let mut line = String::new();
        for (offset, expected) in SAMPLES {
            let hit = pick(&mut harness, VIEWPORT_CENTRE + (0.0, offset)).and_then(CanvasHit::link);
            samples += 1;
            if hit.is_some() != expected {
                changes += 1;
            }
            line.push_str(&format!(
                "  {offset:>4.0} px -> {}",
                if hit.is_some() { "link" } else { "-   " }
            ));
        }
        println!("  zoom {:>5.2}x{line}", stats(&mut harness).zoom);
    }

    (changes, samples)
}

/// The Phase 0 pass criteria, evaluated against the numbers just measured.
///
/// Bounds are set well clear of the measured values (§20.5), because a criterion is
/// a regression alarm, not a performance target: it must fire when the architecture
/// stops holding, and stay silent through ordinary tuning. A criterion whose scenario
/// did not run is simply absent — that is how the quick set drops the ones it cannot
/// decide, rather than passing them by default.
#[allow(clippy::too_many_arguments)]
fn evaluate(
    reports: &[Report],
    count: usize,
    sweep: &[SweepRecord],
    links: &[SweepRecord],
    picks: &[PickRecord],
    zoom_picks: (usize, usize),
    zooms: &[ZoomRecord],
    dense_zooms: &[ZoomRecord],
    wobble: f64,
    far: &[FarRow],
) -> Vec<Criterion> {
    let find = |name: &str| reports.iter().find(|r| r.name == name);
    let mut criteria = Vec::new();

    if let Some(pan) = find("pan") {
        // A pan should only lay out nodes newly entering the viewport. Anything
        // approaching the visible count means every visible node is being relaid
        // out, which is the failure mode this design exists to avoid. Measured: 0.2
        // against a visible set of ~34.
        criteria.push(Criterion {
            name: "pan_does_not_relayout_visible_set",
            claim: "panning does not relayout the visible set",
            kind: Kind::Counter,
            measured: pan.child_layouts_per_frame(),
            bound: pan.after.materialised as f64 * 0.5,
            unit: "child layouts/frame",
        });

        // Virtualisation means the tree holds the viewport, not the graph. Measured:
        // 34 of 5000.
        criteria.push(Criterion {
            name: "culling_bounds_the_live_set",
            claim: "culling keeps the materialised set small",
            kind: Kind::Counter,
            measured: pan.after.materialised as f64,
            bound: count as f64 / 4.0,
            unit: "widgets in tree",
        });
    }

    if let Some(drag) = find("drag one node") {
        // Moving one node should lay out one node. Measured: 0.0.
        criteria.push(Criterion {
            name: "drag_relayouts_one_node",
            claim: "dragging one node relayouts ~one node",
            kind: Kind::Counter,
            measured: drag.child_layouts_per_frame(),
            bound: 4.0,
            unit: "child layouts/frame",
        });
    }

    if let Some(far) = find("pan, whole graph shown") {
        // §20.6a: the far-field scene is recorded in canvas coordinates, so a pan
        // inside the recorded region must reuse it untouched. If this climbs, the
        // scene is being thrown away every frame and the overview zoom is back to
        // costing what it cost before §20.6.
        criteria.push(Criterion {
            name: "far_field_survives_a_pan",
            claim: "far-field scene is not re-recorded while panning",
            kind: Kind::Counter,
            measured: far.far_repaints_per_frame(),
            bound: 0.2,
            unit: "re-records/frame",
        });
    }

    // The decisive pair: does cost follow the visible set or the whole tree?
    if let (Some(first), Some(last)) = (sweep.first(), sweep.last())
        && first.nodes != last.nodes
    {
        let node_ratio = last.nodes as f64 / first.nodes as f64;

        // The counter form, and the one to trust: if the tree stays the size of the
        // viewport, nothing downstream can be linear in the graph. Deterministic, so
        // the bound is tight. Measured: 28 -> 35 across a 64x sweep.
        criteria.push(Criterion {
            name: "live_set_independent_of_graph_size",
            claim: "materialised set does not grow with the graph",
            kind: Kind::Counter,
            measured: last.visible as f64,
            bound: first.visible as f64 * 1.5 + 8.0,
            unit: "widgets in tree",
        });

        // The timing form of the same claim — see the `criteria` module docs for why
        // this one is gated on wall time and nothing else is. Linear in total nodes
        // would mean culling bought nothing. Measured: 1.1x against a bound of 8x.
        criteria.push(Criterion {
            name: "frame_cost_independent_of_graph_size",
            claim: "frame cost does not follow total node count",
            kind: Kind::Timing,
            measured: last.pan_ms / first.pan_ms,
            bound: node_ratio * 0.5,
            unit: "x slower",
        });
    }

    // --- The link layer and the spatial index.

    if let Some(pan) = find("pan") {
        // The far field's trick, applied to curves: the scene is in canvas
        // coordinates, so a pan reuses it through the layer transform.
        criteria.push(Criterion {
            name: "pan_does_not_reselect_links",
            claim: "panning does not re-choose which links are drawn",
            kind: Kind::Counter,
            measured: pan.link_reselects_per_frame(),
            bound: 0.05,
            unit: "link reselects/frame",
        });

        // What the spatial index bought. Before it, this was the whole graph.
        criteria.push(Criterion {
            name: "finding_visible_nodes_does_not_walk_the_graph",
            claim: "choosing the visible set does not walk the graph",
            kind: Kind::Counter,
            measured: pan.slot_visits_per_frame(),
            bound: (count as f64 / 8.0).max(64.0),
            unit: "geometries examined/frame",
        });
    }

    if let (Some(plain), Some(after)) = (find("pan"), find("pan after an overview")) {
        // §28: a region that only ever grew left the whole graph's edges recorded, so
        // the same view cost nine times more after one look at the overview. Counted
        // rather than timed, because the curves are the cause and the milliseconds
        // are only the symptom.
        criteria.push(Criterion {
            name: "an_overview_does_not_leave_the_graph_recorded",
            claim: "returning from the overview zoom restores the recorded set",
            kind: Kind::Counter,
            measured: after.recorded_links(),
            bound: plain.recorded_links() * 3.0 + 32.0,
            unit: "link curves recorded",
        });
    }

    if let Some(drag) = find("drag a linked node") {
        // Redrawing the curves is unavoidable; re-choosing them is not.
        criteria.push(Criterion {
            name: "dragging_a_linked_node_does_not_reselect_links",
            claim: "dragging a linked node does not re-choose the drawn links",
            kind: Kind::Counter,
            measured: drag.link_reselects_per_frame(),
            bound: 0.05,
            unit: "link reselects/frame",
        });
    }

    if let (Some(none), Some(all)) = (links.first(), links.last())
        && none.nodes != all.nodes
    {
        criteria.push(Criterion {
            name: "frame_cost_independent_of_link_count",
            claim: "frame cost does not follow the number of edges",
            kind: Kind::Timing,
            measured: all.pan_ms / none.pan_ms,
            bound: 3.0,
            unit: "x slower",
        });
    }

    // --- Picking by shape (§25).

    if let Some(hover) = find("hover") {
        // A pick is a question about the model, and answering it must not dirty the
        // tree. Counted as layout *passes* rather than as children laid out: a pass
        // over clean children costs almost nothing and would leave the child counter
        // at zero, so bounding that one would have let a hover ask for layout on
        // every frame and still pass — checked by breaking it on purpose.
        criteria.push(Criterion {
            name: "picking_does_not_relayout",
            claim: "picking under the pointer does not relayout",
            kind: Kind::Counter,
            measured: hover.content_layouts_per_frame(),
            bound: 0.5,
            unit: "layout passes/frame",
        });
    }

    if let (Some(small), Some(large)) = (picks.first(), picks.last())
        && small.nodes != large.nodes
    {
        // The grid answers "which nodes are near this point", and its cell holds a
        // fixed number of nodes however big the graph is (§25.1). Bounded against the
        // small graph rather than against a constant, so the criterion keeps meaning
        // the same thing if the generated graph's density changes.
        criteria.push(Criterion {
            name: "node_picking_does_not_follow_graph_size",
            claim: "picking a node examines a bounded number of geometries",
            kind: Kind::Counter,
            measured: large.node_tests,
            bound: small.node_tests * 1.5 + 8.0,
            unit: "geometries/pick",
        });

        // Curves are chosen from the recorded set, which is bounded by the viewport
        // region. The failure this guards against is a pick that walks the edge list:
        // at 16 000 nodes that is about 32 000 curves against a bound of a few
        // hundred.
        criteria.push(Criterion {
            name: "link_picking_does_not_follow_edge_count",
            claim: "picking a link examines a bounded number of curves",
            kind: Kind::Counter,
            measured: large.curve_tests,
            bound: small.curve_tests * 1.5 + 8.0,
            unit: "curves/pick",
        });
    }

    if let Some(last) = picks.last()
        && last.corners > 0
    {
        // §6.1: the hit geometry is the shape, not the box. Counted from the failing
        // side, as the criteria always are: a positive claim would have to be
        // inverted, and a count of wrong answers is a bound like any other.
        criteria.push(Criterion {
            name: "picking_is_by_shape_not_by_box",
            claim: "a point in the box but off the shape does not pick the node",
            kind: Kind::Counter,
            measured: picks.iter().map(|p| p.corner_hits).sum::<usize>() as f64,
            bound: 1.0,
            unit: "corners wrongly on a node",
        });
    }

    let (changes, samples) = zoom_picks;
    if samples > 0 {
        // The tolerance is in screen pixels, so the same screen offset must give the
        // same answer at every zoom (§25.2). With the tolerance in canvas units
        // instead, a 400-fold range of zoom turns it into a 400-fold range of
        // tolerance, and the near samples stop being hits at the bottom of it.
        criteria.push(Criterion {
            name: "picking_means_the_same_at_every_zoom",
            claim: "a pick at a fixed screen offset does not depend on the zoom",
            kind: Kind::Counter,
            measured: changes as f64,
            bound: 1.0,
            unit: "verdicts changed",
        });
    }

    // --- The widget budget (§29).

    let budget = DetailBudget::default();
    let widest = |points: &[ZoomRecord]| points.iter().map(|p| p.widgets).max().unwrap_or(0) as f64;

    if !zooms.is_empty() {
        // What the whole task is for. Before the budget the same sweep peaked at 4507
        // widgets and 31 ms a frame, at a zoom the readability rule was perfectly
        // happy with. The bound leaves a fifth over the ceiling: the canvas budgets in
        // nodes times a per-level cost, and the tree also holds the canvas's own three
        // widgets and whatever the application wraps it in.
        criteria.push(Criterion {
            name: "tree_stays_within_the_widget_budget",
            claim: "no zoom puts more widgets in the tree than the budget",
            kind: Kind::Counter,
            measured: widest(zooms),
            bound: budget.widgets as f64 * 1.2,
            unit: "widgets in tree",
        });
    }

    if !dense_zooms.is_empty() {
        // The failure a zoom threshold cannot avoid: it is a constant tuned for one
        // node size. Four times the density is four times the tree at every zoom, and
        // only a rule stated in widgets holds both. Bounded against the sparse sweep
        // rather than against the ceiling, so it keeps meaning the same thing if the
        // budget is retuned.
        criteria.push(Criterion {
            name: "the_budget_does_not_follow_node_density",
            claim: "a four-times denser graph holds no more widgets",
            kind: Kind::Counter,
            measured: widest(dense_zooms),
            bound: widest(zooms) * 1.5 + 8.0,
            unit: "widgets in tree",
        });
    }

    // Hysteresis. Measured at one switch in forty frames; without the margin it is
    // one per frame, and each one rebuilds every visible node.
    criteria.push(Criterion {
        name: "a_wobble_at_the_boundary_does_not_reswitch",
        claim: "a zoom wobble at the budget boundary does not flip level",
        kind: Kind::Counter,
        measured: wobble,
        bound: 0.2,
        unit: "level switches/frame",
    });

    if let Some(pan) = find("pan") {
        // The other end of the policy: ordinary work at zoom 1 is two orders of
        // magnitude inside the budget, so the policy must not be in the loop at all.
        criteria.push(Criterion {
            name: "ordinary_work_does_not_reach_the_policy",
            claim: "panning at zoom 1 never switches detail level",
            kind: Kind::Counter,
            measured: pan.level_switches_per_frame(),
            bound: 0.05,
            unit: "level switches/frame",
        });
    }

    // --- What the frame costs when there are no widgets left to count (§31).
    //
    // The budget of §29 bounds the tree; below the far-field threshold the tree is
    // three widgets and the canvas still fills the plan with the scene it painted.
    // Draw commands are the unit that half of the frame is charged in — the paint
    // pass rebuilds the plan every frame and re-appends every cached scene into it,
    // so a command is paid for whether or not anything about it changed.
    //
    // The claim is that a far-field frame costs the number of *styles* on screen and
    // not the number of nodes: six tints in six fills, the links in one stroke, and
    // the editor's own furniture. Measured: 13, at every zoom in the far field and on
    // both graphs, against 5000 visible nodes and ~8500 recorded curves. Break the
    // batch — a fill per node, a stroke per link — and it is 13 000.
    let far_commands = zooms
        .iter()
        .chain(dense_zooms)
        .filter(|p| p.detail.contains("Box"))
        .map(|p| p.commands)
        .max();
    if let Some(commands) = far_commands {
        criteria.push(Criterion {
            name: "a_far_field_frame_costs_styles_not_nodes",
            claim: "the far field draws a command per colour, not per node",
            kind: Kind::Counter,
            measured: commands as f64,
            bound: 64.0,
            unit: "draw commands in the plan",
        });
    }

    // --- The short-link rule stays where it belongs (§31.4).
    //
    // A link under two screen pixels is dropped when the set is chosen. The rule is
    // keyed on the curve's own on-screen size and not on the detail level, precisely
    // so that the widget budget never gets to decide whether the graph's structure is
    // visible — and this is what says the two have not become entangled: at every
    // sweep point where the canvas still materialises nodes, on both graphs and both
    // legs, it hides nothing at all. Raise the threshold and it fires at `Simplified`.
    let hidden_with_widgets = zooms
        .iter()
        .chain(dense_zooms)
        .filter(|p| !p.detail.contains("Box"))
        .map(|p| p.hidden_links)
        .max();
    if let Some(hidden) = hidden_with_widgets {
        criteria.push(Criterion {
            name: "the_short_link_rule_only_fires_in_the_far_field",
            claim: "no link is hidden at a zoom whose nodes are still widgets",
            kind: Kind::Counter,
            measured: hidden as f64,
            bound: 1.0,
            unit: "links hidden while nodes are widgets",
        });
    }

    // --- §35: the far field, in the unit its half of the frame is charged in.
    //
    // Draw commands are the cheap half (§32.2), and the criterion above bounds them.
    // What the rasteriser is charged for is path segments, and what the far field puts
    // in a frame is four per node plus two per link — the corner rounding, which is the
    // other four per node, is dropped while it is sub-pixel (§35.2). Counted as the
    // excess over that model, so it fails from the side that costs a frame: restore the
    // rounding at an overview zoom and it is twenty thousand segments over.
    let far_excess = far
        .iter()
        .filter(|row| row.what == "plain nodes" && row.zoom < 0.05)
        .map(|row| {
            row.segments
                .saturating_sub(4 * row.recorded_nodes as u64)
                .saturating_sub(2 * row.recorded_links as u64)
        })
        .max();
    if let Some(excess) = far_excess {
        criteria.push(Criterion {
            name: "a_far_field_node_costs_four_segments",
            claim: "the far field draws four segments a node and two a link",
            kind: Kind::Counter,
            measured: excess as f64,
            bound: 64.0,
            unit: "segments beyond the model",
        });
    }

    // The other side of the same trade: the margin exists so that panning re-uses the
    // recorded scene instead of choosing a new one (§20.6a), and §35.2 priced what it
    // costs in segments. Set the margin to zero and this is 1.00 — every frame.
    let far_records = far
        .iter()
        .filter(|row| row.what == "plain nodes")
        .map(|row| row.far_records)
        .fold(0.0_f64, f64::max);
    if !far.is_empty() {
        criteria.push(Criterion {
            name: "panning_does_not_re_record_the_far_field",
            claim: "panning inside the recorded region re-uses it",
            kind: Kind::Counter,
            measured: far_records,
            bound: 0.1,
            unit: "re-recordings/frame",
        });
    }

    criteria
}
