//! The far field in the unit its half of the frame is charged in (§35).
//!
//! Its own file because it is a table rather than a scenario: one graph at one zoom,
//! with one decision changed per row, and both halves of the frame timed next to the
//! counter that explains them. What the rest of the benchmark measures is the widget
//! tree; what this measures is what the rasteriser is handed.

use std::time::Instant;

use blazy::masonry::core::NewWidget;
use blazy::masonry::dpi::PhysicalSize;
use blazy::masonry::testing::TestHarness;
use node_canvas::editor::NodeEditor;
use node_canvas::{CanvasSpec, property_set};

use crate::bench::{Options, PAN_STEP, ScenarioRecord, VIEWPORT, look_at, measure, node_rect, pan_step, stats};

// --- MARK: the far field, in segments (§35)

/// Zooms the far-field measurements are taken at.
///
/// Two, because they answer different questions. At 0.02 the whole graph is on screen
/// and the recorded region is not the binding constraint — the graph is. At 0.10 the
/// canvas is still in the far field but the graph continues past the region, which is
/// the only situation in which the recorded margin costs anything (§35.2).
pub(crate) const FAR_ZOOMS: [f64; 2] = [0.02, 0.10];

/// Frames the far-field rasterisation is timed over.
///
/// Few, because each one is tens of milliseconds — that being the finding (§32.3).
const FAR_RASTER_FRAMES: usize = 6;

/// The two paths to pixels, opened once for a whole table.
///
/// One device per table rather than one per row, for the reason the host benchmark
/// gives: a driver initialisation inside a measurement is a variable nobody asked for,
/// and §32.4 found that churning the target provokes frames the GPU never draws.
struct Rasterisers {
    /// `false` in the quick set, where the tables are read for counters and not times.
    timed: bool,
    host: Option<blazy::shell::Host>,
    gpu: Option<blazy::shell::gpu::GpuFrames>,
}

impl Rasterisers {
    fn open(timed: bool) -> Self {
        Self {
            timed,
            host: timed.then(|| blazy::shell::Host::any().ok()).flatten(),
            gpu: timed
                .then(|| blazy::shell::gpu::GpuFrames::offscreen(PhysicalSize::new(VIEWPORT.0, VIEWPORT.1)).ok())
                .flatten(),
        }
    }

    /// Rasterises the plan on the blit path and returns milliseconds per frame.
    fn blit_ms(&mut self, plan: &blazy::masonry::app::VisualLayerPlan) -> f64 {
        let Some(host) = self.host.as_mut() else {
            return 0.0;
        };
        let size = blazy::masonry::kurbo::Size::new(f64::from(VIEWPORT.0), f64::from(VIEWPORT.1));
        let _ = host.render(plan, size);
        let start = Instant::now();
        for _ in 0..FAR_RASTER_FRAMES {
            let _ = host.render(plan, size);
        }
        start.elapsed().as_secs_f64() * 1000.0 / FAR_RASTER_FRAMES as f64
    }

    /// The same on the GPU path, where a device opened. Zero where none did.
    fn gpu_ms(&mut self, plan: &blazy::masonry::app::VisualLayerPlan) -> f64 {
        let Some(gpu) = self.gpu.as_mut() else {
            return 0.0;
        };
        let size = blazy::masonry::kurbo::Size::new(f64::from(VIEWPORT.0), f64::from(VIEWPORT.1));
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
}

/// One far-field configuration: what it records, and what that costs.
pub(crate) struct FarRow {
    pub(crate) what: &'static str,
    pub(crate) zoom: f64,
    pub(crate) nodes: usize,
    /// Nodes and link curves in the recording — what the scene is made of.
    pub(crate) recorded_nodes: usize,
    pub(crate) recorded_links: usize,
    /// Link curves the short-link rule dropped as too small to see (§31.4).
    pub(crate) hidden_links: usize,
    /// What vello would be asked to draw (§35.1).
    pub(crate) objects: usize,
    pub(crate) segments: u64,
    /// Decisions per frame: re-choosing the recorded sets is what a margin buys off.
    pub(crate) far_records: f64,
    pub(crate) link_reselects: f64,
    /// Masonry's half of the frame — passes and plan assembly.
    pub(crate) plan_ms: f64,
    /// The other half, on the blit path, and on the GPU path where there is a device.
    pub(crate) cpu_ms: f64,
    pub(crate) gpu_ms: f64,
}

/// Path segments in the frame the canvas would hand the rasteriser.
///
/// Outside the clock, like every other counter here: encoding the scene is the
/// measurement, not the frame (§35.1).
pub(crate) fn segments_of(harness: &mut TestHarness<NodeEditor>) -> u64 {
    let (plan, _) = harness.redraw();
    let composed = blazy::shell::Composition::new(&plan, 1.0).scene;
    blazy::shell::encode::segments(&composed, PhysicalSize::new(VIEWPORT.0, VIEWPORT.1))
}

/// One row of the far-field table: a graph, a zoom, and one decision changed.
#[derive(Clone)]
struct FarCase {
    pub(crate) what: &'static str,
    count: usize,
    pub(crate) zoom: f64,
    links: Vec<blazy::canvas::Link>,
    tuning: node_canvas::FarTuning,
    frames: usize,
}

/// Measures one far-field configuration over a pan.
fn far_case(case: FarCase, paths: &mut Rasterisers) -> FarRow {
    let FarCase {
        what,
        count,
        zoom,
        links,
        tuning,
        frames,
    } = case;
    let (canvas, _graph) = CanvasSpec::new(count).with_links(links).with_far(tuning).build();
    let mut harness = TestHarness::create_with_size(
        property_set(),
        NewWidget::new(node_canvas::editor::new(canvas)),
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
    let composed = blazy::shell::Composition::new(&plan, 1.0).scene;
    let encoded = blazy::shell::encode::encoded(&composed, frame);

    let (mut cpu_ms, mut gpu_ms) = (0.0, 0.0);
    if paths.timed {
        cpu_ms = paths.blit_ms(&plan);
        gpu_ms = paths.gpu_ms(&plan);
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

/// What a far-field frame is made of, and what each lever takes off it.
///
/// The table §35 is argued from. Every row is the same graph at the same zoom; what
/// changes is one decision at a time.
pub(crate) fn far_table(opts: &Options, count: usize, zoom: f64) -> Vec<FarRow> {
    let frames = opts.frames();
    let mut paths = Rasterisers::open(!opts.quick);
    let links = node_canvas::generated_links(count);
    let default = node_canvas::FarTuning::default();
    let with = |overscan: f64| node_canvas::FarTuning { overscan, ..default };
    let links_at = |min_link_px: f64| node_canvas::FarTuning { min_link_px, ..default };

    println!("\nfar field: what the frame is made of at zoom {zoom} ({count} nodes)");
    let base = FarCase {
        what: "",
        count,
        zoom,
        links: links.clone(),
        tuning: default,
        frames,
    };
    let row = |what: &'static str, tuning, links: Option<Vec<blazy::canvas::Link>>| FarCase {
        what,
        tuning,
        links: links.unwrap_or_else(|| base.links.clone()),
        ..base.clone()
    };
    let rows = vec![
        far_case(
            row(
                "rounded nodes",
                node_canvas::FarTuning {
                    min_radius_px: 0.0,
                    ..default
                },
                None,
            ),
            &mut paths,
        ),
        far_case(row("plain nodes", default, None), &mut paths),
        far_case(row("plain, no links", default, Some(Vec::new())), &mut paths),
        far_case(row("overscan 0.50", with(0.50), None), &mut paths),
        far_case(row("overscan 0.10", with(0.10), None), &mut paths),
        far_case(row("overscan 0.00", with(0.0), None), &mut paths),
        far_case(row("links >= 4 px", links_at(4.0), None), &mut paths),
        far_case(row("links >= 8 px", links_at(8.0), None), &mut paths),
    ];
    print_far(&rows);
    rows
}

impl FarRow {
    /// The plain-data form the report archives, so the levers can be diffed across
    /// commits rather than re-argued.
    pub(crate) fn record(&self) -> ScenarioRecord {
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
