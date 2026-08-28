//! What keeping the pixels of idle areas is worth, and what it costs (§36).
//!
//! Its own file for the reason the far-field table has one: it is a table rather than a
//! scenario — the same screen and the same gesture, with the host told to keep the
//! layers or not — and it is the only part of this benchmark that needs a graphics
//! device.

use std::time::{Duration, Instant};

use area_screen::ScreenSpec;
use blazy_areas::AreaScreen;
use masonry::core::NewWidget;
use masonry::dpi::PhysicalSize;
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;

use crate::bench::{Options, PAN_STEP, VIEWPORT, pan_area, zoom_area};

// --- MARK: the layer cache (§36)

/// Frames each cache scenario is timed over. Few: each one is a whole GPU frame.
const CACHE_FRAMES: usize = 8;

/// What one gesture costs a screen of areas, with and without kept layers.
pub(crate) struct CacheRow {
    pub(crate) what: &'static str,
    pub(crate) cached: bool,
    /// Layers the plan carried on the last frame.
    pub(crate) layers: usize,
    /// Per frame: layers copied instead of drawn, and layers drawn.
    pub(crate) reused: f64,
    pub(crate) drawn: f64,
    /// Per frame: layers whose rectangle had to be worked out by walking their scene.
    ///
    /// The counter form of "deciding is cheap" (§36.3): a frame in which nothing changed
    /// must not touch any layer's geometry, and that is a fact about the code rather
    /// than about the machine.
    pub(crate) walks: f64,
    /// What deciding costs, split into its two halves: comparing the scene with the one
    /// from last frame, and — only for a layer that changed — working out where it sits.
    ///
    /// Split because the answer decides whether a cheap pre-filter before the comparison
    /// is worth building (§37.1): a filter can only speed up scenes that differ, and an
    /// idle screen's scenes are all equal.
    pub(crate) compare_ms: f64,
    pub(crate) walk_ms: f64,
    /// The frame on the GPU path.
    pub(crate) gpu_ms: f64,
    /// Texture the cache is holding, in KiB.
    pub(crate) kib: u64,
    /// The ceiling this row ran under, and how many textures it dropped to stay inside
    /// it (§37.2).
    pub(crate) budget_kib: u64,
    pub(crate) evictions: f64,
}

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

/// A screen whose areas declare scene layers, at a zoom where the frame is expensive.
fn layered_harness(areas: usize, nodes: usize) -> TestHarness<AreaScreen> {
    let (screen, _graph) = ScreenSpec::new(areas, nodes).with_isolated_layers(true).build();
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(screen),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    // Every canvas out at an overview zoom: the far field is what makes a frame cost
    // tens of milliseconds (§35.5), and therefore what a kept layer saves.
    for area in 0..areas {
        zoom_area(&mut harness, area, 0.05);
    }
    let _ = harness.redraw();
    harness
}

/// One gesture, on one path.
///
/// The device comes from the caller and serves the whole table: a driver initialisation
/// per row would sit inside the measurement, and §32.4 found that churning the target
/// provokes frames the GPU never draws. A row differs by what the cache is told to keep
/// — nothing at all, for the uncached ones.
#[derive(Clone, Copy)]
struct CacheCase {
    what: &'static str,
    areas: usize,
    nodes: usize,
    /// Whether the host is told it may keep this screen's areas.
    cached: bool,
    /// Bytes of texture the cache may hold, or `None` for what the frame size gives.
    budget: Option<u64>,
}

fn cache_case(
    gpu: &mut blazy_shell::gpu::GpuFrames,
    case: CacheCase,
    mut step: impl FnMut(&mut TestHarness<AreaScreen>, usize),
) -> Option<CacheRow> {
    let CacheCase {
        what,
        areas,
        nodes,
        cached,
        budget,
    } = case;
    let mut harness = layered_harness(areas, nodes);
    let budget = budget.unwrap_or_else(|| gpu.default_cache_budget());
    gpu.cache_budget(budget);
    gpu.cache_layers(if cached {
        harness.root_widget().area_ids()
    } else {
        Vec::new()
    });
    let logical = masonry::kurbo::Size::new(f64::from(VIEWPORT.0), f64::from(VIEWPORT.1));

    // One frame to fill the cache, so the sweep measures the steady state rather than
    // the first frame of it.
    keep_layers(&mut harness);
    let (plan, _) = harness.redraw();
    gpu.draw(&plan, logical, 1.0).ok()?;
    gpu.wait();

    let before = gpu.layer_counters();
    let mut total = Duration::ZERO;
    let mut layers = 0;
    let mut compare = Duration::ZERO;
    let mut walk = Duration::ZERO;
    let mut previous: Vec<masonry::imaging::record::Scene> = Vec::new();
    for i in 0..CACHE_FRAMES {
        step(&mut harness, i);
        keep_layers(&mut harness);
        let (plan, _) = harness.redraw();
        layers = plan.layers.len();

        // The cache's own decision, timed on its own rather than inferred from the
        // difference between two paths: the rectangle a layer occupies, and whether its
        // scene is the one from last frame.
        previous.resize_with(plan.layers.len(), masonry::imaging::record::Scene::new);
        for (index, layer) in plan.layers.iter().enumerate() {
            let masonry::app::VisualLayerKind::Scene(scene) = &layer.kind else {
                continue;
            };
            // What the cache does and in the order it does it: compare first, and only
            // work out where a changed layer sits — the walk is the expensive half.
            let start = Instant::now();
            let changed = previous[index] != *scene;
            compare += start.elapsed();
            if changed {
                let start = Instant::now();
                let _ = blazy_shell::layers::scene_bounds(
                    scene,
                    layer.transform,
                    PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
                );
                walk += start.elapsed();
                previous[index].clone_from(scene);
            }
        }

        let start = Instant::now();
        gpu.draw(&plan, logical, 1.0).ok()?;
        gpu.wait();
        total += start.elapsed();
    }

    let counters = gpu.layer_counters();
    let frames = CACHE_FRAMES as f64;
    Some(CacheRow {
        what,
        cached,
        layers,
        reused: (counters.reused - before.reused) as f64 / frames,
        drawn: (counters.drawn - before.drawn) as f64 / frames,
        walks: (counters.walks - before.walks) as f64 / frames,
        compare_ms: compare.as_secs_f64() * 1000.0 / frames,
        walk_ms: walk.as_secs_f64() * 1000.0 / frames,
        gpu_ms: total.as_secs_f64() * 1000.0 / frames,
        kib: counters.bytes / 1024,
        budget_kib: budget / 1024,
        evictions: (counters.evictions - before.evictions) as f64 / frames,
    })
}

/// What keeping the pixels of idle areas is worth, and what it costs.
pub(crate) fn cache_table(_opts: &Options, areas: usize, nodes: usize) -> Vec<CacheRow> {
    let mut rows = Vec::new();
    println!("\nlayer cache: {areas} areas over one graph, canvases at an overview zoom");
    let Ok(gpu) = blazy_shell::gpu::GpuFrames::offscreen(PhysicalSize::new(VIEWPORT.0, VIEWPORT.1)) else {
        print_cache(&rows);
        return rows;
    };
    let mut gpu = gpu.with_background(masonry::peniko::Color::from_rgb8(0x14, 0x14, 0x18));
    for cached in [false, true] {
        let case = |what| CacheCase {
            what,
            areas,
            nodes,
            cached,
            budget: None,
        };
        rows.extend(cache_case(&mut gpu, case("nothing changes"), |_, _| {}));
        rows.extend(cache_case(&mut gpu, case("one area pans"), |h, _| {
            pan_area(h, 0, PAN_STEP);
        }));
        // In the quick set too: it is the row that keeps the two criteria above from
        // passing on a sweep where nothing ever changes (§20.9).
        rows.extend(cache_case(&mut gpu, case("every area pans"), move |h, _| {
            for area in 0..areas {
                pan_area(h, area, PAN_STEP);
            }
        }));
        // A ceiling two areas wide, so the cache has to drop textures to stay inside it.
        // Without this row the criterion that says it does would pass by never being
        // asked (§20.9).
        rows.extend(cache_case(
            &mut gpu,
            CacheCase {
                what: "under a small ceiling",
                budget: Some(u64::from(VIEWPORT.0) * u64::from(VIEWPORT.1) * 4 / 4),
                ..case("")
            },
            |_, _| {},
        ));
    }
    print_cache(&rows);
    rows
}

fn print_cache(rows: &[CacheRow]) {
    if rows.is_empty() {
        println!("  no graphics device: the layer cache is not measured here");
        return;
    }
    println!(
        "  {:<20} {:>5} {:>6} {:>8} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {:>7} {:>8}",
        "gesture",
        "cache",
        "layers",
        "reused/f",
        "drawn/f",
        "walks/f",
        "evict/f",
        "cmp ms",
        "walk ms",
        "gpu ms",
        "KiB",
        "max KiB"
    );
    for row in rows {
        println!(
            "  {:<20} {:>5} {:>6} {:>8.2} {:>7.2} {:>7.2} {:>7.2} {:>7.3} {:>7.3} {:>8.2} {:>7} {:>8}",
            row.what,
            if row.cached { "on" } else { "off" },
            row.layers,
            row.reused,
            row.drawn,
            row.walks,
            row.evictions,
            row.compare_ms,
            row.walk_ms,
            row.gpu_ms,
            row.kib,
            row.budget_kib,
        );
    }
}
