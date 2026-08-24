//! The host: a chosen rasteriser, a composed plan, and the frame that comes out.
//!
//! This is the piece `rnd/architecture.md` §23.4 wrote by hand in forty lines of test
//! code when the sharpness measurement needed a device scale factor and the upstream
//! harness would not apply one. It belongs here, and the sharpness tests now go
//! through it rather than through a copy of it.

use masonry::app::VisualLayerPlan;
use masonry::dpi::PhysicalSize;
use masonry::imaging::RgbaImage;
use masonry::imaging::render::{ImageRenderer, ImageRendererError};
use masonry::kurbo::Size;
use masonry::peniko::Color;

use crate::backend::{Backend, BackendError};
use crate::compose::{Composition, Hole};

/// What one composed and rasterised frame consists of.
pub struct Frame {
    /// The pixels, at physical size.
    pub image: RgbaImage,
    /// Rectangles the host is expected to fill itself, in physical coordinates.
    ///
    /// Empty for an application without external content, which is most of them.
    pub holes: Vec<Hole>,
}

/// Why a frame could not be produced.
#[derive(Debug)]
pub enum HostError {
    /// The chosen backend could not be opened.
    Backend(BackendError),
    /// The rasteriser refused the scene or the target.
    Render(ImageRendererError),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(error) => write!(f, "{error}"),
            Self::Render(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for HostError {}

/// Counters, for the criteria and for anyone wondering what a frame cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostCounters {
    /// Frames composed and rasterised.
    pub frames: u64,
    /// Visual layers walked, holes included.
    pub layers: u64,
    /// Scene layers replayed into the frame.
    pub scenes: u64,
    /// Holes reported to the host.
    ///
    /// Compared against what the widgets declared, this is what says whether a hole
    /// was lost on the way (§26.1).
    pub holes: u64,
    /// Bytes of frame that became pixels in main memory.
    ///
    /// Counted here because here is where it happens: a rasterised frame *is* this
    /// many bytes, and anything downstream that moves them is moving these. A path
    /// that never asks the host for an image never adds to it, which is what the
    /// criteria of §27.4 are about.
    pub image_bytes: u64,
}

/// Composes plans and rasterises them, with the backend chosen at startup.
///
/// Owns no window: it turns a plan into pixels, and where those pixels go is
/// [`crate::window`]'s business in owner mode and the engine's in guest mode (§14).
pub struct Host {
    backend: Backend,
    renderer: Box<dyn ImageRenderer>,
    device_scale: f64,
    /// What shows where nothing was drawn.
    ///
    /// The host's business rather than the tree's: a window has to have something
    /// behind its widgets, and a rasteriser clears to transparent.
    background: Option<Color>,
    counters: HostCounters,
}

impl Host {
    /// A host on a named backend.
    pub fn new(backend: Backend) -> Result<Self, BackendError> {
        Ok(Self::with_renderer(backend, backend.open()?))
    }

    /// A host on the first backend this machine can provide.
    pub fn any() -> Result<Self, BackendError> {
        let (backend, renderer) = crate::backend::open_any()?;
        Ok(Self::with_renderer(backend, renderer))
    }

    fn with_renderer(backend: Backend, renderer: Box<dyn ImageRenderer>) -> Self {
        Self {
            backend,
            renderer,
            device_scale: 1.0,
            background: None,
            counters: HostCounters::default(),
        }
    }

    /// Which rasteriser this host opened.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// The window's scale factor. `1.0` on a normal display, `2.0` on a HiDPI one.
    pub fn device_scale(&self) -> f64 {
        self.device_scale
    }

    /// Sets the window's scale factor.
    ///
    /// Deliberately nothing else happens: the scale reaches the rasteriser through
    /// the composition transform, so a display change costs a frame and never a
    /// layout pass (§9, §22). That is the claim four criteria and seven tests hold
    /// for `ui_scale`, and this is the same line drawn for the third multiplier.
    pub fn set_device_scale(&mut self, scale: f64) {
        self.device_scale = scale;
    }

    pub fn with_device_scale(mut self, scale: f64) -> Self {
        self.set_device_scale(scale);
        self
    }

    /// Fills the frame with `color` before the plan is replayed over it.
    pub fn with_background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    pub fn counters(&self) -> HostCounters {
        self.counters
    }

    /// Composes a plan and rasterises it.
    ///
    /// `size` is the window's logical size; the frame comes out at that size times
    /// the device scale, redrawn rather than resampled.
    pub fn render(&mut self, plan: &VisualLayerPlan, size: Size) -> Result<Frame, HostError> {
        let (width, height) = self.physical_size(size);
        self.render_sized(plan, PhysicalSize::new(width, height))
    }

    /// Composes a plan and rasterises it at an exact frame size.
    ///
    /// What a window uses, because it already knows the size in physical pixels: going
    /// through the logical size and back would divide by the scale factor and multiply
    /// again, and on a fractional factor — 1.1458 on the machine this was written on —
    /// that round trip can land a pixel away from the surface it has to cover.
    pub fn render_sized(&mut self, plan: &VisualLayerPlan, size: PhysicalSize<u32>) -> Result<Frame, HostError> {
        let (width, height) = (size.width.max(1), size.height.max(1));
        let composition = Composition::new(plan, self.device_scale);

        self.counters.frames += 1;
        self.counters.layers += composition.layers as u64;
        self.counters.scenes += composition.scenes as u64;
        self.counters.holes += composition.holes.len() as u64;
        self.counters.image_bytes += u64::from(width) * u64::from(height) * 4;

        let composition = match self.background {
            Some(color) => composition.on_background(color, width, height),
            None => composition,
        };
        let mut scene = composition.scene;
        let image = self
            .renderer
            .render_source(&mut scene, width, height)
            .map_err(HostError::Render)?;

        Ok(Frame {
            image,
            holes: composition.holes,
        })
    }

    /// Physical pixels for a logical size at this host's device scale.
    fn physical_size(&self, size: Size) -> (u32, u32) {
        Composition::physical_size(size, self.device_scale)
    }
}

#[cfg(test)]
mod tests {
    use masonry::app::{VisualLayer, VisualLayerKind};
    use masonry::imaging::Painter;
    use masonry::imaging::record::Scene;
    use masonry::kurbo::{Affine, Rect};
    use masonry::peniko::Color;

    use super::*;

    fn some_id() -> masonry::core::WidgetId {
        masonry::core::NewWidget::new(crate::ExternalContent::new(Size::ZERO))
            .to_pod()
            .id()
    }

    fn one_rect_plan() -> VisualLayerPlan {
        let mut scene = Scene::new();
        Painter::new(&mut scene)
            .fill(Rect::new(0.0, 0.0, 10.0, 10.0), Color::from_rgb8(0xff, 0, 0))
            .draw();
        VisualLayerPlan {
            layers: vec![VisualLayer {
                kind: VisualLayerKind::Scene(scene),
                transform: Affine::IDENTITY,
                widget_id: some_id(),
            }],
        }
    }

    fn host() -> Host {
        Host::any().expect("some backend opens")
    }

    #[test]
    fn a_frame_comes_out_at_the_physical_size() {
        let plan = one_rect_plan();
        let mut host = host();

        let frame = host.render(&plan, Size::new(40.0, 20.0)).unwrap();
        assert_eq!((frame.image.width, frame.image.height), (40, 20));

        host.set_device_scale(2.0);
        let frame = host.render(&plan, Size::new(40.0, 20.0)).unwrap();
        assert_eq!((frame.image.width, frame.image.height), (80, 40));
    }

    /// A fractional physical size has to be covered, not rounded away.
    #[test]
    fn a_fractional_scale_rounds_the_frame_outwards() {
        let mut host = host().with_device_scale(1.25);
        let frame = host.render(&one_rect_plan(), Size::new(10.2, 10.0)).unwrap();
        assert_eq!((frame.image.width, frame.image.height), (13, 13));
    }

    /// The scale is applied to the drawing, not to the picture: at twice the scale
    /// the same rectangle covers twice as many pixels in each direction.
    #[test]
    fn the_device_scale_reaches_the_rasteriser() {
        let plan = one_rect_plan();
        let mut host = host();

        let small = host.render(&plan, Size::new(20.0, 20.0)).unwrap().image;
        host.set_device_scale(2.0);
        let big = host.render(&plan, Size::new(20.0, 20.0)).unwrap().image;

        let red = |image: &RgbaImage| {
            image
                .data
                .chunks_exact(4)
                .filter(|pixel| pixel[0] > 0x80 && pixel[1] < 0x40)
                .count()
        };
        assert_eq!(red(&big), red(&small) * 4, "the rectangle covers four times the pixels");
    }

    #[test]
    fn counters_follow_the_plan() {
        let mut host = host();
        host.render(&one_rect_plan(), Size::new(10.0, 10.0)).unwrap();
        host.render(&one_rect_plan(), Size::new(10.0, 10.0)).unwrap();

        assert_eq!(host.counters(), HostCounters {
            frames: 2,
            layers: 2,
            scenes: 2,
            holes: 0,
            image_bytes: 10 * 10 * 4 * 2,
        });
    }
}
