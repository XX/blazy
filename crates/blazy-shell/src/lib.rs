//! The host: windows, input, composition, and the choice of rasteriser.
//!
//! `rnd/architecture.md` §16 item 1, and §3's rule about where platform knowledge
//! lives: `blazy-areas`, `blazy-canvas` and `blazy-shape` know about `masonry_core`
//! and `imaging` and nothing about windows; everything that knows about `winit` or a
//! GPU is here.
//!
//! # What is here
//!
//! * [`Backend`] — the rasteriser, chosen when the application starts rather than when it is compiled (§23.5, §26.2).
//! * [`Composition`] — the walk over a [`VisualLayerPlan`](masonry::app::VisualLayerPlan) that applies the device scale
//!   factor and collects the [`Hole`]s Masonry left for the host to fill.
//! * [`Host`] — the two of them together: a plan in, a [`Frame`] out.
//! * [`ExternalContent`] — the widget that declares a hole.
//! * [`window`] — owner mode: a window, an event loop, and a frame on the screen.
//!
//! # What is deliberately not here
//!
//! Guest mode (§14) — an engine handing us its device, queue and texture — is nearly
//! free from upstream (`render_to_texture` already accepts them) but cannot be
//! checked without a real engine, so it waits for `blazy-embed-*` (§16 item 13).
//! Per-area render-to-texture (§7.3) is Phase 3 and belongs to `blazy-compose`; what
//! this crate owes it is to composite by walking the plan's layers rather than
//! flattening them, which [`Composition`] does.

mod backend;
mod compose;
mod external;
#[cfg(feature = "vello")]
pub mod gpu;
mod host;
mod present;

#[cfg(feature = "window")]
pub mod window;

#[cfg(test)]
mod tests;

pub use crate::backend::{Backend, BackendError, COMPILED, open_any};
pub use crate::compose::{Composition, Hole};
pub use crate::external::ExternalContent;
pub use crate::host::{Frame, Host, HostCounters, HostError};
pub use crate::present::{PresentCounters, PresentError, Presenter};
