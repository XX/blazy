// Copyright 2026 the blazy Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

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

use blazy_canvas::{CanvasLayer, CanvasStats};
use masonry::core::NewWidget;
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Point, Vec2};
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;

use crate::editor::NodeEditor;
use crate::{DEFAULT_NODES, build_canvas};

/// Viewport used for all scenarios.
const VIEWPORT: (u32, u32) = (1100, 750);

/// Frames per scenario. Enough to see a trend, short enough to stay interactive.
const FRAMES: usize = 120;

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

    fn child_layouts_per_frame(&self) -> f64 {
        (self.after.child_layouts - self.before.child_layouts) as f64 / self.frames as f64
    }

    fn print(&self) {
        println!(
            "{:<26} {:>7.3} ms/frame  worst {:>7.3} ms  child-layouts/frame {:>8.1}  visible {:>5}",
            self.name,
            self.mean_ms(),
            self.worst.as_secs_f64() * 1000.0,
            self.child_layouts_per_frame(),
            self.after.visible,
        );
    }
}

/// Reads the canvas counters out of the live widget tree.
fn stats(harness: &TestHarness<NodeEditor>) -> CanvasStats {
    // The editor's cached copy is refreshed at the end of every layout pass, so it
    // is the cheapest honest place to read from.
    harness.root_widget().stats()
}

fn new_harness(count: usize) -> TestHarness<NodeEditor> {
    let editor = NodeEditor::new(build_canvas(count));
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

/// Runs all scenarios and prints a report.
pub fn run(count: usize) {
    println!(
        "blazy Phase 0 - node canvas on masonry_core@main\n\
         nodes {count}, viewport {}x{}, {FRAMES} frames per scenario\n",
        VIEWPORT.0, VIEWPORT.1
    );

    let mut reports = Vec::new();

    // --- Scenario 1: idle.
    //
    // Nothing changes. The baseline cost of a frame in which no widget is dirty.
    {
        let mut harness = new_harness(count);
        reports.push(measure("idle", &mut harness, FRAMES, |_, _| {}));
    }

    // --- Scenario 2: pan.
    //
    // The claim under test. Panning changes one `Affine`; culling reruns, but every
    // child that was already laid out must early-return in `run_layout_on`. If
    // child-layouts/frame is roughly the number of nodes entering the viewport
    // rather than the number of visible nodes, the design holds.
    {
        let mut harness = new_harness(count);
        reports.push(measure("pan", &mut harness, FRAMES, |h, _| {
            h.edit_root_widget(|mut editor| {
                NodeEditor::with_canvas(&mut editor, |mut canvas| {
                    CanvasLayer::pan(&mut canvas, Vec2::new(-6.0, -2.0));
                });
            });
        }));
    }

    // --- Scenario 3: zoom.
    //
    // Same as pan, plus LOD threshold crossings. Those *should* cost a relayout —
    // that is what LOD is for — so the spikes are expected and worth seeing in the
    // worst-frame column.
    {
        let mut harness = new_harness(count);
        reports.push(measure("zoom", &mut harness, FRAMES, |h, i| {
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
        reports.push(measure("drag one node", &mut harness, FRAMES, |h, i| {
            let dx = ((i % 40) as f64 - 20.0) * 0.5;
            h.edit_root_widget(|mut editor| {
                NodeEditor::with_canvas(&mut editor, |mut canvas| {
                    let base = CanvasLayer::child_pos(&mut canvas, 0).unwrap_or(Point::ORIGIN);
                    CanvasLayer::move_child(&mut canvas, 0, Point::new(base.x + dx, base.y));
                });
            });
        }));
    }

    // --- Scenario 5: pointer movement over the canvas.
    //
    // Hover routing touches hit testing, which for a canvas means the clip path and
    // the bounding boxes. Cheap in principle; measured because "in principle" is
    // what Phase 0 exists to check.
    {
        let mut harness = new_harness(count);
        reports.push(measure("hover", &mut harness, FRAMES, |h, i| {
            let x = 200.0 + (i % 60) as f64 * 8.0;
            h.mouse_move(Point::new(x, 300.0));
        }));
    }

    // --- Scenario 6: zoomed out to Box LOD.
    //
    // At Box detail every node stashes its slider and checkbox, so the number of
    // laid-out widgets drops by roughly two thirds. This is the measurement that
    // says whether LOD is worth its complexity.
    {
        let mut harness = new_harness(count);
        harness.edit_root_widget(|mut editor| {
            NodeEditor::with_canvas(&mut editor, |mut canvas| {
                CanvasLayer::zoom_around(&mut canvas, Point::new(550.0, 375.0), 0.15);
            });
        });
        let _ = harness.redraw();
        reports.push(measure("pan at box LOD", &mut harness, FRAMES, |h, _| {
            h.edit_root_widget(|mut editor| {
                NodeEditor::with_canvas(&mut editor, |mut canvas| {
                    CanvasLayer::pan(&mut canvas, Vec2::new(-6.0, -2.0));
                });
            });
        }));
    }

    println!();
    for report in &reports {
        report.print();
    }

    let scaling = scaling_sweep();

    println!();
    verdict(&reports, count, &scaling);
}

/// One point of the scaling sweep.
struct ScalePoint {
    nodes: usize,
    visible: usize,
    idle_ms: f64,
    pan_ms: f64,
}

/// Measures frame cost against total node count, with the visible count held fixed.
///
/// This is the sweep that decides whether culling is sufficient. If frame cost
/// tracks the *visible* set, stashing off-screen nodes is enough. If it tracks the
/// *total*, then the widget tree itself is the cost and the nodes have to leave the
/// tree entirely — which is virtualisation, not culling.
fn scaling_sweep() -> Vec<ScalePoint> {
    const COUNTS: [usize; 4] = [250, 1000, 4000, 16000];
    const SWEEP_FRAMES: usize = 40;

    println!("\nscaling: frame cost vs total nodes (visible set stays ~constant)");
    let mut points = Vec::new();

    for nodes in COUNTS {
        let mut harness = new_harness(nodes);
        let idle = measure("idle", &mut harness, SWEEP_FRAMES, |_, _| {});

        let mut harness = new_harness(nodes);
        let pan = measure("pan", &mut harness, SWEEP_FRAMES, |h, _| {
            h.edit_root_widget(|mut editor| {
                NodeEditor::with_canvas(&mut editor, |mut canvas| {
                    CanvasLayer::pan(&mut canvas, Vec2::new(-6.0, -2.0));
                });
            });
        });

        let point = ScalePoint {
            nodes,
            visible: pan.after.visible,
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

/// Prints the pass criteria from the Phase 0 brief, checked against the numbers.
fn verdict(reports: &[Report], count: usize, scaling: &[ScalePoint]) {
    let find = |name: &str| reports.iter().find(|r| r.name == name);

    println!("Phase 0 criteria");

    if let Some(pan) = find("pan") {
        let per_frame = pan.child_layouts_per_frame();
        let visible = pan.after.visible as f64;
        // A pan should only lay out nodes newly entering the viewport. Anything
        // approaching the visible count means every visible node is being relaid
        // out, which is the failure mode this design exists to avoid.
        let ok = per_frame < visible * 0.5;
        println!(
            "  [{}] panning does not relayout the visible set  ({:.1} child layouts/frame vs {:.0} visible)",
            mark(ok),
            per_frame,
            visible
        );
    }

    if let Some(drag) = find("drag one node") {
        let per_frame = drag.child_layouts_per_frame();
        // Moving one node should lay out one node.
        let ok = per_frame < 4.0;
        println!(
            "  [{}] dragging one node relayouts ~one node  ({:.1} child layouts/frame)",
            mark(ok),
            per_frame
        );
    }

    if let Some(pan) = find("pan") {
        let ok = pan.after.visible < count / 4;
        println!(
            "  [{}] culling keeps the visible set small  ({} of {count} nodes visible)",
            mark(ok),
            pan.after.visible
        );
    }

    if let (Some(pan), Some(boxed)) = (find("pan"), find("pan at box LOD")) {
        // Not a pass/fail criterion, just the datum that decides whether LOD earns
        // its complexity.
        println!(
            "  [i] LOD effect: {:.3} ms/frame at full detail vs {:.3} ms/frame at box detail",
            pan.mean_ms(),
            boxed.mean_ms()
        );
    }

    if let Some(idle) = find("idle") {
        println!(
            "  [i] idle frame costs {:.3} ms  (nothing is dirty; this is the floor)",
            idle.mean_ms()
        );
    }

    // The decisive test: does frame cost follow the visible set or the whole tree?
    if let (Some(first), Some(last)) = (scaling.first(), scaling.last()) {
        let node_ratio = last.nodes as f64 / first.nodes as f64;
        let time_ratio = last.pan_ms / first.pan_ms;
        // Linear in total nodes means culling is not buying anything: the passes
        // walk the tree whether or not a widget is stashed.
        let linear = time_ratio > node_ratio * 0.5;
        println!(
            "  [{}] frame cost is independent of total node count  \
             ({:.0}x the nodes costs {:.1}x the time, visible set {} -> {})",
            mark(!linear),
            node_ratio,
            time_ratio,
            first.visible,
            last.visible,
        );
        if linear {
            println!(
                "      Stashing keeps nodes out of paint and hit-testing, but not out of the\n      \
                 pass recursion: `paint_widget` walks every child deliberately (\"to avoid\n      \
                 creating zombie flags\"), and `set_transform` propagates `transform_changed`\n      \
                 to every descendant, so panning composes the whole tree. Culling is\n      \
                 necessary but not sufficient; nodes must leave the tree, as `virtual_scroll`\n      \
                 does. See rnd/architecture.md section 7.2."
            );
        }
    }

    let _ = (count, DEFAULT_NODES);
}

fn mark(ok: bool) -> &'static str {
    if ok { "pass" } else { "FAIL" }
}
