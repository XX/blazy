//! The host: windows, input, composition, and the choice of rasteriser.
//!
//! `rnd/architecture.md` §16 item 1, and §3's rule about where platform knowledge
//! lives: `blazy-areas`, `blazy-canvas`, `blazy-ops` and `blazy-shape` know about
//! `masonry_core` and `imaging` and nothing about windows; everything that knows about
//! `winit` or a GPU is here.
//!
//! # What is here
//!
//! * [`Backend`] — the rasteriser, chosen when the application starts rather than when it is compiled (§23.5, §26.2).
//! * [`Composition`] — the walk over a [`VisualLayerPlan`](masonry::app::VisualLayerPlan) that applies the device scale
//!   factor and collects the [`Hole`]s Masonry left for the host to fill.
//! * [`Host`] — the two of them together: a plan in, a [`Frame`] out.
//! * [`ExternalContent`] — the widget that declares a hole.
//! * [`Presenter`] — how a composed frame reaches the screen: the seam sits after composition, not around the
//!   rasteriser (§27.2).
//! * [`tiles`] — what vello will allocate for a frame, counted before it is sent, so a scene it would drop in silence
//!   is refused out loud (§33, §34).
//! * `gpu`, `layers` and `encode`, behind `vello` — the frame kept on the GPU (§27), a texture per layer so an idle
//!   area is copied rather than drawn (§36, §37.2), and the path segments a frame costs, counted without a device
//!   (§35.1).
//! * [`window`] — owner mode, behind `window`: windows, an event loop, frames on the screen, and the host seat of the
//!   operator layer (§39.5). One driver per process, and every window it hears from is named (§44).
//!
//! # What is deliberately not here
//!
//! Guest mode (§14) — an engine handing us its device, queue and texture — is nearly
//! free from upstream (`render_to_texture` already accepts them) but cannot be
//! checked without a real engine, so it waits for `blazy-embed-*` (§16 item 13).
//! The owner-mode loop holds one window; a second one and detaching an area into it
//! are §16 item 6, still open.

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
pub mod window;

#[cfg(test)]
mod tests;

// Types and budgets at the root; the functions that compute them stay in their modules,
// because `demand`, `segments` and `bounds` are names a library cannot own (§15.1).
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
