//! What keeping the pixels of idle areas is worth, and what it costs (§36).
//!
//! Its own file for the reason the far-field table has one: it is a table rather than a
//! scenario — the same screen and the same gesture, with the host told to keep the
//! layers or not — and it is the only part of this benchmark that needs a graphics
//! device.

use std::time::{Duration, Instant};

use area_screen::{Screen, ScreenSpec};
use blazy::areas::AreaScreen;
use blazy::canvas::CanvasLayer;
use blazy::masonry::core::NewWidget;
use blazy::masonry::dpi::PhysicalSize;
use blazy::masonry::testing::TestHarness;
use blazy::masonry::theme::default_property_set;
use node_canvas::editor::NodeEditor;

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
    /// Per frame: layers the cache said it would copy and then had no pixels for.
    ///
    /// Zero by construction, and measured because it was not: an eviction could drop a
    /// texture the same frame had already decided to reuse, and the copy then did
    /// nothing at all. The area went empty for a frame while every other counter here
    /// said the cache was working (§44.9).
    pub(crate) dropped: f64,
    /// Per frame: registered layers whose rectangle overlapped another's.
    ///
    /// The half of §36's precondition the host can check, and the one that failed: an
    /// area painting outside its own box moves its layer rectangle onto its neighbour,
    /// which is what put the cache over its ceiling in the first place.
    pub(crate) overlaps: f64,
}

/// Asks every area to repaint, the way the shell does before a frame (§36.1).
///
/// Not a method on `AreaScreen`: keeping a layer alive is the host's job — the window
/// loop does it for the ids `ShellDriver::layers` returns — and a library method that
/// exists only so a harness can imitate the host is a second way to say one thing.
fn keep_layers(harness: &mut TestHarness<Screen>) {
    for id in harness.root_widget().area_ids() {
        harness.edit_widget_with_id(id, |mut widget| widget.ctx.request_paint_only());
    }
}

/// A screen whose areas declare scene layers, at a zoom where the frame is expensive.
fn layered_harness(areas: usize, nodes: usize) -> TestHarness<Screen> {
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

/// The same screen, with the operator layer inside every area (§38.5).
///
/// The row it exists for asks where a gesture's pixels land. A selection outline is
/// drawn by the driver *inside* its area, so it dirties that area's layer and no
/// other; an overlay across the window would dirty every one of them, and worse, it
/// would break the condition the cache cannot check — that a cached layer owns its
/// rectangle (§36.4).
fn ops_layered_harness(areas: usize, nodes: usize) -> TestHarness<Screen> {
    let (screen, _graph) = ScreenSpec::new(areas, nodes)
        .with_isolated_layers(true)
        .with_ops(true)
        .build();
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(screen),
        PhysicalSize::new(VIEWPORT.0, VIEWPORT.1),
    );
    let _ = harness.redraw();
    for area in 0..areas {
        zoom_editor(&mut harness, area, 0.05);
    }
    let _ = harness.redraw();
    harness
}

/// The main region of an area, whatever kind of widget it is.
fn main_region(harness: &TestHarness<Screen>, area: usize) -> blazy::masonry::core::WidgetId {
    let area_id = harness.root_widget().area_ids()[area];
    *harness
        .get_widget_with_id(area_id)
        .downcast::<blazy::areas::AreaContent>()
        .expect("every area holds a region stack")
        .region_ids()
        .last()
        .expect("an area has regions")
}

/// Zooms the canvas inside an area's editor.
fn zoom_editor(harness: &mut TestHarness<Screen>, area: usize, factor: f64) {
    let id = main_region(harness, area);
    harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            CanvasLayer::zoom_around(&mut canvas, blazy::masonry::kurbo::Point::new(200.0, 150.0), factor);
        });
    });
}

/// Where a node of an area's graph is on screen, in window coordinates.
fn node_on_screen(harness: &mut TestHarness<Screen>, area: usize, index: usize) -> blazy::masonry::kurbo::Point {
    let id = main_region(harness, area);
    let local = harness.edit_widget_with_id(id, |mut widget| {
        let mut editor = widget.downcast::<NodeEditor>();
        NodeEditor::with_canvas(&mut editor, |mut canvas| {
            let pos = CanvasLayer::child_pos(&mut canvas, index).unwrap_or_default();
            let centre = pos + blazy::masonry::kurbo::Vec2::new(80.0, 48.0);
            canvas.widget.view() * centre
        })
    });
    harness.get_widget_with_id(id).ctx().window_transform() * local
}

/// Right-clicks a node in one area, the way a user selects one.
///
/// At this zoom no node has a widget at all: the press is answered from the model
/// (§25.3), which is the whole reason a selection works in the far field.
fn click_node(harness: &mut TestHarness<Screen>, area: usize, index: usize) {
    let at = node_on_screen(harness, area, index);
    harness.mouse_move(at);
    harness.mouse_button_press(Some(blazy::masonry::ui_events::pointer::PointerButton::Secondary));
    harness.mouse_button_release(Some(blazy::masonry::ui_events::pointer::PointerButton::Secondary));
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
    /// Whether the areas carry the operator layer.
    ops: bool,
}

fn cache_case(
    gpu: &mut blazy::shell::gpu::GpuFrames,
    case: CacheCase,
    mut step: impl FnMut(&mut TestHarness<Screen>, usize),
) -> Option<CacheRow> {
    let CacheCase {
        what,
        areas,
        nodes,
        cached,
        budget,
        ops,
    } = case;
    let mut harness = if ops {
        ops_layered_harness(areas, nodes)
    } else {
        layered_harness(areas, nodes)
    };
    let budget = budget.unwrap_or_else(|| gpu.default_cache_budget());
    gpu.cache_budget(budget);
    gpu.cache_layers(if cached {
        harness.root_widget().area_ids()
    } else {
        Vec::new()
    });
    let logical = blazy::masonry::kurbo::Size::new(f64::from(VIEWPORT.0), f64::from(VIEWPORT.1));

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
    let mut previous: Vec<blazy::masonry::imaging::record::Scene> = Vec::new();
    for i in 0..CACHE_FRAMES {
        step(&mut harness, i);
        // Asked again every frame, exactly as `ShellDriver::layers` is: the set of areas
        // is not fixed once an operation can change it, and re-registering the same set
        // is what the shell does anyway.
        if cached {
            gpu.cache_layers(harness.root_widget().area_ids());
        }
        keep_layers(&mut harness);
        let (plan, _) = harness.redraw();
        layers = plan.layers.len();

        // The cache's own decision, timed on its own rather than inferred from the
        // difference between two paths: the rectangle a layer occupies, and whether its
        // scene is the one from last frame.
        previous.resize_with(plan.layers.len(), blazy::masonry::imaging::record::Scene::new);
        for (index, layer) in plan.layers.iter().enumerate() {
            let blazy::masonry::app::VisualLayerKind::Scene(scene) = &layer.kind else {
                continue;
            };
            // What the cache does and in the order it does it: compare first, and only
            // work out where a changed layer sits — the walk is the expensive half.
            let start = Instant::now();
            let changed = previous[index] != *scene;
            compare += start.elapsed();
            if changed {
                let start = Instant::now();
                let _ = blazy::shell::layers::scene_bounds(
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
        dropped: (counters.dropped - before.dropped) as f64 / frames,
        overlaps: (counters.overlaps - before.overlaps) as f64 / frames,
    })
}

/// What keeping the pixels of idle areas is worth, and what it costs.
pub(crate) fn cache_table(_opts: &Options, areas: usize, nodes: usize) -> Vec<CacheRow> {
    let mut rows = Vec::new();
    println!("\nlayer cache: {areas} areas over one graph, canvases at an overview zoom");
    let Ok(gpu) = blazy::shell::gpu::GpuFrames::offscreen(PhysicalSize::new(VIEWPORT.0, VIEWPORT.1)) else {
        print_cache(&rows);
        return rows;
    };
    let mut gpu = gpu.with_background(blazy::masonry::peniko::Color::from_rgb8(0x14, 0x14, 0x18));
    for cached in [false, true] {
        let case = |what| CacheCase {
            what,
            areas,
            nodes,
            cached,
            budget: None,
            ops: false,
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
        // A gesture rather than a pan: where a selection's pixels land is the question
        // §38.5 asks, and the answer is a column — one area drawn, the rest copied.
        // Two nodes alternately, because selecting the same node twice changes nothing
        // and a row that changes nothing would agree with any answer at all.
        rows.extend(cache_case(
            &mut gpu,
            CacheCase {
                ops: true,
                ..case("one area selects")
            },
            // Two nodes near the corner the areas are zoomed about, so that both
            // points are certainly inside area 0: at this zoom the whole graph is a
            // few hundred pixels wide and a node further along the row lands in the
            // area next door, where it would select something and prove nothing.
            |h, i| click_node(h, 0, if i % 2 == 0 { 3 } else { 5 }),
        ));
        // The set of layers *is* the set of areas, so an operation that changes the areas
        // changes what the cache holds — §41.2 asks whether it survives that. The join
        // happens once, part way through, so the row measures the frames on both sides of
        // it rather than the frame it happened in.
        let join = |h: &mut TestHarness<Screen>, i: usize| {
            if i != CACHE_FRAMES / 2 {
                return;
            }
            let sibling = h.root_widget().tree().joinable(0);
            h.edit_root_widget(|mut screen| {
                if let Some(sibling) = sibling {
                    assert!(AreaScreen::join(&mut screen, 0, sibling));
                }
            });
        };
        rows.extend(cache_case(&mut gpu, case("join"), join));
        // The same join with the cache already evicting, which is the case the ceiling
        // criterion is about: a set of layers that changes *while* the cache is at its
        // limit. Under the default ceiling nothing is ever dropped, so that row would
        // agree with a cache that had forgotten how to count.
        rows.extend(cache_case(
            &mut gpu,
            CacheCase {
                what: "join, small ceiling",
                budget: Some(u64::from(VIEWPORT.0) * u64::from(VIEWPORT.1) * 4 / 4),
                ..case("")
            },
            join,
        ));
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
        "  {:<20} {:>5} {:>6} {:>8} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {:>7} {:>8}",
        "gesture",
        "cache",
        "layers",
        "reused/f",
        "drawn/f",
        "walks/f",
        "evict/f",
        "drop/f",
        "over/f",
        "cmp ms",
        "walk ms",
        "gpu ms",
        "KiB",
        "max KiB"
    );
    for row in rows {
        println!(
            "  {:<20} {:>5} {:>6} {:>8.2} {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7.3} {:>7.3} {:>8.2} {:>7} {:>8}",
            row.what,
            if row.cached { "on" } else { "off" },
            row.layers,
            row.reused,
            row.drawn,
            row.walks,
            row.evictions,
            row.dropped,
            row.overlaps,
            row.compare_ms,
            row.walk_ms,
            row.gpu_ms,
            row.kib,
            row.budget_kib,
        );
    }
}
