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
//! how many nodes, where each one is, how to move one, add one and take one away, and
//! which other canvases show the same graph. Not what a node looks like (that is the
//! canvas's [`NodeSource`](blazy_canvas::NodeSource)), and not what a node holds — only
//! a way to carry it, unread, from a node into the history or the clipboard and back
//! ([`NodeGraph::Content`], §57).
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

mod edge_pan;
mod editor;
mod field;
mod menu;
pub mod ops;
mod panel;
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
pub use crate::views::{Change, Refresh, ViewCounters, ViewToken, Views, WatchToken, sync_canvas, sync_root};
pub use crate::world::{DEFAULT_NODE_SIZE, Edit, EditorWorld, LinkPreview, MoveRecord, MoveRecorder, Removed, journal};

/// The class a selected node wears (`CanvasLayer::set_node_class`).
///
/// What an application's style is written against: a layer of the node type's property
/// stack with `Selector::classes(&[SELECTED])` is how a node looks selected without
/// knowing what a selection is (§38.7). Per view, like the selection itself.
pub const SELECTED: &str = "selected";

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

    /// How many names the graph uses: one past the highest, holes a removal left included —
    /// what a canvas is built over (`CanvasLayer::new`), and what a view that starts over
    /// from a replaced graph is given (§69).
    ///
    /// Defaulted to [`node_count`](Self::node_count), which is the answer for a graph with no
    /// holes; a graph that frees names answers its own, or the nodes past the count would be
    /// missing from every view that starts over.
    fn names(&self) -> usize {
        self.node_count()
    }

    /// Where node `index` is, in canvas coordinates.
    fn node_rect(&self, index: usize) -> Rect;

    /// Moves node `index` so that its top-left corner is at `pos`.
    ///
    /// Defaulted through [`set_node_rect`](Self::set_node_rect), keeping the size (§70): a move
    /// is a rectangle with the same size, and one writer of geometry is one place to get it
    /// wrong. A graph whose positions live apart from its sizes overrides it.
    fn set_node_pos(&mut self, index: usize, pos: Point) {
        let size = self.node_rect(index).size();
        self.set_node_rect(index, Rect::from_origin_size(pos, size));
    }

    /// Gives node `index` the rectangle `rect`: a new size, and a new corner when a node is
    /// resized from its left edge (`node.resize`, §60).
    ///
    /// Required, like everything here that writes what a node is: a default that moved the
    /// node and dropped the size would compile, pass every test that counts nodes, and leave
    /// two views disagreeing about how wide a node is (§42). A graph whose nodes do not
    /// resize still writes the rectangle — whether they may is
    /// [`width_limits`](Self::width_limits).
    fn set_node_rect(&mut self, index: usize, rect: Rect);

    /// The least and the greatest width node `index` may be resized to, in canvas units, or
    /// `None` for a node that is not resized at all.
    ///
    /// Blender's `minwidth`/`maxwidth` of a node type. Defaulted to `None`: a node keeps the
    /// size it was given, which is what the editor did before this question existed, and an
    /// edge that does nothing when dragged is not a silent failure — it is simply not an
    /// edge (no cursor, no resize, `node.move` takes the drag). Only width, as in Blender,
    /// where only a frame's height can be dragged.
    fn width_limits(&self, index: usize) -> Option<(f64, f64)> {
        let _ = index;
        None
    }

    /// The canvases showing this graph, and what each of them has not been told yet.
    ///
    /// Where the other views of a moved node are found in the same frame (§30), and where
    /// the views of *other windows* collect what they missed (§44.3). Required rather than
    /// defaulted to an empty registry, because "no views" is the answer that compiles,
    /// passes every test with one view, and splits two views of one graph apart on the
    /// first drag — which is exactly how §30 was found.
    ///
    /// The graph keeps one [`Views`] and hands out a [`ViewToken`] from it in
    /// [`NodeSource::attached`](blazy_canvas::NodeSource::attached); the editor records
    /// what its operators change. What the application changes past the operators, it
    /// records with [`Views::note`].
    fn views(&self) -> &Views;

    /// Adds a node at `rect` and hands back the name it got.
    ///
    /// **The model names nodes, not the canvas** (§43): the model is the truth (§30), a
    /// name is part of the truth, and a canvas is a mirror that may be one of several.
    /// A name a removal freed should be handed out again before a fresh one, or the
    /// arrays a caller keys by name grow with the number of edits a session has made
    /// rather than with the number of nodes it holds — the free list `SplitTree` keeps
    /// for areas (§41.2), for the same reason.
    fn insert_node(&mut self, rect: Rect) -> usize;

    /// The kinds of node this graph can add by name — Blender's `Shift+A` menu (§64).
    ///
    /// Defaulted to none: a graph whose nodes are all one thing (`hello`) has nothing to
    /// choose between, and `node.add` without a kind goes on calling
    /// [`insert_node`](Self::insert_node). A graph that names kinds gets an `Add` menu
    /// listing them, and each entry runs `node.add` with `kind` set to the kind's name.
    fn node_kinds(&self) -> Vec<NodeKind> {
        Vec::new()
    }

    /// Adds a node of kind `kind` at `rect`, and hands back its name; `None` for a kind the
    /// graph does not have.
    ///
    /// Named as [`insert_node`](Self::insert_node) names. The graph may give the node a
    /// size of its own — the editor asks for the rectangle back. Defaulted to `None`, which
    /// is visible rather than silent: a graph that lists kinds and forgets this gets an
    /// `Add` menu that adds nothing and says so (`node.add` refuses, and a warning names
    /// the kind), not nodes of the wrong kind.
    fn insert_kind(&mut self, rect: Rect, kind: &str) -> Option<usize> {
        let _ = (rect, kind);
        None
    }

    /// Puts a node back under the name it had, holding `content`, for undo and redo.
    ///
    /// Separate from [`insert_node`](Self::insert_node) rather than a lucky consequence
    /// of it: a free list hands the last freed name back first, which is the right name
    /// only while nothing else was added in between. Undo has to be right always.
    ///
    /// The content comes from the history rather than from the slot the removal left:
    /// a slot keeps it only until an insertion takes the name back, and one example's
    /// slot did not keep it at all — an undone delete brought its node back with the
    /// slider at its default (§57).
    fn restore_node(&mut self, index: usize, rect: Rect, content: Self::Content);

    /// Removes a node and every link that ended on it, and hands the links back.
    ///
    /// They come back because undo needs them: a delete that returned only the name
    /// would restore a node with no links, and nothing would report it.
    fn remove_node(&mut self, index: usize) -> Vec<Link>;

    /// Adds a link. `false` if the graph refused it — an end that does not exist, or a
    /// link that is already there.
    fn insert_link(&mut self, link: Link) -> bool;

    /// Whether `link` may be made at all: whether its two ports go together.
    ///
    /// Asked **while a link is being dragged**, for the port under the pointer only, so
    /// the refusal shows before the button comes up (`link.drag` draws the curve in
    /// [`OverlayStyle::refused`](crate::OverlayStyle::refused)), and asked again on the
    /// release; a link it refuses is not offered to [`insert_link`](Self::insert_link).
    /// A question to the graph rather than to the canvas's `NodeSource`, because what a
    /// port takes follows from what its node is — the graph's
    /// [`Content`](Self::Content) — and because it is one rule for every view of the
    /// graph, not a matter of how one of them draws it.
    ///
    /// Defaulted, unlike everything here that carries what a node is (§42): "every link
    /// goes" is what a graph without port types means and what the editor did before the
    /// question existed. A typed graph that forgets it loses the warning, not the rule —
    /// [`insert_link`](Self::insert_link) still has the last word, and a link that is
    /// already there is refused there, not here.
    fn accepts_link(&self, link: Link) -> bool {
        let _ = link;
        true
    }

    /// How many links input `port` of node `index` holds at most; `usize::MAX` for no limit.
    ///
    /// One by default, as an ordinary input in Blender (`node_socket_link_limit`): a link
    /// connected to a full input takes off the oldest link there, in the same undo step
    /// (`EditorWorld::connect`). That is what keeps the links of one input from lying on top
    /// of each other where they meet it, where no pick can tell them apart — and what makes
    /// "take one link off" a drag from its input. Outputs have no limit, as in Blender. A
    /// graph whose input gathers several links by meaning answers more; drawing such an input
    /// so that its links do not overlap is `issues/multi-input sockets.md`. Zero refuses
    /// every link into the input.
    fn input_limit(&self, index: usize, port: u16) -> usize {
        let _ = (index, port);
        1
    }

    /// Removes a link, in either direction.
    fn remove_link(&mut self, link: Link);

    /// What a node holds past its rectangle — its type, its values — as the application
    /// keeps it.
    ///
    /// Opaque to the editor, which only carries it: from a node into the history, from
    /// a node into the clipboard, and back into the graph (§57). `()` for a graph whose
    /// nodes hold nothing.
    type Content: Clone + 'static;

    /// What node `index` holds.
    fn content(&self, index: usize) -> Self::Content;

    /// Puts `content` into node `index`, for undo and redo of an edit made in a node's
    /// widget or a panel (§67).
    ///
    /// Required, like everything here that writes what a node is (§42): a default that did
    /// nothing would compile, and `Ctrl+Z` after renaming a node would quietly undo whatever
    /// came before the rename instead — which is what it did before this existed. The
    /// editor records the change for every view itself; the graph only stores.
    fn set_content(&mut self, index: usize, content: Self::Content);

    /// The text form of the graph's content, for a copy that leaves the process — the system
    /// clipboard (§66) — or `None` for content with none: a copy then stays in the process.
    ///
    /// One answer for the three things a text form is — a format name, a writer, a reader —
    /// so that a graph has all of them or none (§70). Three methods with defaults let a graph
    /// write content and forget the format name, which was a debug panic on `Ctrl+C`.
    fn text_form(&self) -> Option<&dyn ContentText<Self::Content>> {
        None
    }

    /// Adds a node at `rect` holding `content`, and hands back the name it got — a copy,
    /// for duplicate and paste.
    ///
    /// Named as [`insert_node`](Self::insert_node) names. Required, like everything here
    /// that carries what a node is: a default answering with a fresh node would compile,
    /// pass every test that counts nodes, and paste empty boxes — the reason
    /// [`views`](Self::views) has no default either (§42).
    fn insert_copy(&mut self, rect: Rect, content: Self::Content) -> usize;

    /// The links that end on node `index`, at either end.
    ///
    /// What a copy of several nodes keeps the links between them by. Called once per
    /// copied node, not per frame.
    fn links_of(&self, index: usize) -> Vec<Link>;
}

/// A graph shared by the editor, its operators and every view of it.
///
/// `Rc<RefCell<_>>` rather than a channel: everything that touches it lives on the UI
/// thread, and a change has to be visible to the next build of a node immediately, not
/// one frame later.
pub type SharedGraph<G> = Rc<RefCell<G>>;

/// How a graph's node content goes to text and back, for the system clipboard (§66, §70).
///
/// The format is the application's, and only it can read it back; the library wraps it with
/// the rectangles and links of the copy (`blazy-nodes 1 <format>`) and escapes it onto one
/// line, so any text will do. A copy is read only by a graph whose format has the same name.
pub trait ContentText<C> {
    /// The format's name: one word, such as `calc`. Two applications that name one format
    /// exchange copies on purpose; a copy in another format is not read — without the name,
    /// `hello`, which takes any text as a title, pasted a `calc` copy as nodes called
    /// `number 2`.
    fn format(&self) -> &str;

    /// `content` as text.
    fn write(&self, content: &C) -> String;

    /// Reads what [`write`](Self::write) wrote; `None` for text that is not one. The text is
    /// input from outside the process: read it as such.
    fn read(&self, text: &str) -> Option<C>;
}

/// A kind of node a graph can add: its name, as a binding and a keymap file say it, and
/// what a menu says for it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct NodeKind {
    /// The name `node.add` is given in `kind` — one word, as a keymap file writes it.
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
