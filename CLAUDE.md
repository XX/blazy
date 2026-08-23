# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repository is

`blazy` is a **GUI library**: a Blender-style UI layer on top of
[Masonry](https://github.com/linebender/xilem) — screen areas and splits, a node canvas
with ordinary widgets inside the nodes, operators and a keymap. The end product is a
library other people build applications with.

It is early. Nothing is usable yet, and the work so far has gone into de-risking the
subsystems by measurement before building them out, because several of the load-bearing
assumptions turned out to be false when checked (§20.2, §22.1). Each phase asks one
architectural question, answers it with numbers, and writes the answer into
`rnd/architecture.md` as a numbered section. Phases 0 (node canvas), 0.5 (areas), 0.6
(regions and `ui_scale`), 1.1 (render and sharpness) and the shape hit test (§25) are
done; their checks run in CI on every push, so the answers keep holding rather than
becoming folklore.

Treat the examples as the current front line, not as demos: each one is where a
subsystem is being worked out before it moves into a crate. That it is a library rather
than an application also raises the stakes on two things the architecture document
already flags — a public API worth living with, and the git-pinned upstream (§15.1,
§18): an application can absorb churn in a dependency, a published library passes it on
to everyone downstream.

## Commands

Everything goes through `cargo-make`. `cargo fmt` needs nightly; everything else is
stable.

```bash
cargo make ci               # what CI runs: lint + tests + both benchmark gates
cargo make lint             # fmt --check + clippy -D warnings
cargo make fmt              # nightly rustfmt
cargo make test             # cargo test --workspace

cargo make run-node-canvas  # Phase 0 window
cargo make run-area-screen  # Phase 0.5/0.6 window

cargo make bench-canvas     # Phase 0 measurements and criteria
cargo make bench-areas      # Phase 0.5/0.6 measurements and criteria
cargo make bench            # both
cargo make bench-report     # both, plus JSON reports into target/
```

**Pass arguments without a `--` separator.** cargo-make forwards the separator itself
as an argument, and clap rejects it:

```bash
cargo make bench-canvas --quick --nodes 250   # correct
cargo make bench-canvas -- --quick            # broken: clap sees a bare `--`
cargo make run-area-screen --areas 16
```

A single test:

```bash
cargo test -p blazy-areas ui_scale        # substring filter
cargo test -p node-canvas --lib
```

`--quick` runs only the scenarios the criteria are computed from — same verdict,
fewer numbers, and a fast inner loop.

## Crate layout

| Crate | Role |
|---|---|
| `crates/blazy` | Facade — the crate an application depends on. Re-exports the rest. |
| `crates/blazy-canvas` | Virtualised, zoomable canvas: nodes, links, spatial index. |
| `crates/blazy-areas` | Split tree, areas, regions, per-region `ui_scale`. |
| `crates/blazy-shape` | Shape-accurate hit testing, for widgets and for bare curves. |
| `crates/bench-utils` | Criteria, verdict, JSON report, and render metrics. |
| `examples/node-canvas` | Phase 0 experiment: 5000 nodes, measurements, criteria. |
| `examples/area-screen` | Phase 0.5/0.6 experiment: tiled screen, regions, criteria. |

Each example is a **library plus a thin binary plus a bench target**, not one binary.
That is forced: `benches/` targets are separate crates and can only reach the
package's lib, so a binary-only layout would mean duplicating the graph generator that
every measurement depends on being identical. `[lib]` and `[[bin]]` carry
`bench = false` so `cargo bench` does not also run their libtest harnesses.

`examples/area-screen` depends on `examples/node-canvas` on purpose: reusing the graph
model rather than copying it is what keeps the two sets of numbers comparable.

## The measurement discipline

This is the part that is easiest to break by accident.

**Criteria are gated on deterministic counters, never on milliseconds.** A counter —
child layouts per frame, widgets in the tree, area resizes per frame — is identical on
a laptop and on a shared CI runner, so a threshold on one either holds or reports a
real regression. Wall-clock times on a shared runner move by a factor that would force
any honest threshold so wide it stops catching anything. Times are printed and archived
into the JSON report; they are never a bound. The one exception is a claim about time
that cannot be restated as a counter, and it is marked `Kind::Timing` and given an
enormous margin. The full argument is in `crates/bench-utils/src/criteria.rs`.

Consequences to respect when touching benchmarks:

- A benchmark exits non-zero when a criterion fails, which is what makes CI a gate.
- An **empty criteria list is a failure**, not a pass. Otherwise the easiest way to
  turn CI green is to rename a scenario so `evaluate` stops finding it.
- A `Criterion` is `measured < bound`. A positive claim ("this must happen") is
  expressed by counting from the failing side — e.g. "scale changes the region did not
  see" — rather than by inverting the comparison.
- New criteria should be checked for vacuity: break the thing on purpose once and
  confirm the criterion fires.
- **Debug builds inflate dirty-flag counters.** `run_layout_on` deliberately marks
  every child as needing layout under `debug_assertions` so it can check the parent
  visited them all. Counters derived from `child_needs_layout` are only meaningful
  under the `bench` profile. Counters derived from observed size changes are honest in
  both, which is why `blazy-areas` counts resizes rather than dirty flags.

## Architecture notes that took measurement to learn

These are load-bearing and not obvious from any single file. Section numbers refer to
`rnd/architecture.md`, which is the document of record.

**Frame cost is the cost of walking the widget tree, and the tree is per window
(§20.2).** Culling with `set_stashed` does not help: a stashed widget stays in the
pass recursion. Nodes have to leave the tree entirely. `blazy-canvas` therefore
*virtualises* — geometry for every node, a widget only while it is in view — and node
state lives in the model, not in the widget, because the widget does not exist most of
the time. 64× the nodes costs 1.1× the time.

**Links are curves, not widgets, and are chosen through adjacency (§24).** There are
as many edges as nodes and one with a single endpoint on screen still has to be drawn,
so an edge that is a widget puts the linear cost straight back. The curve scene lives
in canvas coordinates like the far field, so a pan reuses it. Keep the two flags
apart: dragging a node **repaints** its curves without **reselecting** which curves are
on screen — conflating them walks the region once per frame.

**The visible set comes from a uniform grid, not a scan (§24.2).** Query slack is
derived from the widest node rather than assumed to be one cell: a node wider than a
cell would otherwise stop being drawn near the left edge, silently.

**Sweep endpoints are part of a criterion, at both ends.** The node sweep stopped at
16 000 and the "frame cost does not follow graph size" criterion passed for months while
a linear cost sat there — it only shows above 64 000 (§24.1). The picking sweep had the
opposite fault: it started at 250 nodes, a graph *smaller* than the region the canvas
records links for, so the low end measured the graph running out rather than the index
working (§25.4). When adding a criterion, check where the effect it guards against
actually becomes visible — and where it is still hidden by something else.

**The canvas is two widgets and they cannot be merged (§20.3).** `CanvasLayer` holds
the viewport, the clip path and the view; `CanvasContent` carries the view transform
and owns the placed children. A single widget holding both clip and view would zoom
its own viewport clip along with the content.

**Below a readability threshold the canvas stops building widgets and paints the nodes
itself (§20.6).** The far-field scene is recorded in canvas coordinates, so panning
and zooming inside the recorded region reuse it untouched. Level of detail is a
decision about *which widgets to build*, not about how to draw them (§20.7).

**Areas do not add up (§21).** `SplitTree` is pure geometry and knows nothing of
widgets; `AreaScreen` places one child per area at the rect the tree computed. Keep
that seam: the tree is what will later be serialised into a workspace file.
Rectangles are **rounded to whole pixels**, and that is load-bearing rather than
cosmetic — a fractional boundary makes every area count as resized on every frame of a
drag.

**Picking asks the model, not the widget tree (§25.3).** A node below the far-field
threshold has no widget and a link never has one, so `find_widget_under_pointer` cannot
answer for either; `CanvasLayer::hit_test` answers from the geometry and the recorded
curve set instead. Two consequences that are easy to undo by accident: the pick
tolerance is in **screen pixels** and divided by the scale at test time (canvas units
span a factor of 400 across the zoom range), and a pick must not run layout — which is
why its counters are published by the pick itself rather than by the layout pass.

**`ui_scale` goes into layout; `view` goes only into paint (§9, §22).** Mixing them
means re-running layout on every frame of a zoom. Four criteria and seven tests hold
that line.

**Sharpness is measured, not eyeballed (§23).** Magnifying by 4 must come out several
times sharper than the same content upscaled bilinearly. Use the mean **squared**
gradient: the sum of absolute differences is conserved under blurring and is blind to
it by construction — that mistake was made once already. The metric cannot tell a
redraw from a nearest-neighbour magnification, so it is paired with a check that the
image differs from block magnification. Snapshots are a viable gate here because the
harness pins its font, the rasteriser is CPU-side and the upstream rev is pinned;
regenerate with `MASONRY_TEST_BLESS=1`.

**The device scale factor is the host's job (§4.2, §23.4).** `VisualLayerPlan` comes
out in logical coordinates and `TestHarness::render()` never applies a scale factor, so
anything testing HiDPI has to replay the plan itself. Backend choice is compile-time in
upstream, but `ImageRenderer` is object-safe and the backend modules are independently
feature-gated, so a runtime choice is buildable here (§23.5) — it needs our own window
layer, not an upstream patch.

**Masonry has no inherited properties (§22.1).** A `PropertyStack` hangs off the
widget itself and `Selector` matches classes and state flags, never ancestry. The
working mechanism is `WidgetMut::insert_prop` → `Widget::property_changed` → the widget
asks for layout, one widget at a time; a container that wants its children scaled must
forward the value itself. **Masonry's own widgets do not honour `UiScale`** — nothing
reads it, and their sizes come from the theme's `DefaultProperties`, which is one map
per application. The test `masonry_widgets_do_not_follow_ui_scale` pins that down and
will fail loudly if upstream ever grows a per-subtree scale.

## Upstream dependency

`masonry` and `masonry_winit` are **git dependencies pinned to a commit** on purpose:
the rendering IR `imaging`, `Widget::paint(&mut Painter)` and `VisualLayerPlan` exist
only on git main, and the published 0.4.0 predates that migration. Living on two young
crates' main branch is the project's declared main risk (§15.1). The pin is in the
workspace `Cargo.toml`; a local checkout of the pinned tree is the fastest way to
answer "does Masonry let us do X" and is usually the right first step.

Strategy towards upstream is **contribute, not fork** (§17). Nothing here patches
Masonry.

## Because it is a library, not an application

Two things follow from the end product being something other people depend on, and
both are already argued in §15.1 and §18:

- **New public surface goes through `crates/blazy`.** A new subsystem crate is not
  finished until the facade re-exports it. That re-export is also the only place where
  upstream churn can be absorbed once instead of by every downstream application.
- **The git pin is a graver risk here than it would be in an application.** An app can
  swallow a breaking change in a dependency on its own schedule; a published library
  passes it on. That is what the facade and the criteria in CI are insurance for.

## Conventions

- **Prose docs are in Russian; code, rustdoc and comments are in English.** Follow
  both.
- Comments explain *why*, and especially why an obvious alternative was rejected.
  Several of them record a measurement that contradicted an assumption; do not delete
  those when refactoring the code around them.
- `rnd/architecture.md` is the document of record, and §16 is the plan the work
  follows. Finishing a phase means adding a numbered section with the numbers, what was
  disproved, and what it changes in §16 — not just landing the code.
- `issues/` holds tasks. When one is finished, append the outcome to the file **in
  place** — moving it into `issues/done/` is the user's call, not yours.
- Do not commit unless asked.
