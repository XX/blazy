//! Turning a [`VisualLayerPlan`] into one scene and a list of holes.
//!
//! Walks the layers rather than flattening the plan: applies the device scale (one
//! `Affine::scale` at replay, so curves are drawn at physical size rather than upscaled)
//! and collects the external layers the host draws itself, which Masonry's flattening
//! helpers skip.

use masonry::app::{VisualLayer, VisualLayerKind, VisualLayerPlan};
use masonry::core::WidgetId;
use masonry::imaging::Painter;
use masonry::imaging::record::{Scene, replay_transformed};
use masonry::kurbo::{Affine, Rect, Size};
use masonry::peniko::Color;

/// A rectangle the host is expected to fill itself, in physical window coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hole {
    /// The widget that declared it.
    pub widget_id: WidgetId,
    /// Where it is, in physical window coordinates.
    pub rect: Rect,
}

/// One plan, composed.
#[derive(Debug, Default)]
#[non_exhaustive]
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

/// What a caller wants done with one scene layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayerChoice {
    /// Replay it into the frame's scene, which is what an ordinary frame does.
    Draw,
    /// Leave it out: its pixels are coming from somewhere else.
    #[cfg_attr(
        not(feature = "vello"),
        expect(dead_code, reason = "only the layer cache keeps a layer")
    )]
    Keep,
}

impl Composition {
    /// Walks a plan at the window's device scale factor, applied to each layer's transform.
    pub fn new(plan: &VisualLayerPlan, device_scale: f64) -> Self {
        Self::build(plan, device_scale, None, |_, _, _, _| LayerChoice::Draw)
    }

    /// The same walk, over an opaque fill covering `width` x `height` physical pixels, so
    /// the rasteriser blends edges against it. One command in front of the walk.
    pub fn on_background(plan: &VisualLayerPlan, device_scale: f64, color: Color, width: u32, height: u32) -> Self {
        Self::build(plan, device_scale, Some((color, width, height)), |_, _, _, _| {
            LayerChoice::Draw
        })
    }

    /// The same walk, with the caller deciding which scene layers are drawn.
    pub(crate) fn build(
        plan: &VisualLayerPlan,
        device_scale: f64,
        background: Option<(Color, u32, u32)>,
        mut choose: impl FnMut(usize, &VisualLayer, &Scene, Affine) -> LayerChoice,
    ) -> Self {
        let to_physical = Affine::scale(device_scale);
        let mut composition = Self::default();

        // First, under everything, in physical coordinates.
        if let Some((color, width, height)) = background {
            Painter::new(&mut composition.scene)
                .fill(Rect::new(0.0, 0.0, f64::from(width), f64::from(height)), color)
                .draw();
        }

        for (index, layer) in plan.layers.iter().enumerate() {
            composition.layers += 1;
            let transform = to_physical * layer.transform;
            match &layer.kind {
                VisualLayerKind::Scene(scene) => {
                    if choose(index, layer, scene, transform) == LayerChoice::Draw {
                        composition.scenes += 1;
                        replay_transformed(scene, &mut composition.scene, transform);
                    }
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

    /// The base colour goes under everything, and nothing else about the walk changes.
    ///
    /// It used to be prepended to a finished composition, which meant copying the whole
    /// frame into a second scene once per frame. Painting it first has to produce the
    /// same commands in the same order, and this is where that is checked rather than
    /// assumed.
    #[test]
    fn the_background_is_the_first_command_and_the_plan_follows() {
        let plan = plan(vec![VisualLayer {
            kind: VisualLayerKind::Scene(scene_of(Rect::new(0.0, 0.0, 10.0, 10.0))),
            transform: Affine::IDENTITY,
            widget_id: some_id(),
        }]);
        let base = Color::from_rgb8(0x14, 0x14, 0x18);

        let composed = Composition::on_background(&plan, 1.0, base, 40, 20);

        let mut expected = Scene::new();
        Painter::new(&mut expected)
            .fill(Rect::new(0.0, 0.0, 40.0, 20.0), base)
            .draw();
        expected.append_transformed(&Composition::new(&plan, 1.0).scene, Affine::IDENTITY);

        assert_eq!(composed.scene, expected);
        assert_eq!(composed.scenes, 1, "the background is not a layer of the plan");
        assert_eq!(composed.layers, 1);
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
