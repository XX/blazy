# AGENTS.md

Guidance for agents working in this repository. **Keep this file under 16 KB** — the rule
loader cuts it there (§70). The arguments and numbers live in `rnd/architecture.md` (the
document of record); here a rule is a line and a section number.

## What this is

`blazy` is a **GUI library**: a Blender-style UI layer on Masonry — areas and splits, a node
canvas with ordinary widgets in the nodes, operators and a keymap. Each phase asks one
architectural question, answers it with numbers, and records it as a numbered section of
`rnd/architecture.md`; the checks run in CI so the answers keep holding. Examples are the
front line, not demos. It is a library: the public API and the git-pinned upstream are risks
downstream inherits (§15.1, §18).

## Commands

Everything through `cargo-make`; `cargo fmt` needs nightly, the rest is stable.

```bash
cargo make ci               # lint + feature matrix + docs + tests + three benchmark gates
cargo make lint             # fmt --check + clippy -D warnings
cargo make doc              # rustdoc -D warnings
cargo make fmt              # nightly rustfmt
cargo make test             # cargo test --workspace
cargo make run-hello        # smallest application, facade only
cargo make run-calc         # second application: arithmetic graph saved to a file
cargo make run-node-canvas  # Phase 0 window
cargo make run-area-screen  # Phase 0.5/0.6 window
cargo make bench            # bench-canvas, bench-areas, bench-shell; bench-report for JSON
cargo test -p blazy-areas ui_scale   # a single test, by substring
```

**No `--` separator** for cargo-make arguments: `cargo make bench-canvas --quick`.
`--quick` runs only the scenarios criteria use.

## Crates

| Crate | Role |
|---|---|
| `blazy` | Facade — what an application depends on; re-exports the rest and `masonry`. |
| `blazy-canvas` | Virtualised zoomable canvas: nodes, links, spatial index, picking. |
| `blazy-areas` | Split tree, areas, regions (header, main, sidebar), `ui_scale`, workspace file. |
| `blazy-ops` | Operators, keymap as data, modal stack, undo journal. |
| `blazy-node-editor` | Node editor: operators, driver widget, `Views`, panels, `ContentField`. |
| `blazy-shape` | Shape-accurate hit testing. |
| `blazy-widgets` | Carries `ui_scale` into stock widgets (`Bar`, `Label`). |
| `blazy-shell` | Host: winit loop, composition, rasteriser choice, clipboard, IME. |
| `blazy-app` | Assembly: editors in areas and windows; the `ShellDriver` (`EditorApp`). |
| `bench-utils` | Criteria, verdict, JSON report. |
| `examples/hello`, `examples/calc` | Applications through the facade only. |
| `examples/node-canvas`, `examples/area-screen` | Experiments: lib + thin bin + bench. |

A benchmark lives where what it needs lives: mechanism-only in the crate, application-level
with the application (§26).

## Measurement discipline

- **Criteria gate on deterministic counters, never milliseconds**; times are printed only.
  A benchmark exits non-zero on a failed criterion; an empty criteria list is a failure.
- A criterion is `measured < bound`; a positive claim counts from the failing side.
- **Break every new check once** and see it fire. A check that never failed proves nothing.
- **A counter of work cannot see a defect in the result or in the size of what is held**
  (§28, §45): one criterion compares frames by 16x16 block means; held sets are counted.
- **A miss that can be normal is a number** (counter, `debug_assert`, warning) (§44.9).
- Sweep endpoints matter at both ends (§24.1, §25.4).
- Debug builds inflate dirty-flag counters; count observed size changes instead.
- **Masonry counts no rewrite passes**: the harness's "Loop detected" is the only gauge; an
  improvement no test can see is not taken (§65).

## Invariants (load-bearing; see the section before changing)

- Frame cost is walking the widget tree; `set_stashed` does not cull — **virtualise** (§20.2).
- Links are curves, chosen by adjacency; repaint ≠ reselect (§24). Visible set from a grid
  whose slack follows the widest node (§24.2). A recorded region must shrink too (§28).
- Canvas is two widgets (`CanvasLayer` clip/view, `CanvasContent` children) (§20.3).
- LOD: zoom threshold and widget budget, stricter wins; budget split per canvas (§29).
- Far field: nodes painted, not built (§20.6); far links must be visible to be drawn (§53).
- Rects are rounded to whole pixels (§21). `AreaId` never renumbers; only split builds;
  swap moves leaves, maximize is a flag (§41.2). Join merges siblings only (§41.1).
- **The model is the truth; several views per graph is the normal case** (§30). Test shared
  state with two views; Masonry `Checkbox` is controlled (click twice). Node sources in such
  tests must register with `Views` (`NodeSource::attached`), or there are no peers (§65).
- Picking asks the model (`CanvasLayer::hit_test`), tolerance in screen px (§25.3); link
  boxes follow nodes (§40.1). Ports are the canvas's rule, `PortLayout` (§52).
- `ui_scale` → layout, `view` → paint only (§9, §22). Masonry has no inherited properties:
  containers carry scale (`ScaleCarrier`, `Bar`, `Bar::column`); a pass per level (§46, §48).
- Device scale is the host's (§23.4); rasteriser choice ≠ path to screen (§27); vello
  overflows buffers silently (§33). An idle area keeps its pixels as its own layer (§36).
- **An operator never touches a widget** (§38.3); a modal operator owns the window's pointer
  (§54); `modal(None)` is a cut-off and must clean up (§58).
- Key events: `OpEvent::from_keyboard`, never `key.key` (§49). Keymap is a file (§49).
- Text fields need the host; `Ctrl+V` outside a field is the editor's (§56).
- Node names are holes in an array, never shifted; `NodeGraph::names()` counts them (§43, §69).
- `NodeGraph` (§42–§69): required — geometry, structure, `Content`, `set_content`,
  `insert_copy`, `links_of`, `views`; defaulted — `set_node_pos`, `names`, `width_limits`,
  `node_kinds`/`insert_kind`, `accepts_link`, `input_limit` (1), `text_form`.
- **One history per graph**, shared by every session over it (§70); undo is a journal (§38.4).
- Selection, stacking, clipboard copy are the graph's, in `Views` (§50, §57).
- What a view is owed is the library's to record (§47); a move replaces the owed move of the
  same node, never across a structure edit (§70). Watchers (panels) are owed only what they
  show (§62, §70); the editor refreshes them in its window, `sync_root` in others.
- An inserted node in view is built in the mutate pass that inserts it (§65).
- Link drag: take off, refuse by `accepts_link`, one input link by default (§59).
- Resize by left/right edge, width only (§60). Sidebar is the second axis; its state is the
  session's (§61).
- Edits made in widgets: `ContentField` — model on each key, one undo step when the field is
  left, unreadable text restored (§67, §70).
- Typed add: `node_kinds` + `insert_kind`, `node.add kind="…"` (text property) (§64).
- Node copy to the system clipboard: `text_form()` → `blazy-nodes 1 <format>`, driver writes
  after the event and reads before a `node.paste` key (§66).
- Graph reload: `EditorApp::on_load` — cancel gestures first, keep the `Views` registry, then
  `graph_replaced` → `Change::Reset` (§69). Save: `on_save`; workspace read at first window.
- `EditorSession`/`EditorWorld` have no public fields; use methods (§68). `Detail` stays
  exhaustive on purpose.

## Upstream

`masonry` is a git dependency pinned to a commit of our fork (`XX/xilem`, branch `dev`):
upstream plus our own commits (§17). A change Masonry needs is the user's to make in the
fork — **say so and stop**, do not work around or patch it here. `blazy-shell` runs its own
winit loop over `RenderRoot` (§26.3). A local checkout of the pinned tree answers "does
Masonry let us do X" fastest.

## Conventions

- Prose docs in Russian; code, rustdoc and comments in English.
- Comments explain *why*, especially a rejected alternative or a contradicting measurement;
  keep those when refactoring.
- Finishing a task adds a numbered section to `rnd/architecture.md` (numbers, what was
  disproved, what it changes in §16) and a row in §19. A measured number lives in one place.
- Published crates carry `#![warn(missing_docs, unreachable_pub)]`.
- Results the library produces and counters are `#[non_exhaustive]`; configs built with
  `..Default::default()` are not (§40.4, §63.3).
- New public surface reaches applications through `crates/blazy`, features included;
  examples depend on `blazy` alone.
- `issues/`: append the outcome in place; moving to `issues/done/` is the user's call.
- Back up files in scripts by path, not `basename`: `hello` and `calc` are both `main.rs`.
- Do not commit unless asked.
