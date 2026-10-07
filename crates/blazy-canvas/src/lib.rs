//! A zoomable, pannable canvas of freely positioned widgets, for Masonry.
//!
//! It began as the Phase 0 question from `rnd/architecture.md` — can a Blender-style
//! node editor be built on top of `masonry_core` without forking it? — and §20.8
//! answered yes. Three claims carried that answer, and they are still what the crate
//! is built around:
//!
//! 1. **Pan and zoom cost one `Affine`.** Changing the view sets a transform on the content widget. It must not
//!    re-encode any child's cached scene, and it must not re-run any child's `layout`. This is the payoff of a retained
//!    tree on top of a vector display list: the encoded scene stores curves, not triangles, so it stays sharp at any
//!    scale.
//!
//! 2. **Off-screen nodes leave the tree, not just the picture.** The claim was first stated as culling, and §20.2
//!    disproved that form of it: a stashed widget stays in every pass's recursion, so the frame cost followed the whole
//!    graph — 64× the nodes cost 81× the time at a fixed visible set. The canvas therefore *virtualises*: geometry for
//!    every node, a widget only while the node is in view, and node state in the model because the widget does not
//!    exist most of the time (§20.3). The same economy applies to the commands the canvas draws itself, and it is the
//!    reason links and far-field nodes are batched rather than drawn one shape at a time: a command is charged in every
//!    frame it sits in the scene and costs an order of magnitude more than the same geometry inside a shared one, and
//!    an idle canvas pays that bill as surely as a busy one (§31.1 has the prices).
//!
//! 3. **Ordinary widgets work inside nodes.** Masonry already inverts `window_transform` when routing pointer events,
//!    so sliders and checkboxes inside a zoomed node need no special handling from us.
//!
//! # Structure
//!
//! The canvas is two widgets, not one:
//!
//! ```text
//! CanvasLayer      viewport: fixed size, clip path, owns the view. No transform.
//!   └ CanvasContent    carries the view transform; owns the placed children.
//! ```
//!
//! They cannot be merged. A widget's transform maps its own border-box into its
//! parent's space, and the paint pass transforms the clip path by that same
//! `window_transform` (`passes/paint.rs`). A single widget holding both the clip
//! and the view would zoom its own viewport clip along with the content.
//!
//! Because a `WidgetPod` hands its widget to the arena on insertion, the canvas
//! cannot read its own children through `&self`. Everything that needs child state
//! is therefore an associated function taking a [`WidgetMut`](masonry::core::WidgetMut), which is the normal
//! Masonry idiom.
//!
//! # Level of detail
//!
//! Two rules decide what a node is built as, and the stricter one wins (§29):
//!
//! * [`DetailThresholds`] asks whether the zoom still leaves a control large enough to use. Readability, and it is what
//!   the level meant until §29.
//! * [`DetailBudget`] asks whether the resulting tree is affordable, in widgets. A zoom threshold is a constant tuned
//!   for one node size and one density; at four times the density the same zoom puts four times the tree in the window,
//!   which is how a canvas ends up holding 4507 widgets and a 31 ms frame at a zoom nothing looked wrong at.
//!
//! An application tiling several canvases in one window should divide one budget
//! between them ([`DetailBudget::split`]): the frame walks the window's tree, not any
//! single canvas's.
//!
//! # What is missing
//!
//! Not a finished node editor yet. Virtualisation, level of detail, the link layer
//! and the spatial index are in and measured (§20, §24, §29, §31, §35), and so is
//! picking by shape rather than by rectangle (§25, §40.1).
//!
//! * **The graph's shape is fixed when the canvas is built.** [`CanvasLayer::new`] takes a node count and
//!   [`CanvasLayer::with_links`] a link list; nothing adds or removes a node or a link afterwards, and a node is
//!   identified by its index, which a removal would shift.
//! * **No selection model and no serialisation of the graph here.** Selection, box-select, dragging and undo are
//!   operators in `blazy-node-editor` (§38, §39, §42), which drives this canvas from outside; serialisation is the
//!   application's, because the graph is.

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
