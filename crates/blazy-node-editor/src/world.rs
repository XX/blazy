//! Everything an operator may touch, and the undo steps that change it.

use std::collections::BTreeSet;

use blazy_canvas::CanvasHit;
use blazy_ops::undo::Step;
use masonry::kurbo::{Point, Rect, Size, Vec2};

use crate::{Link, NodeGraph, SharedGraph};

/// What a new node is sized at until an application says otherwise.
///
/// Neither small enough to be a dot nor large enough to fill a viewport; the number is
/// a default, not a measurement.
pub const DEFAULT_NODE_SIZE: Size = Size::new(160.0, 96.0);

/// A change to the shape of the graph, on its way from the model into the views.
///
/// The counterpart of [`EditorWorld::moved`] for structure rather than geometry: an
/// operator writes the model and leaves this behind, and the driver carries it into its
/// own canvas and into the graph's other views (§30). Nothing here is a widget, and that
/// is the rule the whole crate is built on (§38.3).
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Edit {
    /// A node appeared, with the name the model gave it.
    NodeAdded {
        /// The node's name.
        index: usize,
        /// Where it is, in canvas coordinates.
        rect: Rect,
    },
    /// A node went, taking its links with it.
    NodeRemoved {
        /// The name that is now free.
        index: usize,
    },
    /// A link appeared.
    LinkAdded(Link),
    /// A link went.
    LinkRemoved(Link),
}

/// Everything the operators are allowed to touch.
///
/// Deliberately not a widget in sight: the model, the selection, where the pointer is
/// and what it is over. A driver fills the last two in before it dispatches, and reads
/// [`moved`](Self::moved) afterwards.
pub struct EditorWorld<G: NodeGraph> {
    /// The graph. The source of truth, shared with every view of it (§30).
    pub graph: SharedGraph<G>,
    /// Selected nodes, by index.
    ///
    /// A `BTreeSet` so that iteration order is the graph's order rather than a hash
    /// seed's: a test comparing two runs would otherwise compare their orderings.
    pub selection: BTreeSet<usize>,
    /// What the pointer is over, as the canvas last picked it.
    ///
    /// The context an operator polls against, and the reason a press picks as well as
    /// a move (§38.3): a driver holding an `EventCtx` cannot hit-test a child, so the
    /// canvas publishes what it found and the driver reads it.
    pub hover: Option<CanvasHit>,
    /// Pointer position, in canvas coordinates.
    pub pointer: Point,
    /// Pointer position in the driver's own coordinates.
    ///
    /// Both, because they are not interchangeable during a pan: the view moves under
    /// the pointer, so an operator that panned in canvas coordinates would chase its
    /// own tail.
    pub pointer_screen: Point,
    /// The rubber band, in canvas coordinates, while a box select is running.
    pub band: Option<Rect>,
    /// Nodes whose position changed and whose views have not been told yet.
    ///
    /// Drained by the driver. Duplicates are allowed and expected — a drag pushes the
    /// same index every frame — because deduplicating costs more than moving a child
    /// twice would.
    pub moved: Vec<usize>,
    /// View movement the driver has not applied yet, in screen units.
    ///
    /// The view is not model state and not the operator's to touch, so it leaves here
    /// the same way a moved node does — as something for the driver to carry in.
    pub pan: Vec2,
    /// View zooms the driver has not applied yet: the point to zoom about, in the
    /// driver's own (screen) units, and the factor.
    ///
    /// The zoom twin of [`pan`](Self::pan), and kept as a list rather than a product
    /// because two zooms about two points are not one zoom about either.
    pub zoom: Vec<(Point, f64)>,
    /// Set when something changed that only affects pixels.
    pub dirty: bool,
    /// Changes to the shape of the graph the views have not been told about yet.
    ///
    /// Drained by the driver, exactly like [`moved`](Self::moved). Structure is not
    /// geometry: a canvas chooses what it draws for a *region* and re-chooses it when
    /// the view leaves that region, so an edit nobody announces is an edit no view ever
    /// notices (§28.4, §43).
    pub edits: Vec<Edit>,
    /// The size a new node is given, unless an operator is told another one.
    ///
    /// The canvas holds each node's rectangle and the model decides what a node *is*;
    /// between the two there is nobody to ask how big a node the user just asked for
    /// should be, so it is a setting rather than a guess.
    pub new_node_size: Size,
    /// How a finished move becomes an undo step. [`journal`] unless replaced.
    ///
    /// A seam rather than a fixed choice because §38.4 measured the alternative — a
    /// snapshot of the whole graph per step — against the journal, and the measurement
    /// has to stay reproducible. The snapshot needs to know what a node *is*, which
    /// only the application does, so the application supplies it.
    pub record_move: MoveRecorder<G>,
}

impl<G: NodeGraph> EditorWorld<G> {
    /// A world over `graph`, with nothing selected and moves journalled.
    pub fn new(graph: &SharedGraph<G>) -> Self {
        Self {
            graph: graph.clone(),
            selection: BTreeSet::new(),
            hover: None,
            pointer: Point::ORIGIN,
            pointer_screen: Point::ORIGIN,
            band: None,
            moved: Vec::new(),
            pan: Vec2::ZERO,
            zoom: Vec::new(),
            dirty: false,
            edits: Vec::new(),
            new_node_size: DEFAULT_NODE_SIZE,
            record_move: journal::<G>,
        }
    }

    /// The node under the pointer, if the pointer is over one.
    pub fn hovered_node(&self) -> Option<usize> {
        self.hover.and_then(CanvasHit::node)
    }

    /// Nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.graph.borrow().node_count()
    }

    /// A node's rectangle, in canvas coordinates.
    pub fn node_rect(&self, index: usize) -> Rect {
        self.graph.borrow().node_rect(index)
    }

    /// Writes a node's position to the model and records that views must follow.
    pub fn set_pos(&mut self, index: usize, pos: Point) {
        self.graph.borrow_mut().set_node_pos(index, pos);
        self.moved.push(index);
    }

    /// Adds a node at `rect` and returns the name the model gave it.
    pub fn add_node(&mut self, rect: Rect) -> usize {
        let index = self.graph.borrow_mut().insert_node(rect);
        self.edits.push(Edit::NodeAdded { index, rect });
        self.dirty = true;
        index
    }

    /// Puts a node back under its own name, with the links it had.
    pub fn restore_node(&mut self, index: usize, rect: Rect, links: &[Link]) {
        self.graph.borrow_mut().restore_node(index, rect);
        self.edits.push(Edit::NodeAdded { index, rect });
        for &link in links {
            if self.graph.borrow_mut().insert_link(link) {
                self.edits.push(Edit::LinkAdded(link));
            }
        }
        self.dirty = true;
    }

    /// Removes a node, and hands back what it takes to put it back.
    pub fn remove_node(&mut self, index: usize) -> (Rect, Vec<Link>) {
        let rect = self.node_rect(index);
        let links = self.graph.borrow_mut().remove_node(index);
        self.edits.push(Edit::NodeRemoved { index });
        self.selection.remove(&index);
        self.dirty = true;
        (rect, links)
    }

    /// Adds a link, unless the graph refuses it.
    pub fn add_link(&mut self, link: Link) -> bool {
        if !self.graph.borrow_mut().insert_link(link) {
            return false;
        }
        self.edits.push(Edit::LinkAdded(link));
        self.dirty = true;
        true
    }

    /// Removes a link.
    pub fn remove_link(&mut self, link: Link) {
        self.graph.borrow_mut().remove_link(link);
        self.edits.push(Edit::LinkRemoved(link));
        self.dirty = true;
    }

    /// Replaces the selection, and says whether it changed.
    pub fn select(&mut self, nodes: BTreeSet<usize>) -> bool {
        if self.selection == nodes {
            return false;
        }
        self.selection = nodes;
        self.dirty = true;
        true
    }

    /// The nodes whose rectangles meet `rect`.
    ///
    /// A scan of the model rather than the canvas's spatial index, and on purpose: the
    /// index is a *view's* answer to "what is on screen", and a box select must find
    /// nodes the view has not materialised — including, at an overview zoom, nodes
    /// that have no widget at all (§25.3). Twenty thousand rectangle tests happen once
    /// per gesture, not once per frame: the band is drawn on every move and resolved
    /// on release.
    pub fn nodes_in(&self, rect: Rect) -> BTreeSet<usize> {
        let graph = self.graph.borrow();
        (0..graph.node_count())
            .filter(|&index| {
                // Not `Rect::area`, which is the product of two lengths and comes out
                // positive when the boxes miss in both axes.
                let overlap = graph.node_rect(index).intersect(rect);
                overlap.width() > 0.0 && overlap.height() > 0.0
            })
            .collect()
    }
}

/// A finished move, as it goes to the history: which nodes, from where, to where.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct MoveRecord {
    /// The nodes that moved, by index.
    pub nodes: Vec<usize>,
    /// Where each of them started, in the same order.
    pub from: Vec<Point>,
    /// Where each of them ended, in the same order.
    pub to: Vec<Point>,
}

/// Turns a finished move into an undo step. See [`EditorWorld::record_move`].
///
/// Called once per move, when it is confirmed, with the graph already in its final
/// state — so a recorder that wants the whole graph can read it from the world.
pub type MoveRecorder<G> = fn(&EditorWorld<G>, MoveRecord) -> Box<dyn Step<EditorWorld<G>>>;

/// The default [`MoveRecorder`]: a journal entry of what moved (§38.4).
///
/// Costs what was touched — 40 bytes for a one-node drag — where a snapshot costs the
/// graph.
pub fn journal<G: NodeGraph>(_world: &EditorWorld<G>, record: MoveRecord) -> Box<dyn Step<EditorWorld<G>>> {
    Box::new(MoveStep(record))
}

/// A move, as a journal entry.
struct MoveStep(MoveRecord);

impl<G: NodeGraph> Step<EditorWorld<G>> for MoveStep {
    fn name(&self) -> &'static str {
        "node.move"
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        for (&index, &pos) in self.0.nodes.iter().zip(&self.0.from) {
            world.set_pos(index, pos);
        }
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        for (&index, &pos) in self.0.nodes.iter().zip(&self.0.to) {
            world.set_pos(index, pos);
        }
    }

    fn bytes(&self) -> usize {
        self.0.nodes.len() * (size_of::<usize>() + 2 * size_of::<Point>())
    }
}

/// A selection change.
pub(crate) struct SelectStep {
    pub(crate) before: BTreeSet<usize>,
    pub(crate) after: BTreeSet<usize>,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for SelectStep {
    fn name(&self) -> &'static str {
        "node.select"
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        world.select(self.before.clone());
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        world.select(self.after.clone());
    }

    fn bytes(&self) -> usize {
        (self.before.len() + self.after.len()) * size_of::<usize>()
    }
}

/// A node that appeared: undo takes it away, redo puts it back under its own name.
///
/// The selection travels with it, and that is the point: adding a node selects it, which
/// is one action to the user and therefore one step. Recording the selection change as a
/// step of its own — which is what pushing it through the shared selection path did —
/// makes one `Shift+A` take two undos, and the second one does something the user never
/// asked for.
pub(crate) struct AddStep {
    pub(crate) index: usize,
    pub(crate) rect: Rect,
    /// What was selected before, so undo puts it back.
    pub(crate) selection: BTreeSet<usize>,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for AddStep {
    fn name(&self) -> &'static str {
        "node.add"
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        world.remove_node(self.index);
        world.select(self.selection.clone());
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        world.restore_node(self.index, self.rect, &[]);
        world.select(BTreeSet::from([self.index]));
    }

    fn bytes(&self) -> usize {
        size_of::<usize>() + size_of::<Rect>() + self.selection.len() * size_of::<usize>()
    }
}

/// Nodes that went, with what it takes to bring each of them back.
///
/// The links come with the nodes rather than as steps of their own: deleting a node and
/// deleting its links is one thing the user did, and an undo that returned the node
/// without its links would be a different graph wearing the same names.
pub(crate) struct DeleteStep {
    pub(crate) nodes: Vec<(usize, Rect, Vec<Link>)>,
    pub(crate) selection: BTreeSet<usize>,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for DeleteStep {
    fn name(&self) -> &'static str {
        "node.delete"
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        for (index, rect, links) in &self.nodes {
            world.restore_node(*index, *rect, links);
        }
        world.select(self.selection.clone());
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        for (index, ..) in &self.nodes {
            world.remove_node(*index);
        }
    }

    fn bytes(&self) -> usize {
        self.nodes
            .iter()
            .map(|(_, _, links)| size_of::<usize>() + size_of::<Rect>() + links.len() * size_of::<Link>())
            .sum::<usize>()
            + self.selection.len() * size_of::<usize>()
    }
}

/// One link, added or removed.
pub(crate) struct LinkStep {
    pub(crate) link: Link,
    pub(crate) added: bool,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for LinkStep {
    fn name(&self) -> &'static str {
        if self.added { "link.add" } else { "link.delete" }
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        if self.added {
            world.remove_link(self.link);
        } else {
            world.add_link(self.link);
        }
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        if self.added {
            world.add_link(self.link);
        } else {
            world.remove_link(self.link);
        }
    }

    fn bytes(&self) -> usize {
        size_of::<Link>()
    }
}
