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
    /// What deciding costs: the rectangle of each layer plus a scene comparison.
    pub(crate) decide_ms: f64,
    /// The frame on the GPU path.
    pub(crate) gpu_ms: f64,
    /// Texture the cache is holding, in KiB.
    pub(crate) kib: u64,
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
    pub(crate) what: &'static str,
    areas: usize,
    nodes: usize,
    /// Whether the host is told it may keep this screen's areas.
    pub(crate) cached: bool,
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
    } = case;
    let mut harness = layered_harness(areas, nodes);
    gpu.cache_layers(if cached {
        harness.root_widget().area_ids()
    } else {
        Vec::new()
    });
    let logical = masonry::kurbo::Size::new(f64::from(VIEWPORT.0), f64::from(VIEWPORT.1));

    // One frame to fill the cache, so the sweep measures the steady state rather than
    // the first frame of it.
    harness.edit_root_widget(|mut screen| AreaScreen::keep_layers(&mut screen));
    let (plan, _) = harness.redraw();
    gpu.draw(&plan, logical, 1.0).ok()?;
    gpu.wait();

    let before = gpu.layer_counters();
    let mut total = Duration::ZERO;
    let mut layers = 0;
    let mut decide = Duration::ZERO;
    let mut previous: Vec<masonry::imaging::record::Scene> = Vec::new();
    for i in 0..CACHE_FRAMES {
        step(&mut harness, i);
        harness.edit_root_widget(|mut screen| AreaScreen::keep_layers(&mut screen));
        let (plan, _) = harness.redraw();
        layers = plan.layers.len();

        // The cache's own decision, timed on its own rather than inferred from the
        // difference between two paths: the rectangle a layer occupies, and whether its
        // scene is the one from last frame.
        let start = Instant::now();
        previous.resize_with(plan.layers.len(), masonry::imaging::record::Scene::new);
        for (index, layer) in plan.layers.iter().enumerate() {
            let masonry::app::VisualLayerKind::Scene(scene) = &layer.kind else {
                continue;
            };
            // What the cache does and in the order it does it: compare first, and only
            // work out where a changed layer sits — the walk is the expensive half.
            if previous[index] != *scene {
                let _ = blazy_shell::layers::scene_bounds(
                    scene,
                    layer.transform,
                    PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
                );
                previous[index].clone_from(scene);
            }
        }
        decide += start.elapsed();

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
        decide_ms: decide.as_secs_f64() * 1000.0 / frames,
        gpu_ms: total.as_secs_f64() * 1000.0 / frames,
        kib: counters.bytes / 1024,
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
        "  {:<18} {:>7} {:>7} {:>8} {:>8} {:>8} {:>10} {:>9} {:>8}",
        "gesture", "cache", "layers", "reused/f", "drawn/f", "walks/f", "decide ms", "gpu ms", "KiB"
    );
    for row in rows {
        println!(
            "  {:<18} {:>7} {:>7} {:>8.2} {:>8.2} {:>8.2} {:>10.3} {:>9.2} {:>8}",
            row.what,
            if row.cached { "on" } else { "off" },
            row.layers,
            row.reused,
            row.drawn,
            row.walks,
            row.decide_ms,
            row.gpu_ms,
            row.kib,
        );
    }
}
