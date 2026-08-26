//! What the rasteriser will actually be asked to draw, counted without a device.
//!
//! `rnd/architecture.md` §32 measured the second half of a frame and found that it does
//! not follow the number of draw commands at all: the same eight thousand curves cost
//! the same 34 ms whether they arrive in two commands or in eight thousand. What the
//! rasteriser charges for is **path segments** — a line, a quadratic or a cubic in the
//! flattened scene — and nothing in the plan counts those.
//!
//! vello can encode a scene on the CPU, and the encoding says exactly how much of both
//! there is: `draw_tags` is one entry per draw object, `n_path_segments` one per
//! segment. Both are exact and machine-independent, so a criterion may stand on them
//! (§20.9), and they exist on a machine with no graphics device at all.
//!
//! **This is a measurement, not a guard.** Encoding a scene costs milliseconds — the
//! whole point of [`crate::tiles::over_budget`] is that it answers in front of every
//! frame without encoding anything. Nothing here belongs on the frame path.

use masonry::dpi::PhysicalSize;
use masonry::imaging::record::{Scene, replay};
use masonry::kurbo::Rect;

/// What one composed scene amounts to once vello has encoded it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Encoded {
    /// Draw objects: one per command the rasteriser has to set up.
    pub objects: usize,
    /// Path segments: one per line, quadratic or cubic in the scene.
    ///
    /// The unit a far-field frame is charged in (§32.3). A rounded rectangle is eight
    /// of them, a link curve one.
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

    /// What each shape of the far field costs in the unit that matters.
    ///
    /// The measurement the levers are chosen from (§35.1): a node drawn as a rounded
    /// rectangle is eight segments, the same node as a plain rectangle is four, and a
    /// stroked link curve is two — a stroke pays for both sides of its outline. So five
    /// thousand nodes outweigh eight thousand links, which is not what §31 — written
    /// when commands were the unit — would have suggested.
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

    /// And a straight line costs exactly what the curve costs, which is why "simpler
    /// geometry" is not a lever (§31.1 found the same for encoding, §35.2 for
    /// rasterising).
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

    /// Batching changes the number of objects and not the number of segments — the
    /// whole of §32 in one assertion.
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
