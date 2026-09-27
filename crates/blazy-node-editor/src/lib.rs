//! A node editor over blazy's canvas: the interaction half of one.
//!
//! `blazy-canvas` draws a graph and picks in it; `blazy-ops` runs operators from a
//! keymap. Between them sits the part every node editor has and none of them should
//! write twice: selection, box select, grab, pan and undo as operators, and a widget
//! that seats the operator layer in front of a canvas and carries what the operators
//! changed back into it — and into every other view of the same graph (§30).
//!
//! It was worked out in the `node-canvas` example first (§38, §39) and moved here
//! unchanged in behaviour: the example's measurements run against this crate now, and
//! their criteria did not move (§42).
//!
//! # The seam
//!
//! The graph is the application's. What this crate needs from it is [`NodeGraph`]:
//! how many nodes, where each one is, how to move one, and which other canvases show
//! the same graph. Nothing else — not what a node looks like (that is the canvas's
//! [`NodeSource`](blazy_canvas::NodeSource)), not what a node holds.
//!
//! * [`EditorWorld`] — everything an operator may touch: the graph, the selection, the pointer and what it is over. No
//!   widget in it (§38.3).
//! * [`ops`] — the operators, the keymap they open with, and a runtime with both.
//! * [`NodeEditor`] — the widget: a canvas, the three seats of the operator layer (§38.1), the selection overlay, and
//!   an optional statistics overlay.
//!
//! # What an area has to say about itself
//!
//! The three pieces of state a view of a graph holds and the graph does not are all
//! reachable from here: the view is the canvas's, the selection is [`EditorWorld`]'s,
//! and the history is the runtime's. Which of them an area must hand over when it is
//! rebuilt elsewhere is the open question of §16 item 6; this crate is where the
//! answer will have to be implemented.

#![warn(missing_docs, unreachable_pub)]

mod editor;
pub mod ops;
mod world;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::rc::Rc;

pub use blazy_canvas::Link;
use masonry::core::WidgetId;
use masonry::kurbo::{Point, Rect};

pub use crate::editor::{EditorSession, NodeEditor, OverlayStyle, SessionHandle};
pub use crate::world::{DEFAULT_NODE_SIZE, Edit, EditorWorld, MoveRecord, MoveRecorder, journal};

/// What a node editor needs from the graph behind it.
///
/// The model is the truth (§20.2, §30): a canvas keeps its own copy of the geometry,
/// and a second canvas over the same graph keeps another, so everything an operator
/// changes is written here and carried into the views afterwards.
///
/// Nodes are named by index, as the canvas names them.
pub trait NodeGraph: 'static {
    /// How many nodes the graph holds.
    fn node_count(&self) -> usize;

    /// Where node `index` is, in canvas coordinates.
    fn node_rect(&self, index: usize) -> Rect;

    /// Moves node `index` so that its top-left corner is at `pos`.
    fn set_node_pos(&mut self, index: usize, pos: Point);

    /// Pushes into `out` every canvas showing this graph except `this`.
    ///
    /// The canvases a moved node has to reach in the same frame (§30). Required rather
    /// than defaulted to "none", because "none" is the answer that compiles, passes
    /// every test with one view, and splits two views of one graph apart on the first
    /// drag — which is exactly how §30 was found. A graph shown in one canvas only
    /// answers with nothing, deliberately.
    ///
    /// A canvas learns its own id through
    /// [`NodeSource::attached`](blazy_canvas::NodeSource::attached), which is the natural
    /// place to record it.
    fn other_views(&self, this: WidgetId, out: &mut Vec<WidgetId>);

    /// Adds a node at `rect` and hands back the name it got.
    ///
    /// **The model names nodes, not the canvas** (§43): the model is the truth (§30), a
    /// name is part of the truth, and a canvas is a mirror that may be one of several.
    /// A name a removal freed should be handed out again before a fresh one, or the
    /// arrays a caller keys by name grow with the number of edits a session has made
    /// rather than with the number of nodes it holds — the free list `SplitTree` keeps
    /// for areas (§41.2), for the same reason.
    fn insert_node(&mut self, rect: Rect) -> usize;

    /// Puts a node back under the name it had, for undo.
    ///
    /// Separate from [`insert_node`](Self::insert_node) rather than a lucky consequence
    /// of it: a free list hands the last freed name back first, which is the right name
    /// only while nothing else was added in between. Undo has to be right always.
    fn restore_node(&mut self, index: usize, rect: Rect);

    /// Removes a node and every link that ended on it, and hands the links back.
    ///
    /// They come back because undo needs them: a delete that returned only the name
    /// would restore a node with no links, and nothing would report it.
    fn remove_node(&mut self, index: usize) -> Vec<Link>;

    /// Adds a link. `false` if the graph refused it — an end that does not exist, or a
    /// link that is already there.
    fn insert_link(&mut self, link: Link) -> bool;

    /// Removes a link, in either direction.
    fn remove_link(&mut self, link: Link);
}

/// A graph shared by the editor, its operators and every view of it.
///
/// `Rc<RefCell<_>>` rather than a channel: everything that touches it lives on the UI
/// thread, and a change has to be visible to the next build of a node immediately, not
/// one frame later.
pub type SharedGraph<G> = Rc<RefCell<G>>;
