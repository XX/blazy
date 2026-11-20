//! How a composed frame reaches the screen.
//!
//! The seam sits after composition (the device scale, the base colour, the holes happen
//! once), not around the rasteriser: `imaging_wgpu::TextureRenderer` cannot be a trait
//! object.
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PresentCounters {
    /// Frames presented.
    pub frames: u64,
    /// Bytes of frame data that travelled through main memory; zero on the GPU path.
    pub cpu_bytes: u64,
    /// Times a frame was copied back from the GPU; zero on the swapchain path.
    pub readbacks: u64,
    /// Holes reported to the host, summed.
    pub holes: u64,
    /// Frames refused because the rasteriser would have dropped them silently.
    pub frames_refused: u64,
}

/// Errors from putting a frame on the screen.
#[derive(Debug)]
#[non_exhaustive]
pub enum PresentError {
    /// The frame could not be composed or rasterised.
    Host(crate::HostError),
    /// The scene needs more tiles than the rasteriser can allocate; not sent (it would be
    /// dropped silently). The CPU rasteriser has no such limit.
    SceneTooLarge {
        /// Tiles the scene asks for.
        tiles: u64,
        /// Tiles the rasteriser can allocate.
        budget: u64,
    },
    /// The scene nests layers deeper than the rasteriser's blend scratch allows.
    ///
    /// The sibling of [`Self::SceneTooLarge`], in the second of vello's six fixed
    /// buffers, and the one a user interface reaches first: five nested groups over a
    /// HiDPI window are enough. Not sent to the GPU either, and for the same reason —
    /// the frame would come back missing with `Ok`. The caller can flatten the
    /// nesting, make the window smaller, or fall back to the CPU rasteriser.
    SceneTooDeep {
        /// Words of blend scratch the scene asks for.
        words: u64,
        /// Words the rasteriser can allocate.
        budget: u64,
    },
    /// The platform refused the buffer or the surface.
    Platform(String),
}

impl std::fmt::Display for PresentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host(error) => write!(f, "{error}"),
            Self::SceneTooLarge { tiles, budget } => write!(
                f,
                "the scene needs {tiles} tiles and the rasteriser can allocate {budget}"
            ),
            Self::SceneTooDeep { words, budget } => write!(
                f,
                "the scene nests deep enough to need {words} words of blend scratch and the rasteriser can allocate {budget}"
            ),
            Self::Platform(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PresentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Host(error) => Some(error),
            // The two overflows are this crate's own findings and wrap nothing; the
            // platform's error arrives as text on purpose, so it has no cause
            // to hand on either.
            _ => None,
        }
    }
}

impl From<crate::HostError> for PresentError {
    fn from(error: crate::HostError) -> Self {
        Self::Host(error)
    }
}

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

    /// The rectangles the last frame left for the host to fill.
    fn holes(&self) -> &[Hole];

    /// What this presenter's frames have cost so far.
    fn counters(&self) -> PresentCounters;

    /// Tells the presenter the window changed size, in physical pixels.
    fn resize(&mut self, size: PhysicalSize<u32>);

    /// Names the layers whose pixels may be kept between frames.
    ///
    /// A presenter that cannot keep pixels ignores this, and that is the honest answer
    /// for the blit path: it rasterises into a buffer on the CPU and has no texture to
    /// keep. The default does nothing, so a presenter written later is not obliged to
    /// have an opinion.
    fn cache_layers(&mut self, ids: Vec<masonry::core::WidgetId>) {
        let _ = ids;
    }
}
