# vello 0.10: a frame is silently dropped when the tile buffer overflows

*A report meant to be filed upstream verbatim, which is why this one file is in English
while the rest of `rnd/` is in Russian. Background and what blazy does about it:
`rnd/architecture.md` §33.*

Checked on **0.10.0** and **0.9.0**: identical code in the three places below, and
identical behaviour down to the exact path count at which the frame disappears.

## Summary

`Renderer::render_to_texture` returns `Ok(())` and draws **nothing at all** when a scene
needs more tiles than the fixed `BufferSizes::tiles` allocation. The target keeps
whatever it held before — for a UI that renders into an intermediate texture, that is the
previous frame, so the window silently freezes rather than showing anything wrong. No
error, no log, no way for the caller to find out short of reading the frame back.

The boundary is exact and reproducible: at 1100x750, **646 screen-spanning paths draw and
647 draw nothing**; at 2200x1500, **161 draw and 162 do not**. Both land within 0.15% of
`1 << 21` tiles, which is what `vello_encoding::BufferSizes::new` allocates:

```rust
// vello_encoding-0.10.0/src/config.rs, BufferSizes::new
// The following buffer sizes have been hand picked to accommodate the vello test scenes
// as well as paris-30k. These should instead get derived from the scene layout using
// reasonable heuristics.
let tiles = BufferSize::new(1 << 21);
```

Nothing about that allocation depends on the scene or on the size of the target, so the
number of paths a frame may contain falls as the window grows: a 4K window can hold a
sixth as many as an 800x600 one.

tiles(path) is the path's bounding box clipped to the target, in 16x16 tiles — checked:
making every path three times the size of the frame does not move the boundary at all.

## Why it is silent

`Renderer::render_to_texture_async_internal` reads the bump allocators back only when
compiled with the `debug_layers` feature:

```rust
// vello-0.10.0/src/lib.rs:730
let robust = cfg!(feature = "debug_layers");
```

Without it, the `failed` flag the coarse stage sets is never looked at, and the sync
`render_to_texture` has no way to report it either. `Scene::bump_estimate` cannot warn a
caller in advance because `BumpEstimator` does not model this buffer —
`vello_encoding-0.10.0/src/estimate.rs` opens with `// TODO: support tile` and `tally`
returns `tile: 0`. So the one public, device-free API that looks like it should predict
this is silent about exactly the buffer that overflows.

## Reproducer

The whole program is next to this file as `vello-silent-tile-overflow.rs`: drop it into a
crate whose only dependency is `vello = "0.10"` and run it. It bisects the boundary
itself and prints it next to the prediction. wgpu is used through vello's own re-export,
so there is no second version to line up. Each path is one diagonal across the whole
target, so its bounding box is the target and the tiles it needs are known exactly.

```rust
for paths in [640_u32, 660] {
    let mut scene = Scene::new();
    for i in 0..paths {
        let y = f64::from(i % 3);
        let line = Line::new(Point::new(0.0, y), Point::new(1100.0, 750.0 - y));
        scene.stroke(&Stroke::new(1.0), Affine::IDENTITY, Color::from_rgb8(0x80, 0xa0, 0xf0), None, &line);
    }
    // A fresh texture each time, so an undrawn frame is visibly empty rather than the
    // frame before it.
    let texture = /* 1100x750, Rgba8Unorm, STORAGE_BINDING | COPY_SRC */;
    renderer.render_to_texture(&device, &queue, &scene, &view, &RenderParams {
        base_color: Color::from_rgb8(0x14, 0x14, 0x18),
        width: 1100, height: 750, antialiasing_method: AaConfig::Area,
    }).unwrap();                       // Ok in both cases
    // read the texture back: 640 paths -> every pixel painted; 660 paths -> all zeroes.
}
```

Measured on Intel UHD Graphics (CML GT2), Mesa 25.2.8, Vulkan backend. The same numbers
on vello 0.10.0 (wgpu 29.0.4) and on vello 0.9.0 — the boundary does not move by a single
path between the two.

```
1100x750:  3243 tiles per path, budget 2097152 => 646 paths draw (2094978 tiles),
                                                  647 paths draw nothing (2098221)
2200x1500: 12972 tiles per path                => 161 paths draw (2088492 tiles),
                                                  162 paths draw nothing (2101464)
```

## The same failure, closer to home: nested blend layers

The tile buffer needs hundreds of large paths, which is a lot. `blend_spill` (`1 << 20`
words) needs almost nothing: a tile deeper than `BLEND_STACK_SPLIT` (4) spills
`(depth - 4) * 256` words, so a full-window stack of layers overflows at

| Window | tiles | depth 4 | depth 5 | depth 6 |
|---|---:|---|---|---|
| 1100x750 | 3243 | draws | draws (830 208 words) | **empty** (1 660 416) |
| 2200x1500 | 12972 | draws | **empty** (3 320 832) | **empty** (6 641 664) |

The layers in that test are visual no-ops — `Mix::Normal`, alpha 1.0 — so this is about
nesting alone, not about what is inside. **Five nested layers on a HiDPI window** is not
an exotic scene for a UI: an opacity animation inside a popover inside a modal gets there
without trying, and the failure is the same silent empty frame.

Plain clip layers (`push_clip_layer`) are unaffected — checked to depth 8 at both sizes,
which is worth knowing because clipping is what a toolkit does constantly.

A third buffer, `ptcl`, is handled differently again: `alloc_cmd` in `coarse.wgsl` points
every further allocation at offset 0 with the comment "This sets us up for technical UB,
as lots of threads will be writing to the same locations". That path would produce a
*wrong* frame rather than a missing one. We could not reach it — the tile buffer always
ran out first in every scene we tried — so it is reported here as something a reader of
this issue may want to check, not as an observed symptom.

## Prior art in this repository

This is not a new discovery, and the report is written knowing that — what seems to be
missing is the user-visible half of it: the symptom, a boundary, and a reproducer.

* **#366, "Strategy for robust dynamic memory, readback, and async"** (open since
  2023-09-28) is the umbrella issue, and its first listed strategy is precisely today's
  behaviour: pre-determined sizing that fails silently on overflow. The trade-offs are
  well argued there; what the issue does not record is what the chosen strategy looks
  like from the outside, which is a window that stops updating with no diagnostic.
* **#606, "Robust memory allocation handling"** (DJMcNab, open, not a draft, created
  2024-06-07, last touched 2026-03-14) implements the fix: cancel the pipeline, read the
  bump buffer back from two frames ago, reallocate. If it lands, everything below is
  moot. It has been waiting on review for about two years, which is why the smallest ask
  below is worth having in the meantime.
* **#788** (open) is the same fixed-size approach failing in a different buffer:
  `bin_data` underflows with very many images, wrapping in release and reaching
  out-of-bounds memory. Different symptom, same root.

Checked on `main` on 2026-08-25: `BufferSizes::new` is unchanged, `let robust =
cfg!(feature = "debug_layers");` is still there, and the sync `render_to_texture` returns
`Result<()>`. Note also that `render_to_texture_async` — the one entry point that can
return `BumpAllocators` at all — is now deprecated, so the only path by which a caller
could learn of an overflow is on its way out.

## What would help, in order of how much

1. **Report it**, without waiting for #606. Anything a caller can branch on: an error
   from `render_to_texture`, a flag, a `tracing` warning behind the existing feature — a
   frozen window with no diagnostic is the worst possible failure mode, and it is one bit
   away from being a recoverable one. A caller that knows can fall back to `vello_cpu`,
   split the scene, or tell the user; a caller that does not know can only ship a frozen
   window.
2. **Estimate it.** `BumpEstimator` already exists and is public; teaching it the tile
   count would let a caller size its own scene before submitting. That is what we ended
   up doing from the outside — summing the tiles of each path's bounding box clipped to
   the target — and it predicts the boundary exactly, but it does so against a constant
   that is not part of the public API and can change under us in any release.
3. **Grow it.** Sizing the buffer from the scene layout, as the comment in
   `BufferSizes::new` already proposes and as #606 does properly.

Any one of the three stops a UI from freezing without saying why; the first is the
smallest, and it is useful even after #606 lands.

A note on what a caller can do today: the tile demand can be re-derived from the outside
(sum of each path's bounding box in tiles, clipped to the target) and predicts the
boundary exactly — we do that and refuse the frame rather than showing a stale one. The
blend spill can be predicted the same way. But both are arithmetic against constants that
are not public API, so every caller doing this is one release away from either freezing
again or refusing frames that would have rendered.
