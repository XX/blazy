//! The host: windows, input, composition, and the choice of rasteriser.
//!
//! Everything that knows about `winit` or a GPU is here; the other crates know Masonry only.
//!
//! # What is here
//!
//! * [`Backend`] — the rasteriser, chosen when the application starts rather than when it is compiled.
//! * [`Composition`] — the walk over a [`VisualLayerPlan`](masonry::app::VisualLayerPlan) that applies the device scale
//!   factor and collects the [`Hole`]s Masonry left for the host to fill.
//! * [`Host`] — the two of them together: a plan in, a [`Frame`] out.
//! * [`ExternalContent`] — the widget that declares a hole.
//! * [`Presenter`] — how a composed frame reaches the screen: the seam sits after composition, not around the
//!   rasteriser.
//! * [`tiles`] — what vello will allocate for a frame, counted before it is sent, so a scene it would drop in silence
//!   is refused out loud.
//! * `gpu`, `layers` and `encode`, behind `vello` — the frame kept on the GPU, a texture per layer so an idle area is
//!   copied rather than drawn, and the path segments a frame costs, counted without a device.
//! * [`window`] — owner mode, behind `window`: windows, an event loop, frames on the screen, and the host seat of the
//!   operator layer. One driver per process, and every window it hears from is named.
//! * [`text`] — the clipboard and the input method, behind `window`: what the text widgets inside nodes need from the
//!   platform, and the rule that keeps `Ctrl+V` a key for a keymap outside a text field.
//!
//! Not here: guest mode (an engine's device and texture), until there is an engine to
//! check it with.

#![warn(missing_docs, unreachable_pub)]

mod backend;
mod bounds;
mod compose;
#[cfg(feature = "vello")]
pub mod encode;
mod external;
#[cfg(feature = "vello")]
pub mod gpu;
mod host;
#[cfg(feature = "vello")]
pub mod layers;
mod present;
pub mod tiles;

#[cfg(feature = "window")]
pub mod text;
#[cfg(feature = "window")]
pub mod window;

#[cfg(test)]
mod tests;

// Types and budgets at the root; the functions that compute them stay in their modules,
// because `demand`, `segments` and `bounds` are names a library cannot own.
pub use crate::backend::{Backend, BackendError, COMPILED, open_any};
pub use crate::compose::{Composition, Hole};
#[cfg(feature = "vello")]
pub use crate::encode::Encoded;
pub use crate::external::ExternalContent;
pub use crate::host::{Frame, Host, HostCounters, HostError};
#[cfg(feature = "vello")]
pub use crate::layers::{LayerCounters, PixelRect};
pub use crate::present::{PresentCounters, PresentError, Presenter};
pub use crate::tiles::{BLEND_BUDGET, Demand, Overflow, TILE_BUDGET};
