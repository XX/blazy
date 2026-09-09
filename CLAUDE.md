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
(regions and `ui_scale`) and the whole of Phase 1 — render and sharpness (§23), the
shape hit test (§25), the host (§26) — are done; their checks run in CI on every push,
so the answers keep holding rather than becoming folklore.

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
cargo make ci               # what CI runs: lint + tests + deps rule + three benchmark gates
cargo make lint             # fmt --check + clippy -D warnings
cargo make fmt              # nightly rustfmt
cargo make test             # cargo test --workspace
cargo make deps-rule        # core crates must not depend on a window system (§3, §26.4)

cargo make run-node-canvas  # Phase 0 window
cargo make run-area-screen  # Phase 0.5/0.6 window

cargo make bench-canvas     # Phase 0 measurements and criteria
cargo make bench-areas      # Phase 0.5/0.6 measurements and criteria
cargo make bench-shell      # host measurements and criteria (§26)
cargo make bench            # all three
cargo make bench-report     # all three, plus JSON reports into target/
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
| `crates/blazy-ops` | Operators, keymap as data, modal stack, undo journal. |
| `crates/blazy-shape` | Shape-accurate hit testing, for widgets and for bare curves. |
| `crates/blazy-shell` | The host: window, event loop, composition, choice of rasteriser. |
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

**Where a benchmark lives is decided by what it needs, not by what it measures.** A
measurement that only needs the crate's own mechanism lives in the crate:
`crates/blazy-shell/benches/shell` defines the two widgets it draws in the bench file
itself, because composition, the device scale and external holes do not care what is on
screen. A measurement that needs an application lives with the application: the canvas
numbers stand on `GraphNode`, `GraphModel` and `NodeEditor`, and `blazy-canvas` has no
widgets of its own by design. Moving those into the crate would mean either a second
node implementation to keep in step with the example's, or a crate dev-depending on its
own example — and the first one also breaks the property the layout above exists for,
that the window and the benchmark build the same scene from the same code.

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

**A recorded region has to shrink as well as grow (§28).** The link set and the
far-field set are chosen for a region — the viewport plus a quarter of it on each side,
measured down from a half in §35.2 — and "has the viewport left the region?" is only
half the question: a region chosen at the overview zoom contains every viewport that
follows, so the set never shrank again and the canvas kept drawing all 9857 edges at a
zoom whose viewport held 96. `region_covers` asks for proportion as well as containment,
and the slack it allows is derived from that margin rather than fixed (§35.3). The
general lesson is in §28.4: counters that measure *work per frame* cannot see a defect
that lives in the *size of what is held*, and a sweep that enters a state without
leaving it tests half a switch.

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

**Level of detail obeys two rules, and the stricter wins (§29).** A zoom threshold
asks whether a control is still usable; `DetailBudget` asks whether the resulting tree
is affordable, in **widgets** — a panned frame costs 6.5-8.5 us per widget in the tree
whatever level produced them. The decision lives in `cull`, because only the cull knows
how many nodes are visible. Two things measurement disproved along the way: a larger
graph does *not* materialise more at a given zoom (the viewport bounds it) — density
and node size do, which is why the criterion sweeps density; and the budget has to be
divided between the canvases sharing a window (`DetailBudget::split`), because the
frame walks the window's tree and an idle area at an overview zoom charges its
neighbours for what it holds.

**Below a readability threshold the canvas stops building widgets and paints the nodes
itself (§20.6).** The far-field scene is recorded in canvas coordinates, so panning
and zooming inside the recorded region reuse it untouched. Level of detail is a
decision about *which widgets to build*, not about how to draw them (§20.7).

**Areas do not add up (§21).** `SplitTree` is pure geometry and knows nothing of
widgets; `AreaScreen` places one child per area at the rect the tree computed. Keep
that seam: the tree is what a `Workspace` serialises, and what fills each area lives
beside it rather than in it (§41.5). Rectangles are **rounded to whole pixels**, and
that is load-bearing rather than cosmetic — a fractional boundary makes every area
count as resized on every frame of a drag.

**An `AreaId` is the identity, and no operation renumbers one (§41.2).** Split, join,
swap, maximize/restore and a workspace load all obey one rule: an area that survives
keeps its widget, because a view, a selection and materialised nodes live nowhere else
(§30). That is what the tree's free lists and `AreaScreen`'s tombstoned `pods` are for,
and the `builds` counter is what holds it — zero for everything but a split. Two
consequences: **swap exchanges the leaves, not the widgets** (moving the id moves
everything keyed by it, at no cost at all), and **maximize is a flag plus `set_stashed`,
not a saved tree** — the tree underneath is untouched, so restoring returns the same
rectangles bit for bit rather than rebuilt ones.

**Join can only merge siblings, and §41.1 says what that costs.** Blender merges any two
areas with a coincident border; a binary tree can only replace a split with one of its
two children. Measured: 4 of 10 bordering pairs on eight areas, 8 of 24 on sixteen. It
is the price §8 named in advance, not a defect to patch — and an area whose sibling is a
split rather than a leaf has no partner at all. Do not "fix" it locally; replacing the
tree with a vertex-and-edge graph is its own work with its own numbers.

**Several canvases over one model is the normal case, and the model has to carry
every field of a node's state (§30).** Node geometry included: a canvas keeps its own
copy of the positions, so a drag that only moved the copy split two views of one graph
apart permanently. The seams are `NodeSource::attached` (the canvas hands the source its
own id), `NodeSource::moved` (the drag reaches the model, and the source names the other
views) and `CanvasLayer::update_child` (a model change reaches a node that is already on
screen); the fan-out goes through `mutate_later`, so the other areas show it in the same
frame. Two consequences for tests: a shared model has to be tested with **two** views —
with one, "the truth is in the model" and "the truth is in the view" are
indistinguishable — and a toggle has to be clicked **twice**, because Masonry's
`Checkbox` is a controlled component that never changes its own state.

**Picking asks the model, not the widget tree (§25.3).** A node below the far-field
threshold has no widget and a link never has one, so `find_widget_under_pointer` cannot
answer for either; `CanvasLayer::hit_test` answers from the geometry and the recorded
curve set instead. Two consequences that are easy to undo by accident: the pick
tolerance is in **screen pixels** and divided by the scale at test time (canvas units
span a factor of 400 across the zoom range), and a pick must not run layout — which is
why its counters are published by the pick itself rather than by the layout pass.

**A link pick is rejected by a stored box, and the box has to follow the node (§40.1).**
Candidates come from the *recorded* set, which is bounded by the region rather than by
the viewport, so it grows as the view pulls back: one pointer move used to rebuild
10 042 cubics at an overview zoom. The box each curve occupies is already computed when
the set is chosen (the short-link rule) and is now kept beside it, so a pick rejects
before it builds anything. Two things hold it up: `node_moved` re-measures the boxes of
the moved node's curves, because a drag moves a curve without re-choosing the set and
nothing else would notice; and the cost is counted **twice** — `hit_curve_tests` for
curves built, `hit_curve_scans` for boxes walked — because a filter that makes the
per-candidate work free would otherwise hide the number of candidates growing (§28.4
again).

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

**The device scale factor is the host's job (§4.2, §23.4), and the host is now real
(§26).** `VisualLayerPlan` comes out in logical coordinates; `blazy-shell`'s `Host`
applies the scale as one `Affine` at composition, which is why a display change costs a
frame and never a layout pass. Anything testing HiDPI goes through `Host`, not through
`TestHarness::render()`, which applies no scale factor at all.

**Runtime backend choice is a choice of rasteriser, not of the path to the screen
(§26.2).** `ImageRenderer` is object-safe and renders into a caller-owned buffer, so
`Box<dyn ImageRenderer>` picked at startup works. `TextureRenderer` has associated types
and cannot be a trait object — so the seam that *is* ours is `Presenter`, and it sits
after composition rather than around the rasteriser (§27.2).

**Two paths to the screen, and the direct one is not always faster (§27.4).**
`SwapchainPresenter` draws the scene into a texture and blits it into the swapchain;
`BlitPresenter` rasterises into a buffer and copies it into the window, and is the
fallback wherever there is no usable device. Measured: below about a megapixel the blit
path wins (submission overhead dominates), above it the swapchain path wins by 2-3x, and
at 3840x2400 the channel swap alone costs 17.4 ms — more than a 60 Hz frame. Both
examples default to the GPU path; `--backend vello_cpu` selects the other one in a live
window.

**vello renders through a compute shader**, so the intermediate texture needs
`STORAGE_BINDING`: without it the first frame fails wgpu validation rather than looking
wrong (§27.3).

**An external hole lives exactly one paint (§26.1).** `PaintCtx::set_paint_layer_mode`
is public upstream now — §7.3 and §17 are out of date on that — but the mode is reset
for every widget at the start of every paint pass, and a clean widget is not painted. A
widget that wants to stay a host hole has to keep painting; `ExternalContent` does that
through an animation frame, and a criterion counts frames in which the hole went
missing.

**The rasteriser has fixed buffers and overflows them in silence (§33, §34).** vello
sizes six bump-allocated buffers with constants that do not depend on the scene, and a
scene that needs more is dropped with `Ok` and no diagnostic — the window keeps the frame
before it. Two of the six are modelled in `blazy-shell`, and the arithmetic is checked
against vello itself rather than reasoned about: tiles are per path, blend scratch is per
*tile*, and what nests over a tile is not what nests in the scene — a group charges every
tile of its box, a rectangular clip only the tiles along its outline. Four levels of
nesting are free; the fifth costs a HiDPI frame. The guard answers from the command
stream and reaches the geometry only for a scene that could plausibly be over, which is
why it costs 0.000 ms on a real frame.

**A far-field frame is charged in path segments, and they are not equal (§32, §35).**
Rasterisation does not follow the number of draw commands at all — the batch of §31
made the plan eighty times cheaper and left the pixels alone. It follows segments: a
node is four of them (eight if its corners are rounded, which is why they are not below
half a pixel), a stroked link is two — and a stroked segment costs seven times a filled
one. So an overview frame *is* its links, and the levers that matter are the ones that
draw fewer curves, not simpler ones: a straight line costs exactly what a cubic costs.

**An idle area can keep its pixels (§36).** An area that declares
`PaintLayerMode::IsolatedScene` becomes its own layer in the plan, and `GpuFrames` keeps
a texture per layer: a layer whose `Scene` and transform are the ones from last frame is
copied rather than drawn. Eight areas over one graph cost a third of a frame that way.
Two things are load-bearing. The layer lives one paint, exactly like a hole, so the host
asks the layer owners to repaint on frames that are happening anyway
(`ShellDriver::layers`) rather than through an animation frame, which would stop the
window from ever idling. And a cached layer **owns its rectangle** — nothing else may
draw into it — which the host cannot check and therefore asks for. The cache is bounded:
the layers tile the window, so all of them together are about one frame of pixels, and the
default ceiling is two frames with eviction by least recent use (§37.2).

**There is no one place to intercept an event, and the layer is not a widget (§38).**
Masonry calls `Layer::capture_pointer_event` on every *layer root* before it even works
out the target — a genuine pre-tree hook, and it cannot stop the event: no return flag,
its `EventCtx` is discarded, `capture_pointer` is refused there. The only lever that
keeps an event from the tree is pointer capture, and Masonry offers it during a press
and nowhere else. So a modal operator started by a press is airtight and one started by
a key (`G`, `B`) leaks every event to the tree — counted, at 21 events and 20 needless
picks a gesture, not argued. `blazy-ops` therefore keeps its state in a plain
`OpRuntime<W>` and a driver feeds it, saying through `Seat` where the event came from;
the host seat in front of `RenderRoot` can withhold anything but must pay
`edit_widget` (a whole rewrite battery) to learn what is under the pointer.

The corollary shapes the keymap: because capture is only granted on the press, an
operator that may have to hold the pointer must start there — before anyone knows
whether the gesture will become a drag. So Blender's `CLICK` / `CLICK_DRAG` event
values cannot be keymap data here; what the gesture turned out to be is decided by the
operator holding it, steered by properties on the binding (§38.3).

**An operator never touches a widget (§38.3).** It changes the model and lists what
moved — or, for a view operator like `view.pan`, how far the view should move; the
driver carries that into its own canvas and into the graph's other views.
That is what makes `exec` worth having — the same operators run in a test with no tree,
so "the key and the script do the same thing" is checked by comparing model state. The
context an operator polls against is assembled *on hover*: the canvas picks on every
pointer event, including the press, and publishes the answer, because a driver holding
an `EventCtx` cannot hit-test a child.

**Undo is a journal, and the number decided it (§38.4).** A snapshot step costs the
graph — 1.92 MB and 0.29 ms on 20 000 nodes — where a journal step costs what was
touched: 40 bytes for a one-node drag. The graph is shared and the selection is not,
deliberately: a selection in the model would repaint every area showing that graph,
and one drawn inside its own area repaints one of eight (12.99 ms against 50.57).

**Masonry has no inherited properties (§22.1).** A `PropertyStack` hangs off the
widget itself and `Selector` matches classes and state flags, never ancestry. The
working mechanism is `WidgetMut::insert_prop` → `Widget::property_changed` → the widget
asks for layout, one widget at a time; a container that wants its children scaled must
forward the value itself. **Masonry's own widgets do not honour `UiScale`** — nothing
reads it, and their sizes come from the theme's `DefaultProperties`, which is one map
per application. The test `masonry_widgets_do_not_follow_ui_scale` pins that down and
will fail loudly if upstream ever grows a per-subtree scale.

## Upstream dependency

`masonry` is a **git dependency pinned to a commit** on purpose: the rendering IR `imaging`,
`Widget::paint(&mut Painter)` and `VisualLayerPlan` exist only on git main, and the
published 0.4.0 predates that migration. Living on a young crate's main branch is the
project's declared main risk (§15.1). The pin is in the workspace `Cargo.toml`; a local
checkout of the pinned tree is the fastest way to answer "does Masonry let us do X" and
is usually the right first step.

`masonry_winit` is **no longer a dependency**: `blazy-shell` runs its own winit loop
over the public `RenderRoot`, because upstream's runner owns a compile-time rasteriser
and keeps its event conversion private (§26.3). Input conversion is not reimplemented —
`ui-events-winit` is the same public crate upstream uses. `masonry_imaging` went the
same way in §27.3: it was there for one private helper, and twenty lines of our own
device request cost less than a second pinned crate. The GPU path sits on the published
`imaging_vello`, `imaging_wgpu` and `wgpu` instead, and `wgpu` is pinned to the version
`imaging_wgpu` selects, because the texture types have to come from one crate version.

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
  follows — kept current in place, not reconstructed from the "what this changes in §16"
  notes. §19 maps the result sections. Finishing a phase means adding a numbered section
  with the numbers, what was disproved, and what it changes in §16 — not just landing the
  code.
- **A measured number lives in one place.** The argument, the sweep and the figures go
  into `rnd/architecture.md`; a module doc says what the module does, what a caller has
  to promise, and which way its approximations err, then points at the section. Two
  copies of a number drift, and the copy in the code is the one nobody re-measures —
  §40.4 found the price of a draw command restated in the code as three different pairs
  of numbers.
- **The six published crates carry `#![warn(missing_docs, unreachable_pub)]`**, so a new
  public item needs a doc comment. That is also what catches a doc comment orphaned by a
  reordering: four of them had come adrift and were documenting the wrong function.
- **Counter and error types are `#[non_exhaustive]`; configuration types are not.** A
  counter grows every phase and nobody constructs one from outside; a config is
  constructed with `..Default::default()` and that is its interface (§40.4).
- **New public surface reaches an application through `crates/blazy`, features
  included.** The facade forwards `window` and `vello` in both directions — an
  application that wants no window system has to be able to say so (§14, §40.4).
- `issues/` holds tasks. When one is finished, append the outcome to the file **in
  place** — moving it into `issues/done/` is the user's call, not yours.
- Do not commit unless asked.
