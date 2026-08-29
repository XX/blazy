//! Where a drawing command lands, in one place.
//!
//! Two things in this crate need the bounding box of what a scene draws, and they need
//! it for opposite reasons: [`crate::tiles`] charges each box to the rasteriser's tile
//! budget, and [`crate::layers`] unions the boxes to find the rectangle a cached layer
//! occupies. The arithmetic is the same and the mistakes would be silent — a predictor
//! that refuses a frame and a cache that copies the wrong pixels are the same wrong box
//! seen from two sides — so it lives here and neither of them has an opinion of its own.
//!
//! What each shape is worth, and why:
//!
//! * a fill, a clip or a stroke is its geometry's bounding box, which is what vello bins on; a stroke is inflated by
//!   half its width, in the geometry's own space (§31.3);
//! * a glyph is charged its em box, because a glyph is a path and its outline fits inside one;
//! * a blurred rectangle spreads by three standard deviations, which is where the filter is cut off.

use masonry::imaging::record::Glyph;
use masonry::imaging::{BlurredRoundedRect, ClipRef, FillRef, GeometryRef, GlyphRunRef, StrokeRef};
use masonry::kurbo::{Affine, Rect, Shape};

/// The bounding box of a shape, in its own coordinates.
pub(crate) fn shape_bounds(shape: &GeometryRef<'_>) -> Rect {
    match shape {
        GeometryRef::Rect(rect) => rect.bounding_box(),
        GeometryRef::RoundedRect(rect) => rect.bounding_box(),
        // `bounding_box` solves each curve for its extrema, which sounds like a lot in
        // front of every frame. The cheap alternative — the box over the control
        // points, never smaller — was tried and changed the measured cost of a scene
        // walk by 2% (§33.3): the time goes into walking the scene, not into the
        // arithmetic. So the tighter box stays, because it refuses less.
        GeometryRef::Path(path) => path.bounding_box(),
        GeometryRef::OwnedPath(path) => path.bounding_box(),
    }
}

/// Whether a shape's outline stays along the edges of its bounding box.
///
/// True for a rectangle and a rounded rectangle, which is what an area tree and a
/// viewport clip with (§20.3), and what makes them nearly free on the blend stack
/// (§34.2). A path can wander anywhere inside its box.
pub(crate) fn is_rectangular(shape: &GeometryRef<'_>) -> bool {
    matches!(shape, GeometryRef::Rect(_) | GeometryRef::RoundedRect(_))
}

/// Where a clip lands: its transform, its box, and whether its outline hugs that box.
pub(crate) fn clip(clip: &ClipRef<'_>) -> (Affine, Rect, bool) {
    match clip {
        ClipRef::Fill { transform, shape, .. } => (*transform, shape_bounds(shape), is_rectangular(shape)),
        ClipRef::Stroke {
            transform,
            shape,
            stroke,
        } => (
            *transform,
            shape_bounds(shape).inflate(stroke.width / 2.0, stroke.width / 2.0),
            is_rectangular(shape),
        ),
    }
}

/// Where a fill lands.
pub(crate) fn fill(draw: &FillRef<'_>) -> (Affine, Rect) {
    (draw.transform, shape_bounds(&draw.shape))
}

/// Where a stroke lands: half the width on each side, before the transform, because the
/// stroke is in the geometry's own space (§31.3).
pub(crate) fn stroke(draw: &StrokeRef<'_>) -> (Affine, Rect) {
    let outset = draw.stroke.width / 2.0;
    (draw.transform, shape_bounds(&draw.shape).inflate(outset, outset))
}

/// Where a blurred rectangle lands, spread by the extent of the filter.
pub(crate) fn blurred(draw: &BlurredRoundedRect) -> (Affine, Rect) {
    let spread = draw.std_dev * 3.0;
    (draw.transform, draw.rect.inflate(spread, spread))
}

/// Where each glyph of a run lands.
///
/// A callback rather than an iterator because the glyphs arrive as a `&mut dyn Iterator`
/// the caller owns, and because the two callers do different things with each box: one
/// charges it, the other unions it.
pub(crate) fn for_each_glyph(
    draw: &GlyphRunRef<'_>,
    glyphs: &mut dyn Iterator<Item = Glyph>,
    mut each: impl FnMut(Affine, Rect),
) {
    let em = f64::from(draw.font_size);
    let box_of_a_glyph = Rect::new(0.0, -em, em, 0.0);
    for glyph in glyphs {
        let at = Affine::translate((f64::from(glyph.x), f64::from(glyph.y)));
        each(draw.transform * at, box_of_a_glyph);
    }
}
