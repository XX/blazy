# masonry: a layer root sees every pointer event before the tree and cannot keep one out of it

*A report meant to be filed upstream verbatim, which is why this file is in English while
the rest of `rnd/` is in Russian. Background and what blazy does about it:
`rnd/architecture.md` §38.*

Checked on **`b81d8d7`** (2026-08-30) and on **`ce7b04d`**, our pin: identical code in the
three places below, identical behaviour.

## Summary

`Layer::capture_pointer_event` is exactly the pre-tree hook a keymap wants — it is called
for every pointer event, before the target is worked out, including events outside the
layer's own rectangle. What it cannot do is stop one. The method returns nothing, the
`EventCtx` it is given is dropped along with its `is_handled` flag, and `capture_pointer`
is refused there because the pass builds that context with `allow_pointer_capture: false`.
The TODO above the method already names the first half of this:

```rust
// masonry_core/src/core/layer.rs:31
pub trait Layer: Widget {
    // TODO - Possible evolutions:
    // - Return flag to suppress event from reaching children.
    // - Return flag to remove layer.
    // - Pass layer id to method.
```

This report is the use case for that line, a reproducer, and a measurement of what the
workaround costs, so the flag can be weighed against something concrete rather than
against a guess.

The second half is the one that decides whether the flag is enough for us. The only lever
that keeps an event away from the tree today is pointer capture, and capture is granted
only during a press:

```rust
// masonry_core/src/passes/event.rs:248, inside run_on_pointer_event_pass
let handled = run_event_pass(
    root,
    target_widget_id,
    event,
    skip_if_disabled,
    matches!(event, PointerEvent::Down { .. }),   // allow_pointer_capture
    ...
```

Anywhere else, `EventCtx::capture_pointer` takes the `debug_panic!` branch at
`masonry_core/src/core/contexts.rs:458` — "event does not allow pointer capture" — and
returns without capturing. So a widget that wants to hold the pointer must decide **at the
press**, before anyone knows whether the gesture will turn out to be a click or a drag.

## Why this matters to a caller

We are building a Blender-style UI toolkit on Masonry, and the immediate casualty is the
keymap. Blender distinguishes `PRESS`, `CLICK` and `CLICK_DRAG` as *event values in the
keymap data*, which is what lets "click on empty space deselects" and "drag on empty space
pans the view" be two independent, user-rebindable rows on the same mouse button. To
express that, something has to hold a press for a few milliseconds and then decide — and
whatever holds it must be able to withhold those events from the widget tree, because
replaying them is not possible: the only entry point is `RenderRoot::handle_pointer_event`
and it cannot be called from inside a pass.

Today we cannot express it. A modal operator started by a **press** takes pointer capture
and is airtight; a modal operator started by a **key** (Blender's `G` to grab, `B` for box
select) has no press to capture on, so every event of the gesture reaches the tree first,
and the canvas below re-answers "what is under the pointer?" for each one. The keymap
therefore encodes click-versus-drag as ad-hoc properties on individual bindings, which
means the keymap knows the operators by name — precisely the coupling a data-driven keymap
exists to avoid.

## Reproducer

The whole program is next to this file as
`masonry-layer-cannot-withhold-pointer-event.rs`: drop it into a crate whose only
dependency is

```toml
masonry = { git = "https://github.com/linebender/xilem.git", rev = "b81d8d7", default-features = false, features = ["testing"] }
```

and run it with `cargo run`. The scene is one layer root (`Gate`) over two leaf widgets
side by side (`Probe`); each scenario sends a gesture of 23 pointer events — a hover, a
press, twenty moves, a release — and prints how many of them each seat saw.

```
masonry rev b81d8d7, one layer root over two leaf widgets, 23 pointer events per gesture
debug_assertions: true

1. layer sets handled on every event                 layer  23   left   2   right  21
2. left probe captures on Down                       layer  23   left  23   right   0
3. left probe captures on the first Move after Down  layer   3   left   3   right   0
   refused: capture_pointer - '#11': event does not allow pointer capture
4. layer calls capture_pointer in its seat           layer   1   left   0   right   0
   refused: capture_pointer - '#18': event does not allow pointer capture
5. keyboard-started gesture: moves only              layer  21   left  21   right   0
```

* **Row 1** — the seat is genuinely ahead of the tree, and `set_handled` there changes
  nothing: the layer saw all 23 events and so did the tree.
* **Row 2** — capture, taken on the press, is the one thing that does withhold events: the
  right probe sees none of the twenty moves that cross it.
* **Row 3** — the same widget asking one event later, which is the moment a gesture becomes
  a drag, is refused.
* **Row 4** — asking from the pre-tree seat is refused too, on every event.
* **Row 5** — a gesture that never had a press (a modal operator started by a key) has
  nothing to capture on, so all 21 of its events reach the tree.

Rows 3 and 4 hit `debug_panic!`; in a release build `capture_pointer` returns without
capturing and the program prints that instead. The reproducer catches the panic so all
five rows print in one run.

## What the gap costs, measured

Our numbers, on a 5000-node canvas in an 1100×750 window, six gestures per row, one
redraw per event. `tree-1st/g` counts the events that reached the widget tree before our
operator runtime while a modal operator was running; `picks/g` counts hit tests the canvas
performed:

| gesture | seat | events | tree-1st/g | picks/g | ms/g |
|---|---|---|---|---|---|
| drag a node (press-started, captures) | tree | 22 | **0.0** | 2.0 | 2.389 |
| grab (`G` key, no press to capture on) | tree | 22 | **21.0** | 22.0 | 2.649 |
| grab (`G` key), driven from outside `RenderRoot` | host | 22 | 0.0 | 21.0 | 2.182 |

A captured gesture costs two hit tests — the press and the first hover. The same gesture
without capture costs twenty-two, because every leaked move reaches the canvas and it
answers "what is under the pointer?" again, for an answer the operator layer already has.

The third row is our current workaround: we run our own winit loop over the public
`RenderRoot`, so a driver sitting in front of `handle_pointer_event` can withhold anything.
It works and it is not slower — but it has to ask `RenderRoot::edit_widget` what is under
the pointer, which runs the full rewrite-pass battery, at 41.2 probes per gesture. That is
a lever only an embedder who owns the event loop has; an application built on
`masonry_winit` has none.

## What would help, in order of how much

1. **The flag the TODO already names**: let `capture_pointer_event` return something that
   keeps the event from the tree. This is the smallest change and it closes our case
   completely: with it, buffering a press to see whether it becomes a drag needs no
   capture at all, and no event is taken from the tree except deliberately.
2. **Allow `capture_pointer` on a bubbled `Move`.** A different shape with the same
   effect, and it may be the more natural one for widgets rather than layers — a widget
   that discovers mid-gesture that it wants the pointer can simply say so. This is also
   what #1581 asks for from `Split`'s side, independently of us.
3. **Opaque pointer areas** (#1581, #1347): a widget declaring a region where events do
   not descend. More work than the first two, and it solves a superset — but it is a
   property of a *region*, so it does not by itself let a layer withhold an event on a
   condition that is not geometric, which is what a keymap needs.
4. **An ancestor-first phase**, the way #562 dispatched touch events root-down "to allow
   portals and other containers to process gestures". That PR is long stale and touch-only,
   but the shape it reached for is the general one: a container that may want a gesture
   gets the event before its descendants and decides. It is the largest change of the four
   and the only one that would help a widget rather than only a layer root.

Any one of the four would do for us; we are not asking for a particular design, and we
would rather the shape be chosen by whoever maintains the pass than have the narrowest
formulation answered.

## Questions a flag would have to answer, from our side

Naming these because they are the parts we cannot decide from outside, not because we
expect them all resolved at once:

* **Which events may be withheld.** Withholding a `Down` while the tree believes a widget
  is hovered leaves that widget's hover and active state stale, and the same is true of an
  `Up` that would have ended a capture. If the flag skips only the event pass and still
  updates hover/capture bookkeeping — or sends a `Cancel` to a widget that was mid-gesture
  — the caller does not have to reason about it. If not, that needs saying in the doc.
* **Ordering between several layer roots.** The pass walks the root's children in order; a
  first layer that suppresses presumably means later layers do not see the event either,
  but a tooltip layer arguably wants to see it in order to dismiss itself. "Suppress from
  children" and "suppress from other layers" may want to be different answers.
* **Whether the flag makes capture available.** If a layer can withhold events, it may not
  need capture at all — which would be the simplest outcome, and is what we would build on.

## Prior art in this repository

* **The TODO** at `masonry_core/src/core/layer.rs:31`, unchanged since the method was
  added.
* **#1581, "`Split` bar area doesn't get exclusive pointer events"** (open) is the same
  wall from the other side: `Split`'s enlarged bar area cannot take an event away from the
  child under it, and cannot capture on `Move` to claim the cursor icon either — "There is
  no way for `Split` to capture the pointer based on location in `Move` either as that can
  be done only in `Down`". Its two proposed fixes are options 2 and 3 above.
* **#1347, "Improve clip path design"** notes that widgets may want a shape *above* their
  children that is checked before them for pointer events — the geometric half of the same
  need.
* **#562, "Pan/Flick gestures for Portal"** (open, draft, last touched 2024-08-30) hit the
  same wall from a third side: to recognise a pan or a flick, `Portal` needed the event
  before its children, and the PR's answer was a second dispatch that descends from the
  root — "Descend from the root when dispatching touches to allow portals and other
  containers to process gestures". It predates the current pass layout and only covers
  touch, so it is prior art rather than a fix in flight.

We checked the 49 open pull requests on 2026-08-30: none of them changes
`allow_pointer_capture`, and none adds a way for a layer or a widget to withhold a pointer
event. So this is not a request for something already in review.

We are filing this as a second, non-geometric use case for the same mechanism rather than
as a new problem: a keymap's condition for withholding an event is "is there a binding on
this button in this context", which no shape can express.

## We have implemented option 1, and here is what it cost the caller

Since filing, we built the flag ourselves to find out whether it is worth asking for. The
branch is one commit on top of `b81d8d7` — 8 files, +313/−53, of which about 180 lines are
tests — and it is offered here in whatever shape suits you; we are not attached to ours.

`capture_pointer_event` returns `Handled`; `Handled::Yes` skips the tree dispatch. Two
rules had to be chosen, and both are in the method's doc: every layer is still called
(suppression hides the event from the tree, not from the other layers), and pointer
capture outranks a layer, so a widget mid-gesture is always told how its gesture ends.
`ModularWidget` grew a `capture_pointer_event_fn` so the mechanism is testable without a
bespoke widget, and the three harness mouse helpers return `Handled` so a test can assert
what the driver was told.

**One thing we found while doing it, which is a bug on its own and worth fixing whatever
you decide about the flag.** The layer loop in `run_on_pointer_event_pass` does not call
`merge_state_up` after invoking the hook, while `run_event_pass` does it for every widget
it visits. So anything a layer root asks for from that hook — a layout, a paint, an
animation frame, on itself or on a child — never reaches the root, and the frame it asked
for does not happen. Nothing upstream notices today because `Tooltip` and `SelectorMenu`
only call `remove_layer`, which goes through global state. It cost us a real defect the
moment our layer did real work: a drag updated the model and the canvas was never laid out
again — 1 layout pass per gesture where there should have been 20. One line, and a
regression test that fails without it.

And the caller-side numbers the first half of this report was missing, on our own
application (5000 nodes, 1100×750, six gestures per row, one redraw per event):

| gesture | events | reached the tree first | hit tests | ms |
|---|---|---|---|---|
| grab from a key, before | 22 | 21.0 | 22.0 | 2.181 |
| grab from a key, with the flag | 22 | **0.0** | **1.0** | 2.189 |
| the same drag started by a press (capture covers it) | 22 | 0.0 | 2.0 | 2.218 |

The time is unchanged — a hit test costs tens of microseconds — and that is rather the
point: what the flag buys is not speed but that a gesture started by a key costs the same
as one started by a button, so a toolkit can offer both without one of them being a
second-class citizen.

## What we do meanwhile

Nothing here is blocked on an answer — we mention it only so the trade-off is visible.
Our operator runtime keeps its state outside the widget tree and is fed by a driver, and
the driver can sit in a widget, in a layer's `capture_pointer_event`, or in the host in
front of `RenderRoot::handle_pointer_event`; the runtime is told which seat an event came
through and counts the ones the tree saw first. The host seat can withhold everything and
is what we will build click-versus-drag on, at the cost above. We are not patching or
forking Masonry.
