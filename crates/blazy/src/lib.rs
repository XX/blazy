//! blazy — a Blender-style UI layer on top of Masonry.
//!
//! This is the crate an application depends on; the others are the pieces it is made
//! of. See `rnd/architecture.md` for the design it implements.
//!
//! Early, and honest about it: six subsystems exist, all measured against the
//! criteria in §20–§43, and the rest of §16 is still to be written. What is here is
//! meant to be built on rather than replaced — the shapes have been checked — but the
//! API will move.
//!
//! The facade exists for a second reason, from §15.1: everything below it sits on two
//! young crates pinned to a git commit. Re-exporting through one surface is what makes
//! it possible to absorb upstream churn in one place instead of in every application
//! that depends on this one.
//!
//! ```
//! use blazy::areas::SplitTree;
//! use blazy::canvas::{Detail, DetailBudget, DetailThresholds};
//! use blazy::ops::keymap::Keymap;
//!
//! // A screen of eight areas, each holding a canvas over one graph: the widget budget
//! // is a window quantity, so it is divided rather than repeated (§29.1).
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
//! * `window` (default) — owner mode: our own window and event loop. Turn it off for guest mode (§14) or a headless
//!   host, and nothing below pulls in a window system.
//! * `vello` — the GPU rasteriser, chosen at startup rather than at compile time (§26.2). Off by default, because it
//!   needs a graphics device.
//! * `testing` — Masonry's `TestHarness`, as `blazy::masonry::testing`, for an application's own tests.

#![warn(missing_docs, unreachable_pub)]

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
/// `Affine` — and Masonry is a git dependency pinned to one commit (§15.1). An
/// application that named its own `masonry` would have to repeat that pin exactly, or
/// end up with two copies of the crate and types that do not match across the seam.
/// Through this re-export it gets the one blazy was built and measured against.
pub use masonry;
