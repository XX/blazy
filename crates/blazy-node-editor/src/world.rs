//! Everything an operator may touch, and the undo steps that change it.

use std::collections::{BTreeMap, BTreeSet};

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
    /// The selection as it was before the first change the driver has not announced yet.
    ///
    /// The selection itself is the graph's ([`Views::selection`](crate::Views::selection)):
    /// this is what lets the driver tell the views which nodes came into it.
    pub(crate) selection_before: Option<BTreeSet<usize>>,
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
    /// A menu an operator asked to open, for the driver to put on screen at the pointer.
    ///
    /// Left here rather than opened by the operator, for the reason everything here is:
    /// an operator never touches a widget, and a menu on screen is one (§38.3).
    pub menu: Option<crate::Menu>,
    /// A link being dragged out of a port: from its output end to its input end, in canvas
    /// coordinates, for the driver to draw over the canvas.
    pub link_preview: Option<(Point, Point)>,
    /// Whether the running operator needs to know what is under the pointer on every move.
    ///
    /// While an operator holds the pointer the canvas below sees no events and publishes no
    /// hover; the driver picks for it instead — and only while this asks, so a node drag,
    /// which needs no hover, does not pay a pick per move.
    pub track_hover: bool,
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
            selection_before: None,
            hover: None,
            pointer: Point::ORIGIN,
            pointer_screen: Point::ORIGIN,
            band: None,
            moved: Vec::new(),
            pan: Vec2::ZERO,
            zoom: Vec::new(),
            menu: None,
            link_preview: None,
            track_hover: false,
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

    /// Adds a node at `rect` holding `content` — a copy — and returns its name.
    pub fn add_copy(&mut self, rect: Rect, content: G::Content) -> usize {
        let index = self.graph.borrow_mut().insert_copy(rect, content);
        self.edits.push(Edit::NodeAdded { index, rect });
        self.dirty = true;
        index
    }

    /// Puts a node back under its own name, holding what it held, with the links it had.
    pub fn restore_node(&mut self, index: usize, rect: Rect, content: G::Content, links: &[Link]) {
        self.graph.borrow_mut().restore_node(index, rect, content);
        self.edits.push(Edit::NodeAdded { index, rect });
        for &link in links {
            self.add_link(link);
        }
        self.dirty = true;
    }

    /// Removes a node, and hands back what it takes to put it back.
    pub fn remove_node(&mut self, index: usize) -> Removed<G::Content> {
        let rect = self.node_rect(index);
        let content = self.graph.borrow().content(index);
        let links = self.graph.borrow_mut().remove_node(index);
        self.edits.push(Edit::NodeRemoved { index });
        let mut selection = self.selection();
        if selection.remove(&index) {
            self.select(selection);
        }
        self.dirty = true;
        Removed {
            index,
            rect,
            content,
            links,
        }
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

    /// The selected nodes, by index: the graph's selection, shared by every view of it.
    ///
    /// A `BTreeSet` so that iteration order is the graph's order rather than a hash
    /// seed's: a test comparing two runs would otherwise compare their orderings.
    pub fn selection(&self) -> BTreeSet<usize> {
        self.graph.borrow().views().selection()
    }

    /// Whether node `index` is selected.
    pub fn is_selected(&self, index: usize) -> bool {
        self.graph.borrow().views().is_selected(index)
    }

    /// Replaces the selection, and says whether it changed.
    ///
    /// The graph's, not this session's: every view of the graph shows it, as in Blender.
    /// The driver tells the views after the operator, with what else it changed.
    pub fn select(&mut self, nodes: BTreeSet<usize>) -> bool {
        let Some(before) = self.graph.borrow().views().replace_selection(nodes) else {
            return false;
        };
        self.selection_before.get_or_insert(before);
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

/// A node that went, and what it takes to bring it back: [`EditorWorld::remove_node`]'s
/// answer.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Removed<C> {
    /// Its name, free now.
    pub index: usize,
    /// Where it was, in canvas coordinates.
    pub rect: Rect,
    /// What it held. Kept by whoever means to put the node back, rather than left in the
    /// graph's slot, which keeps it only until an insertion takes the name back (§57).
    pub content: C,
    /// The links that ended on it.
    pub links: Vec<Link>,
}

impl<C> Removed<C> {
    /// What the history holds for it, by the step's own reckoning: the content's inline
    /// size, not what it owns on the heap — a journal entry is priced at what it touched
    /// (§38.4), and the editor cannot see past an opaque type.
    fn bytes(&self) -> usize {
        size_of::<usize>() + size_of::<Rect>() + size_of::<C>() + self.links.len() * size_of::<Link>()
    }
}

/// A node that appeared: undo takes it away, redo puts it back under its own name.
///
/// The selection travels with it, and that is the point: adding a node selects it, which
/// is one action to the user and therefore one step. Recording the selection change as a
/// step of its own — which is what pushing it through the shared selection path did —
/// makes one `Shift+A` take two undos, and the second one does something the user never
/// asked for.
pub(crate) struct AddStep<C> {
    pub(crate) index: usize,
    pub(crate) rect: Rect,
    /// What the new node held, so redo brings back the same node rather than a fresh one.
    pub(crate) content: C,
    /// What was selected before, so undo puts it back.
    pub(crate) selection: BTreeSet<usize>,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for AddStep<G::Content> {
    fn name(&self) -> &'static str {
        "node.add"
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        world.remove_node(self.index);
        world.select(self.selection.clone());
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        world.restore_node(self.index, self.rect, self.content.clone(), &[]);
        world.select(BTreeSet::from([self.index]));
    }

    fn bytes(&self) -> usize {
        size_of::<usize>() + size_of::<Rect>() + size_of::<G::Content>() + self.selection.len() * size_of::<usize>()
    }
}

/// Nodes that went, with what it takes to bring each of them back.
///
/// The links come with the nodes rather than as steps of their own: deleting a node and
/// deleting its links is one thing the user did, and an undo that returned the node
/// without its links would be a different graph wearing the same names.
pub(crate) struct DeleteStep<C> {
    pub(crate) nodes: Vec<Removed<C>>,
    pub(crate) selection: BTreeSet<usize>,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for DeleteStep<G::Content> {
    fn name(&self) -> &'static str {
        "node.delete"
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        // Every node first, then the links: a link between two deleted nodes is refused
        // while its other end is still gone.
        for node in &self.nodes {
            world.restore_node(node.index, node.rect, node.content.clone(), &[]);
        }
        for node in &self.nodes {
            for &link in &node.links {
                world.add_link(link);
            }
        }
        world.select(self.selection.clone());
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        for node in &self.nodes {
            world.remove_node(node.index);
        }
    }

    fn bytes(&self) -> usize {
        self.nodes.iter().map(Removed::bytes).sum::<usize>() + self.selection.len() * size_of::<usize>()
    }
}

/// Nodes that came in as copies — a duplicate or a paste — with the links between them.
///
/// One step for the lot, as a delete is one step: undo removes every node it added (the
/// links go with them) and puts the selection back; redo brings back the same names,
/// holding the same contents, where the nodes ended up — after the move a duplicate
/// starts, which is part of the same step (§57).
pub(crate) struct CopyStep<C> {
    pub(crate) name: &'static str,
    pub(crate) nodes: Vec<(usize, Rect, C)>,
    pub(crate) links: Vec<Link>,
    /// What was selected before, so undo puts it back.
    pub(crate) selection: BTreeSet<usize>,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for CopyStep<G::Content> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        for &(index, ..) in &self.nodes {
            world.remove_node(index);
        }
        world.select(self.selection.clone());
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        for (index, rect, content) in &self.nodes {
            world.restore_node(*index, *rect, content.clone(), &[]);
        }
        for &link in &self.links {
            world.add_link(link);
        }
        world.select(self.nodes.iter().map(|&(index, ..)| index).collect());
    }

    fn bytes(&self) -> usize {
        self.nodes.len() * (size_of::<usize>() + size_of::<Rect>() + size_of::<G::Content>())
            + self.links.len() * size_of::<Link>()
            + self.selection.len() * size_of::<usize>()
    }
}

/// Nodes copied out of a graph, with the links between them: what `node.copy` leaves on
/// the graph's clipboard and what `node.duplicate` copies without one.
///
/// Links with one end outside the copy are not in it, as in Blender's duplicate and its
/// clipboard: a copy is a subgraph, and a link into the rest of the graph would be a
/// second edge into a node nobody copied.
#[derive(Debug)]
pub(crate) struct Clip<C> {
    /// Each node's rectangle where it was copied from, and what it held.
    pub(crate) nodes: Vec<(Rect, C)>,
    /// The links between them, their ends named by position in `nodes`.
    pub(crate) links: Vec<Link>,
}

impl<C: Clone + 'static> Clip<C> {
    /// Copies `nodes` out of `graph`, in name order.
    pub(crate) fn of<G: NodeGraph<Content = C>>(graph: &G, nodes: &BTreeSet<usize>) -> Self {
        let positions: BTreeMap<usize, usize> = nodes.iter().enumerate().map(|(at, &index)| (index, at)).collect();
        let position = |index: usize| positions.get(&index).copied();
        let mut links = Vec::new();
        for (at, &index) in nodes.iter().enumerate() {
            // Each link is seen from both ends; it is taken from its output end alone.
            for link in graph.links_of(index) {
                if link.from as usize != index {
                    continue;
                }
                if let Some(to) = position(link.to as usize) {
                    links.push(Link::between(at, link.from_port, to, link.to_port));
                }
            }
        }
        Self {
            nodes: nodes
                .iter()
                .map(|&index| (graph.node_rect(index), graph.content(index)))
                .collect(),
            links,
        }
    }

    /// The box around every copied node, where they were copied from.
    pub(crate) fn bounds(&self) -> Rect {
        self.nodes
            .iter()
            .map(|&(rect, _)| rect)
            .reduce(|all, rect| all.union(rect))
            .unwrap_or_default()
    }

    /// Adds the copy to `world`'s graph, every node moved by `delta`, and hands back the
    /// step that undoes it. Selects what it added, and nothing else.
    pub(crate) fn paste_into<G: NodeGraph<Content = C>>(
        &self,
        world: &mut EditorWorld<G>,
        delta: Vec2,
        name: &'static str,
    ) -> CopyStep<C> {
        let selection = world.selection();
        let nodes: Vec<(usize, Rect, C)> = self
            .nodes
            .iter()
            .map(|(rect, content)| {
                let rect = *rect + delta;
                (world.add_copy(rect, content.clone()), rect, content.clone())
            })
            .collect();
        let links = self
            .links
            .iter()
            .map(|link| {
                Link::between(
                    nodes[link.from as usize].0,
                    link.from_port,
                    nodes[link.to as usize].0,
                    link.to_port,
                )
            })
            .filter(|&link| world.add_link(link))
            .collect();
        world.select(nodes.iter().map(|&(index, ..)| index).collect());
        CopyStep {
            name,
            nodes,
            links,
            selection,
        }
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
