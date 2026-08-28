//! What vello will try to allocate for a frame, worked out before the frame is sent.
//!
//! vello sizes its bump-allocated buffers with constants that do not depend on the
//! scene, and a scene that needs more is dropped in silence: `render_to_texture`
//! returns `Ok`, `fine.wgsl` sees the failure flag of an earlier stage and returns
//! without drawing, and the target keeps the frame before it. Two of the six buffers
//! are modelled here so a host can refuse such a frame out loud instead. Why it is
//! silent, where it is filed upstream and the boundaries this arithmetic was checked
//! against: `rnd/architecture.md` §33 and §34.
//!
//! # The two demands
//!
//! ```text
//! tiles  = sum over paths of  tiles(bbox of the path, clipped to the frame)
//! blend  = sum over tiles of  max(0, deepest nesting over this tile - 4) * 256 words
//! ```
//!
//! The first is per path, the second per **tile**, and the difference is not a detail:
//! what nests over a tile is not what nests in the scene. `coarse.wgsl` includes a
//! layer in a tile when `n_segs != 0 || (backdrop_clear == is_clip) || is_blend`, so
//!
//! * a **group** is a blend layer and charges every tile of its box;
//! * a **clip** charges only the tiles its *outline* crosses. Tiles inside keep the depth they had; tiles outside take
//!   the `clip_zero` branch and charge nothing.
//!
//! That is why an area tree and a canvas viewport, which clip with rectangles, do not
//! go near this ceiling, while an application nesting a few opacity groups does (§34.2).
//!
//! # What this counts, and which way it errs
//!
//! Fills, strokes and clip shapes are counted from their bounding boxes, which is what
//! vello bins on; a glyph is charged its em box; image draws are not counted. A
//! rectangular clip is charged the far column and far row of its box — the count the
//! measurements pin down, placed where the model puts it (§34.3). Every approximation
//! is deliberately **generous**: charging too much refuses a frame that would have
//! drawn, which is visible and reportable, while charging too little brings back the
//! silent one. The two places that shows: a clip that is not a rectangle charges its
//! whole box, and a group with no clip of its own charges the whole frame, because
//! that is what `imaging_vello` pushes it against (`scene_sink.rs`, `surface_clip`).
//!
//! The boundaries themselves live in the tests below, which is where they are checked
//! rather than restated.

mod blend;

use blend::{Blend, Charge, TileBox};
use masonry::dpi::PhysicalSize;
use masonry::imaging::record::{Command, Glyph, Scene, replay};
use masonry::imaging::{BlurredRoundedRect, ClipRef, FillRef, GlyphRunRef, GroupRef, PaintSink, StrokeRef};
use masonry::kurbo::{Affine, Rect};

/// Side of a vello tile, in device pixels.
const TILE_PX: u32 = 16;

/// The same as a float, for the geometry.
const TILE: f64 = TILE_PX as f64;

/// Tiles vello can allocate for one frame, whatever that frame contains.
///
/// `vello_encoding::BufferSizes::new`, `tiles = BufferSize::new(1 << 21)` — the same in
/// 0.9 and in 0.10, checked. Pinned along with the rasteriser: `imaging_vello` 0.0.2
/// selects vello 0.9 (§27.1), and a version that sizes this buffer from the scene would
/// make the whole check moot.
pub const TILE_BUDGET: u64 = 1 << 21;

/// Words of blend scratch vello can allocate for one frame.
///
/// `blend_spill = BufferSize::new(1 << 20)`, with the comment "16 * 16 (1 << 8) is one
/// blend spill, so this allows for 4096 spills" — the same in 0.9 and in 0.10, checked
/// the same way and pinned for the same reason as [`TILE_BUDGET`].
pub const BLEND_BUDGET: u64 = 1 << 20;

/// Levels a tile nests before it starts spilling to `blend_spill`.
///
/// `shader/shared/config.wgsl`, `const BLEND_STACK_SPLIT = 4u`: the first four levels
/// live in registers.
const BLEND_STACK_SPLIT: u32 = 4;

/// Words one tile spills per level beyond the split: one per pixel of the tile.
const SPILL_PER_TILE: u64 = (TILE_PX as u64) * (TILE_PX as u64);

/// What one composed scene asks the rasteriser for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Demand {
    /// Tiles across every path — compare against [`TILE_BUDGET`].
    pub tiles: u64,
    /// Words of blend scratch across every tile — compare against [`BLEND_BUDGET`].
    pub blend_words: u64,
}

/// A buffer a scene does not fit in, and by how much.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overflow {
    /// More tiles than [`TILE_BUDGET`]: too many large paths (§33.1).
    Tiles {
        /// Tiles the scene asks for.
        tiles: u64,
        /// Tiles the rasteriser can allocate.
        budget: u64,
    },
    /// More blend scratch than [`BLEND_BUDGET`]: layers nested too deep over one tile
    /// (§34).
    Blend {
        /// Words the scene asks for.
        words: u64,
        /// Words the rasteriser can allocate.
        budget: u64,
    },
}

/// Whether a scene is over one of the budgets — the question the frame path asks,
/// answered without walking the scene when the answer is obvious.
///
/// Two cheap bounds come first, because this runs in front of every frame:
///
/// * a scene cannot need more tiles than its command count times the frame's own tiles, because no single path can be
///   charged for more than the whole frame;
/// * a tile cannot nest deeper than the scene nests, and nesting is visible in the command stream alone — no geometry,
///   no bounding boxes.
///
/// Both are far under for anything a user interface actually draws, so the exact walk —
/// proportional to the geometry, and the expensive half of this — runs only for a scene
/// that could plausibly be over (§33.3).
pub fn over_budget(scene: &Scene, frame: PhysicalSize<u32>) -> Option<Overflow> {
    if !needs_walk(scene, frame) {
        return None;
    }

    let demand = demand(scene, frame);
    if demand.tiles > TILE_BUDGET {
        return Some(Overflow::Tiles {
            tiles: demand.tiles,
            budget: TILE_BUDGET,
        });
    }
    if demand.blend_words > BLEND_BUDGET {
        return Some(Overflow::Blend {
            words: demand.blend_words,
            budget: BLEND_BUDGET,
        });
    }
    None
}

/// Whether the cheap bounds leave the question open, so the exact walk has to run.
///
/// The property the guard's cost rests on, published so it can be *counted* rather than
/// timed: a benchmark that asserts "the guard is cheap on a real frame" in milliseconds
/// is asserting something about the machine, while this is the same claim as a
/// deterministic counter (§20.9).
pub fn needs_walk(scene: &Scene, frame: PhysicalSize<u32>) -> bool {
    let ceiling = (scene.commands().len() as u64).saturating_mul(frame_tiles(frame));
    ceiling > TILE_BUDGET || nesting_depth(scene) > BLEND_STACK_SPLIT
}

/// What a composed scene will ask vello for, at this frame size.
///
/// The exact walk. [`over_budget`] is what a frame path should call; this is for a test
/// or a measurement that wants the numbers themselves.
pub fn demand(scene: &Scene, frame: PhysicalSize<u32>) -> Demand {
    // The blend map is only allocated for a scene that could reach the spill at all:
    // four levels live in registers, so a shallower scene asks for zero words and the
    // per-tile bookkeeping would measure nothing.
    let deep = nesting_depth(scene) > BLEND_STACK_SPLIT;
    let mut counter = Counter::new(frame, deep);
    replay(scene, &mut counter);
    counter.finish()
}

/// Tiles a composed scene will ask vello for, at this frame size.
pub fn tile_demand(scene: &Scene, frame: PhysicalSize<u32>) -> u64 {
    demand(scene, frame).tiles
}

/// Words of blend scratch a composed scene will ask vello for, at this frame size.
pub fn blend_demand(scene: &Scene, frame: PhysicalSize<u32>) -> u64 {
    demand(scene, frame).blend_words
}

/// The deepest the command stream nests, clips and groups alike.
///
/// An upper bound for the depth of any one tile, read from the command stream without
/// touching a single bounding box. That is what makes it usable as the cheap half of
/// [`over_budget`]: no tile can nest deeper than the scene does.
pub fn nesting_depth(scene: &Scene) -> u32 {
    let mut depth = 0_u32;
    let mut deepest = 0_u32;
    for command in scene.commands() {
        match command {
            Command::PushClip(_) | Command::PushGroup(_) => {
                depth += 1;
                deepest = deepest.max(depth);
            },
            Command::PopClip | Command::PopGroup => depth = depth.saturating_sub(1),
            _ => {},
        }
    }
    deepest
}

/// Tiles of the frame itself.
fn frame_tiles(frame: PhysicalSize<u32>) -> u64 {
    u64::from(f64::from(frame.width).div_euclid(TILE) as u32 + 1)
        * u64::from(f64::from(frame.height).div_euclid(TILE) as u32 + 1)
}

/// Adds up the tiles, one path at a time, and the blend scratch, one tile at a time.
struct Counter {
    /// The frame, in physical pixels: vello bins only over the target (§33.1).
    frame: Rect,
    tiles: u64,
    /// `None` for a scene that cannot reach the blend spill (§34.3).
    blend: Option<Blend>,
}

impl Counter {
    fn new(frame: PhysicalSize<u32>, deep: bool) -> Self {
        let frame = Rect::new(0.0, 0.0, f64::from(frame.width), f64::from(frame.height));
        Self {
            frame,
            tiles: 0,
            blend: deep.then(|| Blend::new(&frame)),
        }
    }

    fn finish(self) -> Demand {
        Demand {
            tiles: self.tiles,
            blend_words: self.blend.map_or(0, Blend::words),
        }
    }

    /// Charges one path, given its bounding box in scene coordinates.
    fn charge(&mut self, transform: Affine, bounds: Rect) {
        self.tiles += charge_of(self.frame, transform, bounds);
    }
}

/// The tiles one path costs: its box, clipped to the frame, over the tile grid.
fn charge_of(frame: Rect, transform: Affine, bounds: Rect) -> u64 {
    let bounds = transform.transform_rect_bbox(bounds).intersect(frame);
    if bounds.is_zero_area() {
        return 0;
    }
    // Tiles are a fixed grid, so a box that straddles a tile boundary pays for both —
    // which is why this is not `width / TILE` rounded up.
    let across = (bounds.x1 / TILE).ceil() - (bounds.x0 / TILE).floor();
    let down = (bounds.y1 / TILE).ceil() - (bounds.y0 / TILE).floor();
    (across.max(1.0) * down.max(1.0)) as u64
}

/// The tiles of a shape, for the blend map: its box, clipped to the frame.
fn tiles_of(frame: Rect, transform: Affine, bounds: Rect) -> TileBox {
    TileBox::of(transform.transform_rect_bbox(bounds).intersect(frame))
}

impl PaintSink for Counter {
    fn push_clip(&mut self, clip: ClipRef<'_>) {
        // A clip shape is a path like any other, and pays for its tiles like one.
        let (transform, bounds, rectangular) = crate::bounds::clip(&clip);
        self.charge(transform, bounds);

        let frame = self.frame;
        if let Some(blend) = &mut self.blend {
            let inside = tiles_of(frame, transform, bounds).intersect(blend.visible);
            // The clip is included only where its outline cuts a tile; inside it the
            // depth is unchanged, outside it nothing is included at all (§34.2).
            let charged = if rectangular {
                Charge::Edges(inside)
            } else {
                Charge::Whole(inside)
            };
            blend.enter(charged, inside);
        }
    }

    fn pop_clip(&mut self) {
        if let Some(blend) = &mut self.blend {
            blend.leave();
        }
    }

    fn push_group(&mut self, group: GroupRef<'_>) {
        // A group is a blend layer: `imaging_vello` pushes it with the group's own
        // clip, or against the whole surface when it has none.
        let (transform, bounds) = match &group.clip {
            Some(clip) => {
                let (transform, bounds, _) = crate::bounds::clip(clip);
                (transform, bounds)
            },
            None => (Affine::IDENTITY, self.frame),
        };
        // The layer's clip path is a path in the scene, so it takes tiles as well.
        self.charge(transform, bounds);

        let frame = self.frame;
        if let Some(blend) = &mut self.blend {
            let inside = tiles_of(frame, transform, bounds).intersect(blend.visible);
            blend.enter(Charge::Whole(inside), inside);
        }
    }

    fn pop_group(&mut self) {
        if let Some(blend) = &mut self.blend {
            blend.leave();
        }
    }

    fn fill(&mut self, draw: FillRef<'_>) {
        let (transform, bounds) = crate::bounds::fill(&draw);
        self.charge(transform, bounds);
    }

    fn stroke(&mut self, draw: StrokeRef<'_>) {
        let (transform, bounds) = crate::bounds::stroke(&draw);
        self.charge(transform, bounds);
    }

    fn glyph_run(&mut self, draw: GlyphRunRef<'_>, glyphs: &mut dyn Iterator<Item = Glyph>) {
        // Every glyph is charged its own tiles: they are separate paths to the
        // rasteriser, however close together they sit.
        let frame = self.frame;
        let mut tiles = 0;
        crate::bounds::for_each_glyph(&draw, glyphs, |transform, box_of_a_glyph| {
            tiles += charge_of(frame, transform, box_of_a_glyph);
        });
        self.tiles += tiles;
    }

    fn blurred_rounded_rect(&mut self, draw: BlurredRoundedRect) {
        let (transform, bounds) = crate::bounds::blurred(&draw);
        self.charge(transform, bounds);
    }
}

#[cfg(test)]
mod tests {
    use masonry::imaging::Painter;
    use masonry::kurbo::{Line, Point, Stroke};
    use masonry::peniko::Color;

    use super::*;

    /// `count` paths, each one a diagonal across the whole frame.
    ///
    /// The scene the reproducer of §33.1 draws, in this crate's own terms: every path
    /// covers the target, so the tiles it needs are the target's tiles.
    pub(super) fn diagonals(count: usize, frame: PhysicalSize<u32>) -> Scene {
        let (w, h) = (f64::from(frame.width), f64::from(frame.height));
        let mut scene = Scene::new();
        let mut painter = Painter::new(&mut scene);
        let stroke = Stroke::new(1.0);
        for i in 0..count {
            let y = (i % 3) as f64;
            let line = Line::new(Point::new(0.0, y), Point::new(w, h - y));
            painter.stroke(line, &stroke, Color::from_rgb8(0x80, 0xa0, 0xf0)).draw();
        }
        scene
    }

    /// The boundary, where vello itself puts it (§33.1).
    ///
    /// Measured against vello directly: at 1100x750, 646 screen-spanning paths draw
    /// and 647 come back empty; at 2200x1500, 161 draw and 162 do not. If this
    /// arithmetic drifts from those numbers the check becomes either paranoid
    /// (refusing frames that would have drawn) or blind, and both are worse than
    /// having no check.
    #[test]
    fn the_boundary_is_where_vello_puts_it() {
        let small = PhysicalSize::new(1100, 750);
        assert!(tile_demand(&diagonals(646, small), small) <= TILE_BUDGET);
        assert!(tile_demand(&diagonals(647, small), small) > TILE_BUDGET);

        let large = PhysicalSize::new(2200, 1500);
        assert!(tile_demand(&diagonals(161, large), large) <= TILE_BUDGET);
        assert!(tile_demand(&diagonals(162, large), large) > TILE_BUDGET);
    }

    /// A path hanging outside the frame pays for the part inside it, and no more.
    ///
    /// Checked against vello rather than assumed: its boundary does not move when
    /// every path is three times the size of the frame (§33.1). Counting the whole
    /// bounding box would refuse a scene that draws perfectly well — a canvas at an
    /// overview zoom has plenty of geometry outside the viewport.
    #[test]
    fn a_path_outside_the_frame_pays_for_what_is_inside() {
        let frame = PhysicalSize::new(1100, 750);
        let mut scene = Scene::new();
        Painter::new(&mut scene)
            .stroke(
                Line::new(Point::new(-5000.0, -5000.0), Point::new(6000.0, 6000.0)),
                &Stroke::new(1.0),
                Color::from_rgb8(0x80, 0xa0, 0xf0),
            )
            .draw();

        assert_eq!(tile_demand(&scene, frame), tile_demand(&diagonals(1, frame), frame));
    }

    /// The canvas's own far field has an order of magnitude of headroom, which is why
    /// §33 is a guard and not a redesign.
    ///
    /// §31 leaves a far-field frame at thirteen commands. Thirteen paths across a
    /// HiDPI window ask for 168 636 tiles of 2 097 152 — 8%, so the canvas would have
    /// to grow twelve times more *styles* (not more curves: a curve inside a batch
    /// costs no tiles of its own) before this check has anything to say to it.
    #[test]
    fn a_far_field_frame_is_far_from_the_budget() {
        let frame = PhysicalSize::new(2200, 1500);
        let demand = tile_demand(&diagonals(13, frame), frame);
        assert!(
            demand * 10 < TILE_BUDGET,
            "thirteen screen-spanning paths want {demand} of {TILE_BUDGET} tiles"
        );
    }

    /// An empty scene asks for nothing, and a scene entirely outside the frame too.
    #[test]
    fn nothing_on_screen_asks_for_no_tiles() {
        let frame = PhysicalSize::new(400, 300);
        assert_eq!(tile_demand(&Scene::new(), frame), 0);

        let mut scene = Scene::new();
        Painter::new(&mut scene)
            .fill(Rect::new(-500.0, -500.0, -100.0, -100.0), Color::from_rgb8(0xff, 0, 0))
            .draw();
        assert_eq!(tile_demand(&scene, frame), 0);
    }
}

#[cfg(test)]
mod fast_path_tests {
    use masonry::imaging::Painter;
    use masonry::kurbo::{Line, Point, Stroke};
    use masonry::peniko::Color;

    use super::tests::diagonals;
    use super::*;

    /// The cheap bound and the exact walk have to agree about the answer, or the fast
    /// path is a second implementation with its own opinion.
    #[test]
    fn the_fast_path_agrees_with_the_walk() {
        let frame = PhysicalSize::new(1100, 750);
        for count in [1, 100, 646, 647, 700, 2000] {
            let scene = diagonals(count, frame);
            let over = over_budget(&scene, frame).is_some();
            assert_eq!(
                over,
                tile_demand(&scene, frame) > TILE_BUDGET,
                "{count} paths: the fast path and the walk disagree"
            );
        }
    }

    /// And it answers the far field without looking at the geometry at all: a dozen
    /// commands cannot reach the budget however many curves are inside them, which is
    /// the case the frame path pays for on every frame.
    #[test]
    fn a_far_field_frame_never_reaches_the_walk() {
        let frame = PhysicalSize::new(2200, 1500);
        let mut scene = Scene::new();
        let stroke = Stroke::new(2.0);
        // One command, eight thousand curves in it — the batch of §31.
        let mut path = masonry::kurbo::BezPath::new();
        for i in 0..8000 {
            let x = f64::from(i % 100) * 22.0;
            let y = f64::from(i / 100) * 18.0;
            path.move_to(Point::new(x, y));
            path.curve_to(
                Point::new(x + 6.0, y),
                Point::new(x + 12.0, y + 8.0),
                Point::new(x + 18.0, y + 8.0),
            );
        }
        Painter::new(&mut scene)
            .stroke(&path, &stroke, Color::from_rgb8(0x6a, 0x7a, 0xc0))
            .draw();
        Painter::new(&mut scene)
            .stroke(
                Line::new(Point::new(0.0, 0.0), Point::new(2200.0, 1500.0)),
                &stroke,
                Color::from_rgb8(0x80, 0x80, 0x80),
            )
            .draw();

        assert_eq!(over_budget(&scene, frame), None);
    }
}

#[cfg(test)]
mod blend_tests {
    use masonry::imaging::{GeometryRef, GroupRef, Painter};
    use masonry::kurbo::{BezPath, Point, Rect};
    use masonry::peniko::{Color, Fill};

    use super::*;

    /// The frame, as a rectangle.
    fn screen(frame: PhysicalSize<u32>) -> Rect {
        Rect::new(0.0, 0.0, f64::from(frame.width), f64::from(frame.height))
    }

    /// Something to draw inside the layers, so the scene is not only nesting.
    fn content(painter: &mut Painter<'_, Scene>) {
        painter
            .fill(Rect::new(30.0, 40.0, 300.0, 300.0), Color::from_rgb8(0x40, 0x70, 0xc0))
            .draw();
    }

    fn clip_to(shape: GeometryRef<'_>) -> ClipRef<'_> {
        ClipRef::Fill {
            transform: Affine::IDENTITY,
            shape,
            fill_rule: Fill::NonZero,
        }
    }

    /// `depth` nested groups, each a visual no-op, around some content.
    fn groups(depth: usize) -> Scene {
        let mut scene = Scene::new();
        let mut painter = Painter::new(&mut scene);
        for _ in 0..depth {
            painter.push_group(GroupRef::new());
        }
        content(&mut painter);
        for _ in 0..depth {
            painter.pop_group();
        }
        scene
    }

    /// `depth` nested rectangular clips around some content.
    fn clips(depth: usize, rect: Rect) -> Scene {
        let mut scene = Scene::new();
        let mut painter = Painter::new(&mut scene);
        for _ in 0..depth {
            painter.push_clip(clip_to(GeometryRef::Rect(rect)));
        }
        content(&mut painter);
        for _ in 0..depth {
            painter.pop_clip();
        }
        scene
    }

    /// A zigzag with an edge in every tile row: a clip whose outline crosses the whole
    /// frame instead of running along its edges.
    fn zigzag(frame: PhysicalSize<u32>) -> BezPath {
        let (width, height) = (f64::from(frame.width), f64::from(frame.height));
        let mut path = BezPath::new();
        path.move_to(Point::new(0.0, 0.0));
        let mut y = 0.0;
        let mut left = false;
        while y < height {
            let (x0, x1) = if left { (width, 0.0) } else { (0.0, width) };
            path.line_to(Point::new(x0, y));
            path.line_to(Point::new(x1, y + 8.0));
            left = !left;
            y += TILE;
        }
        path.line_to(Point::new(width, height));
        path.close_path();
        path
    }

    fn over(scene: &Scene, frame: PhysicalSize<u32>) -> bool {
        blend_demand(scene, frame) > BLEND_BUDGET
    }

    const SMALL: PhysicalSize<u32> = PhysicalSize::new(1100, 750);
    const LARGE: PhysicalSize<u32> = PhysicalSize::new(2200, 1500);

    /// Where vello loses the frame to nested groups, measured against vello itself
    /// (§34.2): five nested layers fit a 1100x750 frame and six do not; on 2200x1500
    /// even five are too many.
    ///
    /// The same shape of test as `the_boundary_is_where_vello_puts_it` for tiles, and
    /// for the same reason: drift here makes the guard either blind or paranoid, and
    /// both are worse than having no guard.
    #[test]
    fn the_blend_boundary_is_where_vello_puts_it() {
        assert!(!over(&groups(5), SMALL));
        assert!(over(&groups(6), SMALL));

        assert!(!over(&groups(4), LARGE));
        assert!(over(&groups(5), LARGE));
    }

    /// A rectangular clip charges about half its outline, and that is why an area tree
    /// nests clips without ever coming near this ceiling.
    ///
    /// Measured: screen-sized clips nested on a 2200x1500 frame draw at 21 and come
    /// back empty at 22; on 1100x750 they draw at 30 and are gone at 40. Compare with
    /// groups above, which lose the frame at five.
    #[test]
    fn a_rectangular_clip_costs_about_half_its_outline() {
        assert!(!over(&clips(21, screen(LARGE)), LARGE));
        assert!(over(&clips(22, screen(LARGE)), LARGE));

        assert!(!over(&clips(30, screen(SMALL)), SMALL));
        assert!(over(&clips(40, screen(SMALL)), SMALL));
    }

    /// Inset clips are charged from their own box, not the frame's.
    ///
    /// Measured on 2200x1500 with a 32-pixel inset: 22 nested clips draw, 23 do not.
    #[test]
    fn an_inset_clip_is_charged_from_its_own_box() {
        let inset = screen(LARGE).inset(-32.0);
        assert!(!over(&clips(22, inset), LARGE));
        assert!(over(&clips(23, inset), LARGE));
    }

    /// A clip that is not a rectangle costs what a group costs, because its outline
    /// can cross every tile — and this one does.
    ///
    /// This is the case that disproves "clips are free" (§33.7 said so, §34.1 corrects
    /// it): the same twelve levels that a rectangular clip survives lose the frame at
    /// six when the clip is a zigzag.
    #[test]
    fn a_clip_that_is_not_a_rectangle_costs_what_a_group_costs() {
        let mut scenes = Vec::new();
        for depth in [5_usize, 6] {
            let path = zigzag(SMALL);
            let mut scene = Scene::new();
            let mut painter = Painter::new(&mut scene);
            for _ in 0..depth {
                painter.push_clip(clip_to(GeometryRef::Path(&path)));
            }
            content(&mut painter);
            for _ in 0..depth {
                painter.pop_clip();
            }
            scenes.push(scene);
        }
        assert!(!over(&scenes[0], SMALL));
        assert!(over(&scenes[1], SMALL));
    }

    /// A rectangular clip around every group does not move the boundary: the clips pay
    /// along their own edges and the groups pay everywhere, so the frame goes at the
    /// same depth as groups alone.
    ///
    /// Measured: on 1100x750, five clip-and-group pairs draw and six do not; on
    /// 2200x1500, four draw and five do not.
    #[test]
    fn a_clip_around_every_group_does_not_move_the_boundary() {
        fn pairs(depth: usize, frame: PhysicalSize<u32>) -> Scene {
            let mut scene = Scene::new();
            let mut painter = Painter::new(&mut scene);
            for _ in 0..depth {
                painter.push_clip(clip_to(GeometryRef::Rect(screen(frame))));
                painter.push_group(GroupRef::new());
            }
            content(&mut painter);
            for _ in 0..depth {
                painter.pop_group();
                painter.pop_clip();
            }
            scene
        }

        assert!(!over(&pairs(5, SMALL), SMALL));
        assert!(over(&pairs(6, SMALL), SMALL));

        assert!(!over(&pairs(4, LARGE), LARGE));
        assert!(over(&pairs(5, LARGE), LARGE));
    }

    /// Four groups is the free depth, and clips nested inside them keep drawing far
    /// past it — until their own edges add up.
    ///
    /// Measured on 2200x1500: twelve clips inside four groups draw, twenty do not.
    #[test]
    fn clips_inside_four_groups_run_out_eventually() {
        fn groups_then_clips(clips: usize, frame: PhysicalSize<u32>) -> Scene {
            let mut scene = Scene::new();
            let mut painter = Painter::new(&mut scene);
            for _ in 0..4 {
                painter.push_group(GroupRef::new());
            }
            for _ in 0..clips {
                painter.push_clip(clip_to(GeometryRef::Rect(screen(frame))));
            }
            content(&mut painter);
            for _ in 0..clips {
                painter.pop_clip();
            }
            for _ in 0..4 {
                painter.pop_group();
            }
            scene
        }

        assert!(!over(&groups_then_clips(12, LARGE), LARGE));
        assert!(over(&groups_then_clips(20, LARGE), LARGE));
    }

    /// Tiles outside an enclosing clip are not charged by what is nested inside it:
    /// vello's `clip_zero` branch suppresses them entirely.
    ///
    /// Without this, six groups inside a small panel would refuse a frame that vello
    /// draws without blinking — which is the shape of a real interface, not a corner
    /// case.
    #[test]
    fn a_group_inside_a_small_clip_charges_only_that_clip() {
        let panel = Rect::new(100.0, 100.0, 400.0, 300.0);
        let mut scene = Scene::new();
        let mut painter = Painter::new(&mut scene);
        painter.push_clip(clip_to(GeometryRef::Rect(panel)));
        for _ in 0..6 {
            painter.push_group(GroupRef::new());
        }
        content(&mut painter);
        for _ in 0..6 {
            painter.pop_group();
        }
        painter.pop_clip();

        assert!(!over(&scene, LARGE));
        // The whole frame at the same depth is far over, so the clip is doing the work.
        assert!(over(&groups(7), LARGE));
    }

    /// Four levels live in registers, so a scene that nests no deeper asks for nothing
    /// — and the frame path knows it from the command stream alone.
    #[test]
    fn nothing_below_the_split_asks_for_scratch() {
        assert_eq!(blend_demand(&groups(4), LARGE), 0);
        assert_eq!(nesting_depth(&groups(4)), 4);
        assert_eq!(nesting_depth(&Scene::new()), 0);
        assert_eq!(over_budget(&groups(4), LARGE), None);
    }
}
