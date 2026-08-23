//! Hit testing by the shape that was drawn, rather than by the box it was drawn in.
//!
//! `rnd/architecture.md` §6.1 gives a widget three geometries: layout is always a
//! rectangle, the visual one is arbitrary, and the hit one is arbitrary too. Masonry
//! supplies the first two and leaves a hook for the third —
//! [`Widget::find_widget_under_pointer`] — which this crate fills in.
//!
//! # Two phases, and only the second is ours
//!
//! Upstream's default already does the coarse phase while descending the tree:
//! bounding box, stashed flag, clip path, then children in reverse z-order
//! (`masonry_core::core::widget::find_widget_under_pointer`). A widget that wants a
//! precise shape adds one test in front of it, which is what [`ShapeHit::find_widget`]
//! does. Overriding the hook for *speed* would be pointless: the child walk is linear
//! in the number of children, and the tree only ever holds what is on screen.
//!
//! # Tolerance is in screen pixels
//!
//! The precise phase runs in the widget's own coordinates, and the pointer's idea of
//! "close enough" lives on the screen. Inside a zoomable canvas the two differ by the
//! zoom, which spans 0.02x to 8x — a slop of 4 canvas units is 0.08 px at the bottom
//! of that range and 32 px at the top (§25.2). So a tolerance is given in screen
//! pixels and divided by the scale at the moment of the test; [`scale_of`] extracts
//! that scale from a transform, and [`ShapeHit::find_widget`] reads it from
//! `QueryCtx::window_transform`, which carries the canvas view.
//!
//! # Why the flattened cache exists
//!
//! `BezPath::contains` re-walks the path elements on every call: measured at 576 ns
//! for an eight-segment rounded rectangle against 26 ns for the same shape as a
//! `RoundedRect`, and 37 ns for a cached polyline (§25.1). A pick tests every
//! candidate under the pointer, so the difference is the difference between
//! imperceptible and a visible fraction of a frame. The cache is built on first use
//! and lives behind a `RefCell`, because the hook takes `&self`.
//!
//! Curves that are not widgets — the canvas's links — are the other half of the
//! problem, and they get [`near_segment`] instead: they are derived from their
//! endpoints every frame, so there is nothing to cache, and the cheap rejection is
//! the control polygon rather than a stored bounding box.

use std::cell::RefCell;

use masonry::core::{QueryCtx, Widget, WidgetRef, find_widget_under_pointer};
use masonry::kurbo::{Affine, BezPath, ParamCurveNearest, PathEl, PathSeg, Point, Rect, Shape, flatten};
use masonry::peniko::Fill;

/// Default picking tolerance for a stroke, in screen pixels.
///
/// About a millimetre on a typical display: enough that a two-pixel curve can be
/// hit without aiming, small enough that two curves a node apart are still distinct.
pub const DEFAULT_SLOP: f64 = 3.0;

/// How far a flattened contour may deviate from the curve, in local units.
///
/// Only the fill cache is flattened, and only for a containment test, so the error
/// budget is "smaller than a pixel at a plausible zoom" rather than "invisible when
/// drawn". A quarter of a unit keeps the polyline short: the eight-segment rounded
/// rectangle of §25.1 flattens to sixteen points.
const FLATTEN_TOLERANCE: f64 = 0.25;

/// Accuracy asked of [`ParamCurveNearest::nearest`].
///
/// **Not a distance error bound**, which is the trap: measured on one link-shaped
/// cubic, an accuracy of 0.1 answered up to 1.66 units away from the true distance,
/// which would eat half of a three-pixel tolerance. At 0.01 the worst error is 0.003
/// units and at 0.001 it is zero to double precision — while the cost barely moves
/// (123 ns at 0.1, 162 ns at 0.001, §25.1), because the expensive part is being
/// called at all rather than being called precisely.
const NEAREST_ACCURACY: f64 = 0.001;

/// What counts as being on the shape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HitMode {
    /// Inside the filled area.
    ///
    /// `rule` decides what "inside" means where a path crosses itself, and it should
    /// be the same rule the shape is painted with — otherwise the hole a user can see
    /// is not the hole they can click through.
    Fill { rule: Fill },
    /// Within half of `width` of the outline, plus `slop` screen pixels.
    ///
    /// `width` is in local units, because it is the width the shape is stroked with
    /// and that is how a stroke is specified. `slop` is in screen pixels, because it
    /// is about the pointer rather than about the drawing.
    Stroke { width: f64, slop: f64 },
}

/// A precise hit shape, with the caches that make testing it cheap.
///
/// Built once and kept by the widget, typically rebuilt in `layout` when the size it
/// is derived from changes. The path is in the widget's **content-box coordinates**,
/// which is the space `QueryCtx::to_local` maps a window position into.
#[derive(Debug)]
pub struct ShapeHit {
    path: BezPath,
    mode: HitMode,
    /// The path's bounding box, widened by half the stroke width.
    ///
    /// Not by the slop: that is in screen pixels and only known at test time.
    bounds: Rect,
    /// Flattened contours, built on first use. See the module docs.
    flat: RefCell<Option<Vec<(Point, Point)>>>,
}

impl ShapeHit {
    /// A shape hit when the point is inside it, by the non-zero winding rule.
    pub fn fill(shape: impl Shape) -> Self {
        Self::new(shape, HitMode::Fill { rule: Fill::NonZero })
    }

    /// A shape hit when the point is inside it, by an explicit fill rule.
    pub fn fill_with(shape: impl Shape, rule: Fill) -> Self {
        Self::new(shape, HitMode::Fill { rule })
    }

    /// An outline hit within half `width` of the curve, plus [`DEFAULT_SLOP`].
    pub fn stroke(shape: impl Shape, width: f64) -> Self {
        Self::new(shape, HitMode::Stroke {
            width,
            slop: DEFAULT_SLOP,
        })
    }

    /// An outline with an explicit tolerance, in screen pixels.
    pub fn stroke_with(shape: impl Shape, width: f64, slop: f64) -> Self {
        Self::new(shape, HitMode::Stroke { width, slop })
    }

    fn new(shape: impl Shape, mode: HitMode) -> Self {
        let path = shape.to_path(FLATTEN_TOLERANCE);
        let outset = match mode {
            HitMode::Fill { .. } => 0.0,
            HitMode::Stroke { width, .. } => width / 2.0,
        };
        Self {
            bounds: path.bounding_box().inflate(outset, outset),
            path,
            mode,
            flat: RefCell::new(None),
        }
    }

    pub fn mode(&self) -> HitMode {
        self.mode
    }

    pub fn path(&self) -> &BezPath {
        &self.path
    }

    /// The shape's bounding box, including half the stroke width but not the slop.
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// Whether `point` — in the shape's own coordinates — hits it.
    ///
    /// `scale` is how many screen pixels one local unit covers, which is what turns a
    /// tolerance in pixels into one in local units. Pass `1.0` where the shape is
    /// already in screen coordinates.
    pub fn contains(&self, point: Point, scale: f64) -> bool {
        match self.mode {
            HitMode::Fill { rule } => {
                if !within(self.bounds, point) {
                    return false;
                }
                let winding = self.winding(point);
                match rule {
                    Fill::EvenOdd => winding % 2 != 0,
                    _ => winding != 0,
                }
            },
            HitMode::Stroke { width, slop } => {
                let radius = width / 2.0 + local_slop(slop, scale);
                if !within(self.bounds.inflate(radius, radius), point) {
                    return false;
                }
                self.path.segments().any(|seg| near_segment(seg, point, radius))
            },
        }
    }

    /// The precise phase, in front of Masonry's coarse one.
    ///
    /// Drop-in body for [`Widget::find_widget_under_pointer`]:
    ///
    /// ```ignore
    /// fn find_widget_under_pointer<'c>(&'c self, ctx: QueryCtx<'c>, pos: Point)
    ///     -> Option<WidgetRef<'c, dyn Widget>> {
    ///     self.hit.find_widget(self, ctx, pos)
    /// }
    /// ```
    ///
    /// The shape gates the whole subtree, exactly as a clip path does: a child
    /// sticking out past the shape is not reachable through it. That is what makes it
    /// usable for a node body — the point of a precise shape is that the corner it
    /// cuts away belongs to whatever is behind it.
    pub fn find_widget<'c>(
        &self,
        widget: &'c dyn Widget,
        ctx: QueryCtx<'c>,
        pos: Point,
    ) -> Option<WidgetRef<'c, dyn Widget>> {
        // The two cheap rejects upstream would do anyway, repeated here because this
        // runs before it: without them a point far outside the widget would pay for a
        // winding test.
        if !ctx.bounding_box().contains(pos) || ctx.is_stashed() {
            return None;
        }
        if !self.contains(ctx.to_local(pos), scale_of(ctx.window_transform())) {
            return None;
        }
        find_widget_under_pointer(widget, ctx, pos)
    }

    /// Winding number of the flattened contours around `point`.
    fn winding(&self, point: Point) -> i32 {
        let mut cache = self.flat.borrow_mut();
        let edges = cache.get_or_insert_with(|| flatten_contours(&self.path));
        let mut winding = 0;
        for &(a, b) in edges.iter() {
            // Half-open in y, so a vertex shared by two edges is counted once.
            if (a.y <= point.y) != (b.y <= point.y) {
                let t = (point.y - a.y) / (b.y - a.y);
                if a.x + t * (b.x - a.x) > point.x {
                    winding += if b.y > a.y { 1 } else { -1 };
                }
            }
        }
        winding
    }
}

/// How many screen pixels one local unit covers under `transform`.
///
/// The length of the transformed unit x vector. For the transforms a UI applies —
/// translation, uniform scale, rotation — that is the scale; for a non-uniform one it
/// is the horizontal scale, which is a choice rather than a truth, and the honest
/// thing to say is that a pointer tolerance under anisotropic scaling is ambiguous
/// anyway.
pub fn scale_of(transform: Affine) -> f64 {
    let c = transform.as_coeffs();
    (c[0] * c[0] + c[1] * c[1]).sqrt()
}

/// Whether `point` lies within `radius` of `seg`.
///
/// For curves that are not widgets and are rebuilt every frame — the canvas's links —
/// so there is nothing to cache and the rejection has to be free-standing. The
/// control polygon contains the curve, so its bounding box is a valid conservative
/// reject, and it is seven times cheaper than the exact answer (§25.1).
pub fn near_segment(seg: PathSeg, point: Point, radius: f64) -> bool {
    if !within(hull(seg).inflate(radius, radius), point) {
        return false;
    }
    seg.nearest(point, NEAREST_ACCURACY).distance_sq <= radius * radius
}

/// The bounding box of a segment's control polygon: cheap, and never smaller than
/// the curve's own bounding box.
pub fn hull(seg: PathSeg) -> Rect {
    match seg {
        PathSeg::Line(l) => Rect::from_points(l.p0, l.p1),
        PathSeg::Quad(q) => Rect::from_points(q.p0, q.p1).union(Rect::from_points(q.p2, q.p2)),
        PathSeg::Cubic(c) => Rect::from_points(c.p0, c.p1).union(Rect::from_points(c.p2, c.p3)),
    }
}

/// Whether `point` is in `rect`, edges included.
///
/// `Rect::contains` excludes the far edges, and every use here is a conservative
/// rejection in front of an exact test: excluding the boundary would drop hits that
/// land exactly on it, which for a rectangle derived from a tolerance is not a rare
/// case but the common one.
fn within(rect: Rect, point: Point) -> bool {
    point.x >= rect.x0 && point.x <= rect.x1 && point.y >= rect.y0 && point.y <= rect.y1
}

/// A tolerance in screen pixels, in local units.
///
/// A degenerate scale — a collapsed transform — would otherwise turn into an infinite
/// tolerance, which is the one failure mode that would make everything hit at once.
fn local_slop(slop: f64, scale: f64) -> f64 {
    if scale > f64::EPSILON { slop / scale } else { 0.0 }
}

/// Flattens a path into closed contours, as line segments.
///
/// Closing each contour is what makes the winding test correct for an open path: a
/// fill is defined by the closed shape whether or not the path says `ClosePath`,
/// which is also how a renderer treats it.
fn flatten_contours(path: &BezPath) -> Vec<(Point, Point)> {
    let mut edges = Vec::new();
    let (mut start, mut current) = (Point::ZERO, Point::ZERO);
    let mut open = false;
    let close = |from: Point, to: Point, edges: &mut Vec<(Point, Point)>| {
        if from != to {
            edges.push((from, to));
        }
    };

    flatten(path.iter(), FLATTEN_TOLERANCE, |el| match el {
        PathEl::MoveTo(p) => {
            if open {
                close(current, start, &mut edges);
            }
            start = p;
            current = p;
            open = true;
        },
        PathEl::LineTo(p) => {
            edges.push((current, p));
            current = p;
        },
        PathEl::ClosePath => {
            close(current, start, &mut edges);
            current = start;
        },
        // `flatten` emits nothing else: curves arrive as `LineTo`.
        _ => {},
    });
    if open {
        close(current, start, &mut edges);
    }
    edges
}

#[cfg(test)]
mod tests {
    use masonry::kurbo::{Circle, CubicBez, Line, RoundedRect, Size, Vec2};

    use super::*;

    fn body() -> RoundedRect {
        RoundedRect::from_rect(Rect::from_origin_size(Point::ORIGIN, Size::new(160.0, 96.0)), 6.0)
    }

    /// The whole point: a corner inside the bounding box but outside the shape is a
    /// miss, and the same point tested as a rectangle is a hit.
    #[test]
    fn a_rounded_corner_is_not_hit() {
        let hit = ShapeHit::fill(body());
        let corner = Point::new(1.0, 1.0);
        assert!(hit.bounds().contains(corner), "the corner is inside the bounding box");
        assert!(!hit.contains(corner, 1.0), "but outside the shape");
        assert!(hit.contains(Point::new(80.0, 48.0), 1.0));
        assert!(hit.contains(Point::new(6.0, 6.0), 1.0), "inside the corner radius");
    }

    /// The flattened cache is an approximation, so it is pinned against the exact
    /// answer over a grid rather than at a few hand-picked points.
    #[test]
    fn the_flattened_cache_agrees_with_the_exact_shape() {
        let shape = body();
        let hit = ShapeHit::fill(shape);
        let mut checked = 0;
        for x in 0..161 {
            for y in 0..97 {
                let p = Point::new(x as f64, y as f64);
                // Points within the flattening tolerance of the outline may legitimately
                // disagree; everything else must not.
                if shape
                    .to_path(0.01)
                    .segments()
                    .any(|s| s.nearest(p, 0.01).distance_sq < 1.0)
                {
                    continue;
                }
                assert_eq!(hit.contains(p, 1.0), shape.contains(p), "at {p:?}");
                checked += 1;
            }
        }
        assert!(checked > 10_000, "only checked {checked} points");
    }

    /// Two concentric circles wound the same way: non-zero says the middle is inside,
    /// even-odd says it is a hole. The rule has to be the one the shape is painted
    /// with, so both must work.
    #[test]
    fn the_fill_rule_decides_what_a_hole_is() {
        let mut path = Circle::new(Point::new(50.0, 50.0), 40.0).to_path(0.1);
        path.extend(Circle::new(Point::new(50.0, 50.0), 20.0).to_path(0.1));
        let centre = Point::new(50.0, 50.0);
        let ring = Point::new(80.0, 50.0);

        let non_zero = ShapeHit::fill(path.clone());
        assert!(non_zero.contains(centre, 1.0) && non_zero.contains(ring, 1.0));

        let even_odd = ShapeHit::fill_with(path, Fill::EvenOdd);
        assert!(!even_odd.contains(centre, 1.0), "the inner circle is a hole");
        assert!(even_odd.contains(ring, 1.0));
    }

    /// An open path fills as if it were closed, which is how it is drawn.
    #[test]
    fn an_open_contour_is_closed_for_the_fill_test() {
        let mut path = BezPath::new();
        path.move_to((0.0, 0.0));
        path.line_to((100.0, 0.0));
        path.line_to((100.0, 100.0));
        path.line_to((0.0, 100.0));
        let hit = ShapeHit::fill(path);
        assert!(hit.contains(Point::new(50.0, 50.0), 1.0));
        assert!(!hit.contains(Point::new(150.0, 50.0), 1.0));
    }

    #[test]
    fn a_stroke_is_hit_along_the_curve_and_not_beside_it() {
        let curve = CubicBez::new((0.0, 0.0), (40.0, 0.0), (60.0, 100.0), (100.0, 100.0));
        let hit = ShapeHit::stroke_with(curve, 2.0, 0.0);
        assert!(hit.contains(curve.p0, 1.0));
        assert!(
            hit.contains(Point::new(50.0, 50.0), 1.0),
            "the curve passes through here"
        );
        assert!(!hit.contains(Point::new(50.0, 70.0), 1.0));
    }

    /// The reason the tolerance is in screen pixels: the same slop must cover more
    /// local units when the shape is drawn smaller, or a curve becomes unclickable
    /// exactly when it is thinnest (§25.2).
    #[test]
    fn the_slop_is_screen_pixels_not_local_units() {
        let hit = ShapeHit::stroke_with(Line::new((0.0, 0.0), (100.0, 0.0)), 0.0, 4.0);
        let near = Point::new(50.0, 6.0);

        assert!(!hit.contains(near, 1.0), "6 local units away, 4 px of slop");
        assert!(hit.contains(near, 0.5), "zoomed out, 6 local units are 3 px");
        assert!(!hit.contains(Point::new(50.0, 40.0), 0.5), "but not without limit");
    }

    /// A collapsed transform must not make everything hit at once.
    #[test]
    fn a_degenerate_scale_does_not_widen_the_slop() {
        let hit = ShapeHit::stroke_with(Line::new((0.0, 0.0), (100.0, 0.0)), 2.0, 4.0);
        assert!(!hit.contains(Point::new(50.0, 500.0), 0.0));
    }

    #[test]
    fn scale_of_reads_the_scale_out_of_a_transform() {
        assert_eq!(scale_of(Affine::IDENTITY), 1.0);
        assert_eq!(scale_of(Affine::scale(2.5)), 2.5);
        assert_eq!(scale_of(Affine::translate(Vec2::new(9.0, -3.0))), 1.0);
        let rotated = Affine::rotate(0.7) * Affine::scale(3.0);
        assert!((scale_of(rotated) - 3.0).abs() < 1e-12);
    }

    /// The cheap reject must never reject something the exact test would accept:
    /// checked against the exact answer over a grid around the curve.
    #[test]
    fn the_control_polygon_never_rejects_a_real_hit() {
        let curve = PathSeg::Cubic(CubicBez::new((0.0, 0.0), (40.0, 0.0), (60.0, 100.0), (100.0, 100.0)));
        let radius = 3.0;
        for x in -20..120 {
            for y in -20..120 {
                let p = Point::new(x as f64 * 1.0, y as f64 * 1.0);
                // A hair inside the tolerance, so that a point sitting exactly on it
                // is not a disagreement about arithmetic.
                let margin = radius - 0.01;
                let exact = curve.nearest(p, 1e-9).distance_sq < margin * margin;
                if exact {
                    assert!(near_segment(curve, p, radius), "rejected a real hit at {p:?}");
                }
            }
        }
    }
}
