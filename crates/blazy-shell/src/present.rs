//! How a composed frame reaches the screen.
//!
//! The seam `rnd/architecture.md` §27.2 puts *after* composition rather than around
//! the rasteriser, and the reason is §26.2: `imaging_wgpu::TextureRenderer` has
//! associated types, so it cannot be a trait object, while composition is the same
//! work whichever way the frame travels. Everything above this trait — the device
//! scale, the base colour, the holes — happens once.
//!
//! Two implementations, chosen at startup with the rasteriser:
//!
//! * [`BlitPresenter`](crate::window::BlitPresenter) rasterises into a buffer and copies it into the window. Works with
//!   any [`Backend`](crate::Backend), needs no graphics device, and costs one pass over the frame plus the platform's
//!   copy.
//! * [`SwapchainPresenter`](crate::gpu::SwapchainPresenter) renders the scene into a texture and blits it into the
//!   swapchain. The frame never enters main memory.

use masonry::app::VisualLayerPlan;
use masonry::dpi::PhysicalSize;

use crate::compose::Hole;

/// What a frame cost on its way to the screen.
///
/// Two of these are the criteria of §27.4, and both are counters rather than times:
/// whether a frame passes through main memory is a fact about the path, not about the
/// machine it ran on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresentCounters {
    /// Frames presented.
    pub frames: u64,
    /// Bytes of frame data that travelled through main memory.
    ///
    /// Zero for a path that keeps the frame on the GPU. For the blit path it is the
    /// size of the rasterised image, which is what has to be written and then read
    /// again to reach the window.
    pub cpu_bytes: u64,
    /// Times a frame was copied back from the GPU.
    ///
    /// A GPU rasteriser that has to hand back pixels does `copy_texture_to_buffer`
    /// and maps the result once per frame. Zero is the point of the swapchain path.
    pub readbacks: u64,
    /// Holes reported to the host, summed.
    pub holes: u64,
}

/// Errors from putting a frame on the screen.
#[derive(Debug)]
pub enum PresentError {
    /// The frame could not be composed or rasterised.
    Host(crate::HostError),
    /// The platform refused the buffer or the surface.
    Platform(String),
}

impl std::fmt::Display for PresentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host(error) => write!(f, "{error}"),
            Self::Platform(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PresentError {}

/// A path from a [`VisualLayerPlan`] to the screen.
pub trait Presenter {
    /// Stable name, for diagnostics and for the benchmark's report.
    fn name(&self) -> &'static str;

    /// Composes the plan and puts the result on the screen.
    ///
    /// `frame` is the size the result has to cover, in physical pixels — the window's
    /// own size, not a size derived from it. `device_scale` is the window's scale
    /// factor, and it reaches the rasteriser as a transform rather than as a size.
    fn present(
        &mut self,
        plan: &VisualLayerPlan,
        frame: PhysicalSize<u32>,
        device_scale: f64,
    ) -> Result<(), PresentError>;

    /// The rectangles the last frame left for the host to fill (§4.3).
    fn holes(&self) -> &[Hole];

    fn counters(&self) -> PresentCounters;

    /// Tells the presenter the window changed size, in physical pixels.
    fn resize(&mut self, size: PhysicalSize<u32>);
}
