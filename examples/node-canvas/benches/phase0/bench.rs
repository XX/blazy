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

use bench_utils::criteria::{Criterion, Kind, Outcome, ScenarioRecord, SweepRecord};
use blazy_canvas::{CanvasLayer, CanvasStats};
use masonry::core::NewWidget;
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Point, Vec2};
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;
use node_canvas::build_canvas_with;
use node_canvas::editor::NodeEditor;

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
}

impl Report {
    fn mean_ms(&self) -> f64 {
        self.total.as_secs_f64() * 1000.0 / self.frames as f64
    }

    fn worst_ms(&self) -> f64 {
        self.worst.as_secs_f64() * 1000.0
    }

    fn child_layouts_per_frame(&self) -> f64 {
        self.per_frame(self.after.counters.child_layouts - self.before.counters.child_layouts)
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

    fn per_frame(&self, delta: u64) -> f64 {
        delta as f64 / self.frames as f64
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
                ("link_repaints_per_frame", self.link_repaints_per_frame()),
                ("link_reselects_per_frame", self.link_reselects_per_frame()),
                ("slot_visits_per_frame", self.slot_visits_per_frame()),
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
    }
}

/// Pans the canvas by one step, as a scenario body.
fn pan_step(harness: &mut TestHarness<NodeEditor>, delta: Vec2) {
    harness.edit_root_widget(|mut editor| {
        NodeEditor::with_canvas(&mut editor, |mut canvas| CanvasLayer::pan(&mut canvas, delta));
    });
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

/// Reads the canvas counters out of the live widget tree.
fn stats(harness: &TestHarness<NodeEditor>) -> CanvasStats {
    // The editor's cached copy is refreshed at the end of every layout pass, so it
    // is the cheapest honest place to read from.
    harness.root_widget().stats()
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

    for i in 0..frames {
        let start = Instant::now();
        step(harness, i);
        // `redraw` runs the rewrite passes and encodes the visual layer plan. That
        // is the frame, minus GPU submission.
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
        after: stats(harness),
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

    // --- Scenario 5: pointer movement over the canvas. Informational.
    //
    // Hover routing touches hit testing, which for a canvas means the clip path and
    // the bounding boxes. Cheap in principle; measured because "in principle" is
    // what Phase 0 exists to check.
    if !opts.quick {
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

    let outcome = Outcome {
        nodes: count,
        viewport: VIEWPORT,
        quick: opts.quick,
        criteria: evaluate(&reports, count, &sweep, &links),
        scenarios: reports.iter().map(Report::record).collect(),
        sweep,
    };
    outcome.report("Phase 0 criteria");
    outcome
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

/// The Phase 0 pass criteria, evaluated against the numbers just measured.
///
/// Bounds are set well clear of the measured values (§20.5), because a criterion is
/// a regression alarm, not a performance target: it must fire when the architecture
/// stops holding, and stay silent through ordinary tuning. A criterion whose scenario
/// did not run is simply absent — that is how the quick set drops the ones it cannot
/// decide, rather than passing them by default.
fn evaluate(reports: &[Report], count: usize, sweep: &[SweepRecord], links: &[SweepRecord]) -> Vec<Criterion> {
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

    criteria
}
