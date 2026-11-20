//! A node editor over blazy's canvas.
//!
//! `blazy-canvas` draws a graph and picks in it; `blazy-ops` runs operators from a keymap.
//! This crate adds what every node editor needs: selection, box select, grab, pan, links,
//! copy and undo as operators, and a widget that runs them in front of a canvas and carries
//! their changes into it and into every other view of the same graph.
//!
//! * [`NodeGraph`] — what the editor needs from the application's graph.
//! * [`EditorWorld`] — what an operator may touch: the graph, the pointer and what it is over.
//! * [`ops`] — the operators, their default keymap, and a runtime with both.
//! * [`NodeEditor`] — the widget: a canvas, the operator seats, the selection overlay.
//! * [`Views`] — the registry of views over one graph, and what each is owed.

#![warn(missing_docs, unreachable_pub)]

mod edge_pan;
mod editor;
mod field;
mod menu;
pub mod ops;
mod panel;
mod screen;
mod views;
mod world;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::rc::Rc;

pub use blazy_canvas::Link;
use masonry::kurbo::{Point, Rect};

pub use crate::edge_pan::{EDGE_PAN, EdgePan, EdgePanCounters};
pub use crate::editor::{EditorSession, NodeEditor, OverlayStyle, SelectionOutline, SessionHandle};
pub use crate::field::ContentField;
pub use crate::menu::{Menu, MenuItem, MenuLayer, MenuOp};
pub use crate::panel::{BuildPanel, GraphPanel, PanelCounters};
pub use crate::screen::{SCREEN_CONTEXT, ScreenAction, ScreenOp, ScreenState, SystemClipboard};
pub use crate::views::{Change, Refresh, ViewCounters, ViewToken, Views, WatchToken, sync_canvas, sync_root};
pub use crate::world::{DEFAULT_NODE_SIZE, Edit, EditorWorld, LinkPreview, MoveRecord, MoveRecorder, Removed, journal};

/// The class a selected node wears; style it with `Selector::classes(&[SELECTED])`.
pub const SELECTED: &str = "selected";

/// What a node editor needs from the graph behind it.
///
/// The graph is the truth; every canvas over it keeps a copy of the geometry, and the editor
/// carries changes into them. Nodes are named by index; a removed node's name is a hole,
/// never shifted.
pub trait NodeGraph: 'static {
    /// How many nodes the graph holds.
    fn node_count(&self) -> usize;

    /// How many names the graph uses, holes included: one past the highest.
    ///
    /// Defaults to [`node_count`](Self::node_count), which is right only for a graph without
    /// holes.
    fn names(&self) -> usize {
        self.node_count()
    }

    /// Where node `index` is, in canvas coordinates.
    fn node_rect(&self, index: usize) -> Rect;

    /// Moves node `index` so that its top-left corner is at `pos`.
    ///
    /// Defaults to [`set_node_rect`](Self::set_node_rect) with the same size.
    fn set_node_pos(&mut self, index: usize, pos: Point) {
        let size = self.node_rect(index).size();
        self.set_node_rect(index, Rect::from_origin_size(pos, size));
    }

    /// Gives node `index` the rectangle `rect` (a move, or a resize from either edge).
    fn set_node_rect(&mut self, index: usize, rect: Rect);

    /// The least and greatest width node `index` may be resized to, in canvas units, or
    /// `None` (the default) for a node that does not resize. Only width, as in Blender.
    fn width_limits(&self, index: usize) -> Option<(f64, f64)> {
        let _ = index;
        None
    }

    /// The registry of views over this graph.
    ///
    /// Hand out a [`ViewToken`] from it in
    /// [`NodeSource::attached`](blazy_canvas::NodeSource::attached). The editor records what
    /// its operators change; record changes made past them with [`Views::note`].
    fn views(&self) -> &Views;

    /// Adds a node at `rect` and returns its name.
    ///
    /// Reuse freed names before fresh ones, so that arrays keyed by name follow the number of
    /// nodes rather than the number of edits.
    fn insert_node(&mut self, rect: Rect) -> usize;

    /// The kinds of node this graph can add, for the `Add` menu (`Shift+A`). None by default:
    /// `node.add` then calls [`insert_node`](Self::insert_node).
    fn node_kinds(&self) -> Vec<NodeKind> {
        Vec::new()
    }

    /// Adds a node of kind `kind` at `rect` and returns its name, or `None` for an unknown
    /// kind (the default). The graph may size the node itself.
    fn insert_kind(&mut self, rect: Rect, kind: &str) -> Option<usize> {
        let _ = (rect, kind);
        None
    }

    /// Puts a node back under the name it had, holding `content`, for undo and redo.
    fn restore_node(&mut self, index: usize, rect: Rect, content: Self::Content);

    /// Removes a node and every link on it, and returns the links (for undo).
    fn remove_node(&mut self, index: usize) -> Vec<Link>;

    /// Adds a link. `false` if the graph refused it — an end that does not exist, or a
    /// link that is already there.
    fn insert_link(&mut self, link: Link) -> bool;

    /// Whether the two ports of `link` go together. Every link goes by default.
    ///
    /// Asked during a link drag for the port under the pointer, so a refusal shows before
    /// the release; [`insert_link`](Self::insert_link) still has the last word.
    fn accepts_link(&self, link: Link) -> bool {
        let _ = link;
        true
    }

    /// How many links input `port` of node `index` takes; `usize::MAX` for no limit, zero
    /// for none.
    ///
    /// One by default, as in Blender: a link into a full input replaces the oldest one there,
    /// in the same undo step. Outputs have no limit.
    fn input_limit(&self, index: usize, port: u16) -> usize {
        let _ = (index, port);
        1
    }

    /// Removes a link, in either direction.
    fn remove_link(&mut self, link: Link);

    /// What a node holds besides its rectangle. Opaque to the editor, which carries it into
    /// the history and the clipboard and back.
    type Content: Clone + 'static;

    /// What node `index` holds.
    fn content(&self, index: usize) -> Self::Content;

    /// Puts `content` into node `index`. The editor tells the views.
    fn set_content(&mut self, index: usize, content: Self::Content);

    /// How content goes to text and back, for the system clipboard. `None` (the default)
    /// keeps copies inside the process.
    fn text_form(&self) -> Option<&dyn ContentText<Self::Content>> {
        None
    }

    /// Adds a node at `rect` holding `content` and returns its name (duplicate, paste).
    fn insert_copy(&mut self, rect: Rect, content: Self::Content) -> usize;

    /// The links on node `index`, at either end.
    fn links_of(&self, index: usize) -> Vec<Link>;
}

/// A graph shared by the editor, its operators and every view of it.
pub type SharedGraph<G> = Rc<RefCell<G>>;

/// How node content goes to text and back, for the system clipboard.
///
/// The library wraps it with the copy's rectangles and links under the header
/// `blazy-nodes 1 <format>`; a copy is read only by a graph whose format has the same name.
pub trait ContentText<C> {
    /// The format's name: one word, such as `calc`.
    fn format(&self) -> &str;

    /// `content` as text.
    fn write(&self, content: &C) -> String;

    /// Reads what [`write`](Self::write) wrote; `None` otherwise. The text comes from outside
    /// the process.
    fn read(&self, text: &str) -> Option<C>;
}

/// A kind of node a graph can add.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct NodeKind {
    /// The name `node.add` takes in `kind`: one word.
    pub name: blazy_ops::keymap::Name,
    /// What the `Add` menu says.
    pub label: blazy_ops::keymap::Name,
}

impl NodeKind {
    /// A kind called `name`, shown as `label`.
    pub fn new(name: impl Into<blazy_ops::keymap::Name>, label: impl Into<blazy_ops::keymap::Name>) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
        }
    }
}
