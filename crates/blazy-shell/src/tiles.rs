//! What vello will try to allocate for a frame, worked out before the frame is sent.
//!
//! `rnd/architecture.md` §33: vello sizes its bump-allocated buffers with numbers
//! that do not depend on the scene at all — `vello_encoding::BufferSizes::new` calls
//! them "hand picked to accommodate the vello test scenes" — and the tile buffer is
//! `1 << 21` entries. A scene that needs more overflows, the coarse stage gives up,
//! and **nothing says so**: the robust read-back of the bump allocators is compiled in
//! only with vello's `debug_layers` feature, so `render_to_texture` returns `Ok` and
//! the target keeps whatever was in it — the previous frame, or nothing at all.
//!
//! That is not a defect this crate can fix (§33.2 says where it is filed), but it is
//! one it can *see coming*: the demand is exactly
//!
//! ```text
//! sum over paths of  tiles(bbox of the path, clipped to the frame)
//! ```
//!
//! with 16x16 tiles, and it is arithmetic we can do on the scene we already composed.
//! Measured against vello directly, the boundary is exact to the path: at 1100x750,
//! 646 screen-spanning paths draw and 647 do not; at 2200x1500, 161 draw and 162 do
//! not (§33.1). The clipping matters and was checked rather than assumed — paths
//! three times the size of the frame move the boundary not at all.
//!
//! Tiles are one buffer of six sized this way, and not the one a user interface is
//! likeliest to run out of — five nested blend layers overflow `blend_spill` on a HiDPI
//! window, just as silently (§33.7). This module does not model that one yet.
//!
//! # What this counts, and what it approximates
//!
//! Fills, strokes and clip shapes are counted from their bounding boxes, which is what
//! vello bins on. Glyphs are charged a square of the font size each, because a glyph
//! is a path and its outline fits in its em box. Groups and image draws are not
//! counted: neither allocates tiles per path in vello's coarse stage.
//!
//! Every approximation here is deliberately on the **generous** side: charging too
//! much refuses a frame that would have drawn, and that is a visible, reportable
//! failure. Charging too little brings back the silent one.

use masonry::dpi::PhysicalSize;
use masonry::imaging::record::{Glyph, Scene, replay};
use masonry::imaging::{
    BlurredRoundedRect, ClipRef, FillRef, GeometryRef, GlyphRunRef, GroupRef, PaintSink, StrokeRef,
};
use masonry::kurbo::{Affine, Rect, Shape};

/// Side of a vello tile, in device pixels.
const TILE: f64 = 16.0;

/// Tiles vello can allocate for one frame, whatever that frame contains.
///
/// `vello_encoding::BufferSizes::new`, `tiles = BufferSize::new(1 << 21)` — the same in
/// 0.9 and in 0.10, checked. Pinned along with the rasteriser: `imaging_vello` 0.0.2
/// selects vello 0.9 (§27.1), and a version that sizes this buffer from the scene would
/// make the whole check moot.
pub const TILE_BUDGET: u64 = 1 << 21;

/// Whether a scene is over the budget, and by how much — the question the frame path
/// asks, answered without walking the scene when the answer is obvious.
///
/// A scene cannot need more tiles than its command count times the frame's own tiles,
/// because no single path can be charged for more than the whole frame. That bound
/// costs two multiplications, and for anything a user interface actually draws it is
/// already far under the budget: a far-field canvas frame is a dozen commands, and a
/// dozen frames' worth of tiles is 8% of what vello can allocate. So the exact walk —
/// which is proportional to the geometry, and measured at 1.8 ms on a scene of 8000
/// curves (§33.3) — runs only for scenes that could plausibly be over.
///
/// Returns `None` when the scene fits, `Some(tiles)` with the exact demand when it
/// does not.
pub fn tiles_over_budget(scene: &Scene, frame: PhysicalSize<u32>) -> Option<u64> {
    let frame_tiles = u64::from(f64::from(frame.width).div_euclid(TILE) as u32 + 1)
        * u64::from(f64::from(frame.height).div_euclid(TILE) as u32 + 1);
    let ceiling = (scene.commands().len() as u64).saturating_mul(frame_tiles);
    if ceiling <= TILE_BUDGET {
        return None;
    }

    let demand = tile_demand(scene, frame);
    (demand > TILE_BUDGET).then_some(demand)
}

/// Tiles a composed scene will ask vello for, at this frame size.
///
/// The exact walk. Compare against [`TILE_BUDGET`]: above it, the frame does not reach
/// the screen and vello says nothing about it. [`tiles_over_budget`] is what a frame
/// path should call; this is for a test or a measurement that wants the number itself.
pub fn tile_demand(scene: &Scene, frame: PhysicalSize<u32>) -> u64 {
    let mut counter = TileCounter {
        frame: Rect::new(0.0, 0.0, f64::from(frame.width), f64::from(frame.height)),
        tiles: 0,
    };
    replay(scene, &mut counter);
    counter.tiles
}

/// Adds up the tiles, one path at a time.
struct TileCounter {
    /// The frame, in physical pixels: vello bins only over the target (§33.1).
    frame: Rect,
    tiles: u64,
}

impl TileCounter {
    /// Charges one path, given its bounding box in scene coordinates.
    fn charge(&mut self, transform: Affine, bounds: Rect) {
        let bounds = transform.transform_rect_bbox(bounds).intersect(self.frame);
        if bounds.is_zero_area() {
            return;
        }
        // Tiles are a fixed grid, so a box that straddles a tile boundary pays for
        // both — which is why this is not `width / TILE` rounded up.
        let across = (bounds.x1 / TILE).ceil() - (bounds.x0 / TILE).floor();
        let down = (bounds.y1 / TILE).ceil() - (bounds.y0 / TILE).floor();
        self.tiles += (across.max(1.0) * down.max(1.0)) as u64;
    }

    fn charge_shape(&mut self, transform: Affine, shape: &GeometryRef<'_>, outset: f64) {
        let bounds = match shape {
            GeometryRef::Rect(rect) => rect.bounding_box(),
            GeometryRef::RoundedRect(rect) => rect.bounding_box(),
            // `bounding_box` solves each curve for its extrema, which sounds like a
            // lot in front of every frame. The cheap alternative — the box over the
            // control points, never smaller — was tried and changed the measured cost
            // of this walk by 2% (§33.3): the time is in walking the scene, not in the
            // arithmetic. So the tighter box stays, because it refuses less.
            GeometryRef::Path(path) => path.bounding_box(),
            GeometryRef::OwnedPath(path) => path.bounding_box(),
        };
        self.charge(transform, bounds.inflate(outset, outset));
    }
}

impl PaintSink for TileCounter {
    fn push_clip(&mut self, clip: ClipRef<'_>) {
        // A clip shape is a path like any other, and pays like one.
        match clip {
            ClipRef::Fill { transform, shape, .. } => self.charge_shape(transform, &shape, 0.0),
            ClipRef::Stroke {
                transform,
                shape,
                stroke,
            } => self.charge_shape(transform, &shape, stroke.width / 2.0),
        }
    }

    fn pop_clip(&mut self) {}

    fn push_group(&mut self, _group: GroupRef<'_>) {}

    fn pop_group(&mut self) {}

    fn fill(&mut self, draw: FillRef<'_>) {
        self.charge_shape(draw.transform, &draw.shape, 0.0);
    }

    fn stroke(&mut self, draw: StrokeRef<'_>) {
        // Half the width on each side, before the transform: the stroke is in the
        // geometry's own space, which is the whole of §31.3.
        self.charge_shape(draw.transform, &draw.shape, draw.stroke.width / 2.0);
    }

    fn glyph_run(&mut self, draw: GlyphRunRef<'_>, glyphs: &mut dyn Iterator<Item = Glyph>) {
        // A glyph is a path, and its outline fits inside its em box. Counting the
        // glyphs costs one pass over an iterator this pass would otherwise skip.
        let em = f64::from(draw.font_size);
        let box_of_a_glyph = Rect::new(0.0, -em, em, 0.0);
        for glyph in glyphs {
            let at = Affine::translate((f64::from(glyph.x), f64::from(glyph.y)));
            self.charge(draw.transform * at, box_of_a_glyph);
        }
    }

    fn blurred_rounded_rect(&mut self, draw: BlurredRoundedRect) {
        // The blur spreads the box, and vello draws it as one path.
        let spread = draw.std_dev * 3.0;
        self.charge(draw.transform, draw.rect.inflate(spread, spread));
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
            let over = tiles_over_budget(&scene, frame).is_some();
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

        assert_eq!(tiles_over_budget(&scene, frame), None);
    }
}
