//! blazy — a Blender-style UI layer on top of Masonry.
//!
//! The crate an application depends on; the others are the pieces it is made of, and
//! `masonry` is re-exported so an application never repeats the git pin. Early: the API
//! will move.
//!
//! ```
//! use blazy::areas::SplitTree;
//! use blazy::canvas::{Detail, DetailBudget, DetailThresholds};
//! use blazy::ops::keymap::Keymap;
//!
//! // A screen of eight areas, each holding a canvas over one graph: the widget budget
//! // is a window quantity, so it is divided rather than repeated.
//! let screen = SplitTree::balanced(8);
//! let per_area = DetailBudget::default().split(screen.area_count());
//! assert_eq!(per_area.widgets, DetailBudget::default().widgets / 8);
//!
//! // Zoom decides readability, the budget decides affordability, and the coarser wins.
//! assert_eq!(DetailThresholds::default().for_scale(0.01), Detail::Box);
//! assert!(Keymap::new().is_empty());
//! ```
//!
//! # Features
//!
//! * `window` (default) — owner mode: our own window and event loop, and [`app`], the assembly of editors in areas and
//!   windows that drives it. Turn it off for guest mode or a headless host, and nothing below pulls in a window system.
//! * `vello` — the GPU rasteriser, chosen at startup rather than at compile time. Off by default, because it needs a
//!   graphics device.
//! * `testing` — Masonry's `TestHarness`, as `blazy::masonry::testing`, for an application's own tests.

#![warn(missing_docs, unreachable_pub)]

#[cfg(feature = "window")]
pub use blazy_app as app;
pub use blazy_areas as areas;
pub use blazy_canvas as canvas;
pub use blazy_node_editor as node_editor;
pub use blazy_ops as ops;
pub use blazy_shape as shape;
pub use blazy_shell as shell;
pub use blazy_widgets as widgets;
/// The Masonry this library is built against.
///
/// Every public signature here speaks Masonry's types — `NewWidget`, `WidgetMut`,
/// `Affine` — and Masonry is a git dependency pinned to one commit. An
/// application that named its own `masonry` would have to repeat that pin exactly, or
/// end up with two copies of the crate and types that do not match across the seam.
/// Through this re-export it gets the one blazy was built and measured against.
pub use masonry;
