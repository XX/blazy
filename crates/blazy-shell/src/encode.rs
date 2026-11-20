//! What the rasteriser will actually be asked to draw, counted without a device.
//!
//! The rasteriser charges for path segments, not draw commands. vello's CPU encoding
//! counts both (`draw_tags`, `n_path_segments`), exactly and without a device. A
//! measurement, not a guard: encoding costs milliseconds and does not belong on the
//! frame path.

use masonry::dpi::PhysicalSize;
use masonry::imaging::record::{Scene, replay};
use masonry::kurbo::Rect;

/// What one composed scene amounts to once vello has encoded it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Encoded {
    /// Draw objects: one per command the rasteriser has to set up.
    pub objects: usize,
    /// Path segments: one per line, quadratic or cubic in the scene.
    pub segments: u64,
}

/// Encodes a composed scene the way the GPU path would, and counts what came out.
///
/// `frame` is the target the scene is encoded against, in physical pixels; it bounds
/// the surface clip the sink sets up, exactly as [`crate::gpu::GpuFrames`] does.
pub fn encoded(scene: &Scene, frame: PhysicalSize<u32>) -> Encoded {
    let mut native = imaging_vello::vello::Scene::new();
    let bounds = Rect::new(0.0, 0.0, f64::from(frame.width), f64::from(frame.height));
    let mut sink = imaging_vello::VelloSceneSink::new(&mut native, bounds);
    replay(scene, &mut sink);
    sink.finish().expect("the composed scene encodes");

    let encoding = native.encoding();
    Encoded {
        objects: encoding.draw_tags.len(),
        segments: u64::from(encoding.n_path_segments),
    }
}

/// Segments alone, for a caller that only wants the number a frame is charged in.
pub fn segments(scene: &Scene, frame: PhysicalSize<u32>) -> u64 {
    encoded(scene, frame).segments
}

#[cfg(test)]
mod tests {
    use masonry::imaging::Painter;
    use masonry::kurbo::{CubicBez, Line, Point, RoundedRect, Stroke};
    use masonry::peniko::Color;

    use super::*;

    const FRAME: PhysicalSize<u32> = PhysicalSize::new(800, 600);

    fn ink() -> Color {
        Color::from_rgb8(0x40, 0x70, 0xc0)
    }

    /// What each far-field shape costs in segments: a rounded rectangle eight, a plain one
    /// four, a stroked curve two.
    #[test]
    fn a_rounded_rectangle_costs_twice_a_plain_one() {
        let mut scene = Scene::new();
        Painter::new(&mut scene)
            .fill(RoundedRect::new(10.0, 10.0, 110.0, 60.0, 6.0), ink())
            .draw();
        assert_eq!(encoded(&scene, FRAME).segments, 8);

        let mut scene = Scene::new();
        Painter::new(&mut scene)
            .fill(Rect::new(10.0, 10.0, 110.0, 60.0), ink())
            .draw();
        assert_eq!(encoded(&scene, FRAME).segments, 4);

        let mut scene = Scene::new();
        Painter::new(&mut scene)
            .stroke(
                CubicBez::new(
                    Point::new(10.0, 10.0),
                    Point::new(40.0, 10.0),
                    Point::new(70.0, 50.0),
                    Point::new(100.0, 50.0),
                ),
                &Stroke::new(1.0),
                ink(),
            )
            .draw();
        assert_eq!(encoded(&scene, FRAME).segments, 2);
    }

    /// A straight line costs exactly what the curve costs.
    #[test]
    fn a_line_costs_what_a_curve_costs() {
        let mut scene = Scene::new();
        Painter::new(&mut scene)
            .stroke(
                Line::new(Point::new(10.0, 10.0), Point::new(100.0, 50.0)),
                &Stroke::new(1.0),
                ink(),
            )
            .draw();
        assert_eq!(encoded(&scene, FRAME).segments, 2);
    }

    /// Batching changes the number of objects and not the number of segments.
    #[test]
    fn a_batch_changes_objects_and_not_segments() {
        let curve = |i: usize| {
            let x = 10.0 + i as f64 * 3.0;
            CubicBez::new(
                Point::new(x, 10.0),
                Point::new(x + 4.0, 10.0),
                Point::new(x + 8.0, 40.0),
                Point::new(x + 12.0, 40.0),
            )
        };

        let mut separate = Scene::new();
        let mut painter = Painter::new(&mut separate);
        for i in 0..64 {
            painter.stroke(curve(i), &Stroke::new(1.0), ink()).draw();
        }
        let separate = encoded(&separate, FRAME);

        let mut batched = Scene::new();
        let mut path = masonry::kurbo::BezPath::new();
        for i in 0..64 {
            let c = curve(i);
            path.move_to(c.p0);
            path.curve_to(c.p1, c.p2, c.p3);
        }
        Painter::new(&mut batched)
            .stroke(&path, &Stroke::new(1.0), ink())
            .draw();
        let batched = encoded(&batched, FRAME);

        assert_eq!(separate.objects, 64);
        assert_eq!(batched.objects, 1);
        assert_eq!(separate.segments, batched.segments);
    }
}
