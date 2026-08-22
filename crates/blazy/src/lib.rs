//! blazy — a Blender-style UI layer on top of Masonry.
//!
//! This is the crate an application depends on; the others are the pieces it is made
//! of. See `rnd/architecture.md` for the design it implements.
//!
//! Early, and honest about it: two subsystems exist, both measured against the
//! criteria in §20–§22, and the rest of §16 is still to be written. What is here is
//! meant to be built on rather than replaced — the shapes have been checked — but the
//! API will move.
//!
//! The facade exists for a second reason, from §15.1: everything below it sits on two
//! young crates pinned to a git commit. Re-exporting through one surface is what makes
//! it possible to absorb upstream churn in one place instead of in every application
//! that depends on this one.

pub use blazy_areas as areas;
pub use blazy_canvas as canvas;
