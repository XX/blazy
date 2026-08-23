//! Which rasteriser draws the frame, chosen when the application starts.
//!
//! Upstream picks a backend with a cascade of `cfg(feature)` in
//! `masonry_imaging::image_render` and stores the concrete type in its window runner
//! (`rnd/architecture.md` §23.5). Neither is a limitation of the design: the backend
//! modules are gated independently, so several can be built at once, and
//! `imaging::render::ImageRenderer` is object-safe, so the choice can be a value.
//!
//! What that choice covers is narrower than it sounds, and §26.2 says why:
//! `ImageRenderer` renders into a buffer the caller owns, while the texture seam has
//! associated types and cannot be a trait object at all. So this is a choice of
//! *rasteriser*, not of the path to the screen — the path to the screen is written
//! once, in [`crate::window`], for whichever rasteriser was chosen.

use std::fmt;

use masonry::imaging::render::ImageRenderer;

/// A rasteriser this build can use.
///
/// Non-exhaustive because backends arrive by feature, and matching exhaustively on
/// them would break every application that turns a new one on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Backend {
    /// `imaging_vello_cpu`: CPU rasterisation, no GPU, no device.
    VelloCpu,
    /// `imaging_vello`: GPU rasterisation through `wgpu`.
    ///
    /// Needs a device, which a machine may not have — see [`BackendError::Unavailable`].
    #[cfg(feature = "vello")]
    Vello,
}

/// Every backend built into this binary, in the order they are offered.
///
/// The registry, and the list an application shows a user. A backend that is compiled
/// in but missing here is a wiring mistake and nothing else would report it, which is
/// why a criterion counts exactly that (§26.4).
pub const COMPILED: &[Backend] = &[
    Backend::VelloCpu,
    #[cfg(feature = "vello")]
    Backend::Vello,
];

/// Why a backend could not be opened.
#[derive(Debug)]
pub enum BackendError {
    /// The backend is not part of this build.
    ///
    /// Also what a backend in [`COMPILED`] with no constructor behind it would
    /// return: the criterion of §26.4 counts these.
    NotCompiled(Backend),
    /// This machine could not provide what the backend needs — no GPU, no device.
    ///
    /// A fact about the machine, not a defect in the build, and the reason the
    /// registry reports it rather than panicking: an application that cannot open
    /// the GPU backend should fall back to the CPU one, not fail to start.
    Unavailable { backend: Backend, reason: String },
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCompiled(backend) => {
                write!(f, "backend {} is not part of this build", backend.name())
            },
            Self::Unavailable { backend, reason } => {
                write!(f, "backend {} is unavailable here: {reason}", backend.name())
            },
        }
    }
}

impl std::error::Error for BackendError {}

impl Backend {
    /// The stable name, as an application would spell it on a command line.
    pub const fn name(self) -> &'static str {
        match self {
            Self::VelloCpu => "vello_cpu",
            #[cfg(feature = "vello")]
            Self::Vello => "vello",
        }
    }

    /// Whether this backend needs a graphics device to open.
    ///
    /// The distinction a criterion needs: a backend that cannot open because the
    /// machine has no GPU is not the same failure as one that cannot open because
    /// nobody wired it up.
    pub const fn needs_device(self) -> bool {
        match self {
            Self::VelloCpu => false,
            #[cfg(feature = "vello")]
            Self::Vello => true,
        }
    }

    /// The backend with this name, if this build has it.
    pub fn from_name(name: &str) -> Option<Self> {
        COMPILED.iter().copied().find(|backend| backend.name() == name)
    }

    /// Creates a renderer.
    ///
    /// The value that makes the choice a runtime one: a `Box<dyn ImageRenderer>`
    /// chosen here is the only thing the rest of the host knows about rasterisation.
    pub fn open(self) -> Result<Box<dyn ImageRenderer>, BackendError> {
        match self {
            Self::VelloCpu => Ok(Box::new(imaging_vello_cpu::VelloCpuRenderer::new(1, 1))),
            #[cfg(feature = "vello")]
            Self::Vello => masonry_imaging::vello::new_headless_renderer()
                .map(|renderer| Box::new(renderer) as Box<dyn ImageRenderer>)
                .map_err(|error| BackendError::Unavailable {
                    backend: self,
                    reason: error.to_string(),
                }),
        }
    }
}

/// The first backend that opens, in the order of [`COMPILED`].
///
/// What an application does when the user expressed no preference: try them in order
/// and take the first that this machine can actually provide.
pub fn open_any() -> Result<(Backend, Box<dyn ImageRenderer>), BackendError> {
    let mut last = BackendError::NotCompiled(Backend::VelloCpu);
    for &backend in COMPILED {
        match backend.open() {
            Ok(renderer) => return Ok((backend, renderer)),
            Err(error) => last = error,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wiring criterion, as a test as well as a counter: a backend that is listed
    /// but has no constructor behind it would be offered to a user and then fail.
    #[test]
    fn every_compiled_backend_is_wired() {
        for &backend in COMPILED {
            match backend.open() {
                Ok(_) => {},
                // A missing device is a fact about the machine; CI has no GPU.
                Err(BackendError::Unavailable { .. }) if backend.needs_device() => {},
                Err(error) => panic!("{} is compiled in but does not open: {error}", backend.name()),
            }
        }
    }

    #[test]
    fn backends_are_addressable_by_name() {
        for &backend in COMPILED {
            assert_eq!(Backend::from_name(backend.name()), Some(backend));
        }
        assert_eq!(Backend::from_name("no_such_backend"), None);
    }

    #[test]
    fn a_build_always_has_at_least_one_backend() {
        assert!(!COMPILED.is_empty());
        assert!(open_any().is_ok());
    }
}
