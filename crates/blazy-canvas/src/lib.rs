//! A zoomable, pannable canvas of freely positioned widgets, for Masonry.
//!
//! * **Pan and zoom cost one `Affine`** on the content widget: no child is re-encoded or laid out again.
//! * **Off-screen nodes leave the tree.** A stashed widget is still walked by every pass, so the canvas virtualises:
//!   geometry for every node, a widget only while it is in view, and node state in the model. Links and far-field nodes
//!   are batched into few draw commands.
//! * **Ordinary widgets work inside nodes**: Masonry inverts the transform when routing pointer events.
//!
//! The canvas is two widgets, [`CanvasLayer`] (the viewport: clip and view) and
//! `CanvasContent` (the view transform and the children); one widget would zoom its own
//! clip. Operations needing the children take a [`WidgetMut`](masonry::core::WidgetMut).
//!
//! What a node is built as is decided by two rules, and the stricter wins:
//! [`DetailThresholds`] (readability by zoom) and [`DetailBudget`] (cost in widgets, to be
//! split between canvases sharing a window).
//!
//! Selection, dragging and undo are operators in `blazy-node-editor`, which drives the
//! canvas from outside.

#![warn(missing_docs, unreachable_pub)]

mod detail;
mod index;
mod links;
mod source;
mod stats;
mod widgets;

pub use crate::detail::{CanvasDetail, DEFAULT_WIDGET_BUDGET, Detail, DetailBudget, DetailThresholds};
pub use crate::links::{Link, LinkStyle, PortLayout, PortSide, Ports, link_curve};
pub use crate::source::NodeSource;
pub use crate::stats::{CanvasCounters, CanvasHit, CanvasStats};
pub use crate::widgets::{CanvasContent, CanvasLayer, WHEEL_ZOOM_RATE, wheel_pixels};
pub(crate) use crate::widgets::{FAR_OVERSCAN, region_covers, region_slack};
