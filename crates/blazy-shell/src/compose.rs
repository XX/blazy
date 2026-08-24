//! Turning a [`VisualLayerPlan`] into one scene and a list of holes.
//!
//! This is the part of the host `rnd/architecture.md` §4.2 describes as "variant 2":
//! rather than flattening the plan and forgetting it had layers, walk the layers,
//! apply the device scale, and hand back what the host itself has to draw.
//!
//! Two things live here that live nowhere else.
//!
//! **The device scale factor.** §9 counts three multipliers and puts this one in
//! composition, not in layout: `VisualLayerPlan` comes out in logical coordinates and
//! applying the window's scale is the host's job (§4.2, §23.4). One `Affine::scale`
//! at replay time, and the rasteriser redraws the curves at the physical size — which
//! is what §23.3 measured as 4.64x sharper than upscaling the same picture.
//!
//! **External layers.** Masonry records a placeholder where a widget said the host
//! draws the content (§4.3). The compatibility helpers — `root_layer`,
//! `overlay_layers`, `replay_into` — skip them by design, which is why a host that
//! wants a 3D viewport has to walk `layers` itself.

use masonry::app::{VisualLayerKind, VisualLayerPlan};
use masonry::core::WidgetId;
use masonry::imaging::Painter;
use masonry::imaging::record::{Scene, replay_transformed};
use masonry::kurbo::{Affine, Rect, Size};
use masonry::peniko::Color;

/// A rectangle the host is expected to fill itself.
///
/// In physical window coordinates, because that is the space the host composites in:
/// the caller gets a rectangle it can hand straight to a viewport pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hole {
    /// The widget that declared it.
    pub widget_id: WidgetId,
    /// Where it is, in physical window coordinates.
    pub rect: Rect,
}

/// One plan, composed.
#[derive(Debug, Default)]
pub struct Composition {
    /// Everything Masonry drew, in physical window coordinates.
    pub scene: Scene,
    /// What Masonry did not draw and left to the host, in painter order.
    pub holes: Vec<Hole>,
    /// Layers walked, holes included.
    pub layers: usize,
    /// Scene layers replayed.
    pub scenes: usize,
}

impl Composition {
    /// Walks a plan at a device scale factor.
    ///
    /// `device_scale` is the window's scale factor: 1.0 on a normal display, 2.0 on a
    /// HiDPI one. It multiplies each layer's own transform, so it reaches the
    /// rasteriser as part of the final transform and never as a resample.
    pub fn new(plan: &VisualLayerPlan, device_scale: f64) -> Self {
        let to_physical = Affine::scale(device_scale);
        let mut composition = Self::default();

        for layer in &plan.layers {
            composition.layers += 1;
            let transform = to_physical * layer.transform;
            match &layer.kind {
                VisualLayerKind::Scene(scene) => {
                    composition.scenes += 1;
                    replay_transformed(scene, &mut composition.scene, transform);
                },
                VisualLayerKind::External { bounds } => {
                    composition.holes.push(Hole {
                        widget_id: layer.widget_id,
                        rect: transform.transform_rect_bbox(*bounds),
                    });
                },
            }
        }

        composition
    }

    /// Prepends an opaque fill, so the frame is opaque before anything presents it.
    ///
    /// A widget tree is under no obligation to cover the window — `AreaScreen` paints
    /// its splitter bars and leaves the rest to its areas — and every path to the
    /// screen flattens alpha away in the end. Doing the fill here means the rasteriser
    /// blends the edges, which is what keeps a curve from becoming a staircase
    /// (§26.4).
    #[must_use]
    pub fn on_background(self, color: Color, width: u32, height: u32) -> Self {
        let mut scene = Scene::new();
        Painter::new(&mut scene)
            .fill(Rect::new(0.0, 0.0, f64::from(width), f64::from(height)), color)
            .draw();
        // Identity: the composition is already in physical coordinates.
        scene.append_transformed(&self.scene, Affine::IDENTITY);
        Self { scene, ..self }
    }

    /// Physical pixels for a logical window size, rounded outwards.
    ///
    /// Outwards rather than to nearest: a window 100.5 physical pixels wide has to be
    /// covered, and a frame one pixel short of the surface shows the surface.
    pub fn physical_size(size: Size, device_scale: f64) -> (u32, u32) {
        (
            ((size.width * device_scale).ceil() as u32).max(1),
            ((size.height * device_scale).ceil() as u32).max(1),
        )
    }
}

#[cfg(test)]
mod tests {
    use masonry::app::VisualLayer;
    use masonry::imaging::Painter;
    use masonry::kurbo::{Point, Size};
    use masonry::peniko::Color;

    use super::*;

    /// A real widget id, since `WidgetId::next` belongs to Masonry.
    ///
    /// A host never invents an id — it reads them out of a plan — so a test that
    /// wants one has to build a widget too.
    fn some_id() -> WidgetId {
        masonry::core::NewWidget::new(crate::ExternalContent::new(Size::ZERO))
            .to_pod()
            .id()
    }

    fn scene_of(rect: Rect) -> Scene {
        let mut scene = Scene::new();
        Painter::new(&mut scene).fill(rect, Color::from_rgb8(0xff, 0, 0)).draw();
        scene
    }

    fn plan(layers: Vec<VisualLayer>) -> VisualLayerPlan {
        VisualLayerPlan { layers }
    }

    /// At a scale factor of one the host must reproduce what upstream's own
    /// flattening helper produces, or every application that switches to this host
    /// changes appearance for no reason.
    #[test]
    fn composing_at_scale_one_matches_the_upstream_flattening() {
        let plan = plan(vec![
            VisualLayer {
                kind: VisualLayerKind::Scene(scene_of(Rect::new(0.0, 0.0, 10.0, 10.0))),
                transform: Affine::IDENTITY,
                widget_id: some_id(),
            },
            VisualLayer {
                kind: VisualLayerKind::Scene(scene_of(Rect::new(0.0, 0.0, 4.0, 4.0))),
                transform: Affine::translate((20.0, 5.0)),
                widget_id: some_id(),
            },
        ]);

        let mut expected = Scene::new();
        plan.replay_into(&mut expected);

        assert_eq!(Composition::new(&plan, 1.0).scene, expected);
    }

    /// The holes upstream's helpers skip are the whole reason to walk the plan.
    #[test]
    fn holes_arrive_in_window_coordinates() {
        let id = some_id();
        let plan = plan(vec![VisualLayer {
            kind: VisualLayerKind::External {
                bounds: Rect::new(0.0, 0.0, 40.0, 20.0),
            },
            transform: Affine::translate((100.0, 50.0)),
            widget_id: id,
        }]);

        let composition = Composition::new(&plan, 2.0);

        assert_eq!(composition.scenes, 0, "a hole is not drawn");
        assert_eq!(composition.holes, vec![Hole {
            widget_id: id,
            rect: Rect::new(200.0, 100.0, 280.0, 140.0),
        }]);
    }

    /// The compatibility helpers drop holes; that is exactly what this replaces.
    #[test]
    fn the_upstream_helpers_would_have_lost_the_hole() {
        let plan = plan(vec![
            VisualLayer {
                kind: VisualLayerKind::Scene(scene_of(Rect::new(0.0, 0.0, 10.0, 10.0))),
                transform: Affine::IDENTITY,
                widget_id: some_id(),
            },
            VisualLayer {
                kind: VisualLayerKind::External {
                    bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
                },
                transform: Affine::IDENTITY,
                widget_id: some_id(),
            },
        ]);

        assert_eq!(plan.overlay_layers().count(), 0, "upstream skips it");
        assert_eq!(Composition::new(&plan, 1.0).holes.len(), 1);
    }

    /// The device scale multiplies the layer transform rather than replacing it.
    #[test]
    fn the_device_scale_composes_with_the_layer_transform() {
        let id = some_id();
        let plan = plan(vec![VisualLayer {
            kind: VisualLayerKind::External {
                bounds: Rect::from_origin_size(Point::ORIGIN, (10.0, 10.0)),
            },
            transform: Affine::scale(3.0),
            widget_id: id,
        }]);

        let rect = Composition::new(&plan, 2.0).holes[0].rect;
        assert_eq!(rect, Rect::new(0.0, 0.0, 60.0, 60.0));
    }
}
