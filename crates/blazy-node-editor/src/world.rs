//! Everything an operator may touch, and the undo steps that change it.

use std::collections::{BTreeMap, BTreeSet};

use blazy_canvas::{CanvasHit, PortLayout, PortSide};
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
/// The counterpart of the moved nodes ([`EditorWorld::take_moved`]) for structure rather than geometry: an
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
    /// What a node holds changed — an edit undone or redone (§67). Every view builds the node
    /// again from the model.
    ContentChanged {
        /// The node's name.
        index: usize,
    },
    /// A link appeared.
    LinkAdded(Link),
    /// A link went.
    LinkRemoved(Link),
    /// A node was given a new rectangle — resized, and moved with it when the resize was
    /// from its left edge (§60). Structure rather than a move because a move carries only the
    /// corner, and every view has to hear the size.
    NodeResized {
        /// The node's name.
        index: usize,
        /// Its rectangle now, in canvas coordinates.
        rect: Rect,
    },
}

/// What a link drag shows: one curve per link being dragged, and whether the port under
/// the pointer would take them.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct LinkPreview {
    /// Each curve from its output end to its input end, in canvas coordinates. One for a
    /// new link or a link taken off an input; one per link for the links of an output.
    pub curves: Vec<(Point, Point)>,
    /// The port under the pointer the links would be connected to, by node, side and
    /// number; `None` while the pointer is on no port they could go to.
    pub target: Option<(usize, PortSide, u16)>,
    /// The port a new link is being drawn out of; `None` when the drag carries links taken
    /// off a port, whose fixed ends are only where those links already were.
    pub source: Option<(usize, PortSide, u16)>,
    /// The graph refuses the links at the port under the pointer
    /// ([`NodeGraph::accepts_link`]): letting go there makes none of them.
    pub refused: bool,
}

/// Everything the operators are allowed to touch.
///
/// Deliberately not a widget in sight: the model, the selection, where the pointer is
/// and what it is over. A driver fills the last two in before it dispatches, and reads
/// the moved nodes ([`take_moved`](Self::take_moved)) afterwards.
pub struct EditorWorld<G: NodeGraph> {
    /// The graph. The source of truth, shared with every view of it (§30).
    pub(crate) graph: SharedGraph<G>,
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
    pub(crate) hover: Option<CanvasHit>,
    /// Pointer position, in canvas coordinates.
    pub(crate) pointer: Point,
    /// Pointer position in the driver's own coordinates.
    ///
    /// Both, because they are not interchangeable during a pan: the view moves under
    /// the pointer, so an operator that panned in canvas coordinates would chase its
    /// own tail.
    pub(crate) pointer_screen: Point,
    /// Screen pixels per canvas unit in this view, as of the last event.
    ///
    /// For an operator whose target is a strip a few *screen* pixels wide — the edge a node
    /// is resized by (§60) — since canvas units span a factor of 400 across the zoom range
    /// (§25.2). Filled in by the driver with the pointer; one until then.
    pub(crate) scale: f64,
    /// The rubber band, in canvas coordinates, while a box select is running.
    pub(crate) band: Option<Rect>,
    /// Nodes whose position changed and whose views have not been told yet.
    ///
    /// Drained by the driver. Duplicates are allowed and expected — a drag pushes the
    /// same index every frame — because deduplicating costs more than moving a child
    /// twice would.
    pub(crate) moved: Vec<usize>,
    /// View movement the driver has not applied yet, in screen units.
    ///
    /// The view is not model state and not the operator's to touch, so it leaves here
    /// the same way a moved node does — as something for the driver to carry in.
    pub(crate) pan: Vec2,
    /// View zooms the driver has not applied yet: the point to zoom about, in the
    /// driver's own (screen) units, and the factor.
    ///
    /// The zoom twin of [`pan`](Self::pan), and kept as a list rather than a product
    /// because two zooms about two points are not one zoom about either.
    pub(crate) zoom: Vec<(Point, f64)>,
    /// A menu an operator asked to open, for the driver to put on screen at the pointer.
    ///
    /// Left here rather than opened by the operator, for the reason everything here is:
    /// an operator never touches a widget, and a menu on screen is one (§38.3).
    pub(crate) menu: Option<crate::Menu>,
    /// The links being dragged, for the driver to draw over the canvas.
    pub(crate) link_preview: Option<LinkPreview>,
    /// Where the canvas puts its ports, for an operator that has to know where a port is
    /// without the pointer on it — the far end of a link taken off an input.
    ///
    /// The canvas's rule (§52), copied in by the editor when it is built over a canvas
    /// (`NodeEditor::with_session`); the default puts every port at the middle of its edge,
    /// as the canvas's own default does.
    pub(crate) ports: PortLayout,
    /// Whether the running operator needs to know what is under the pointer on every move.
    ///
    /// While an operator holds the pointer the canvas below sees no events and publishes no
    /// hover; the driver picks for it instead — and only while this asks, so a node drag,
    /// which needs no hover, does not pay a pick per move.
    pub(crate) track_hover: bool,
    /// Set when something changed that only affects pixels.
    pub(crate) dirty: bool,
    /// Changes to the shape of the graph the views have not been told about yet.
    ///
    /// Drained by the driver, exactly like the moved nodes. Structure is not
    /// geometry: a canvas chooses what it draws for a *region* and re-chooses it when
    /// the view leaves that region, so an edit nobody announces is an edit no view ever
    /// notices (§28.4, §43).
    pub(crate) edits: Vec<Edit>,
    /// The size a new node is given, unless an operator is told another one.
    ///
    /// The canvas holds each node's rectangle and the model decides what a node *is*;
    /// between the two there is nobody to ask how big a node the user just asked for
    /// should be, so it is a setting rather than a guess.
    pub(crate) new_node_size: Size,
    /// How a finished move becomes an undo step. [`journal`] unless replaced.
    ///
    /// A seam rather than a fixed choice because §38.4 measured the alternative — a
    /// snapshot of the whole graph per step — against the journal, and the measurement
    /// has to stay reproducible. The snapshot needs to know what a node *is*, which
    /// only the application does, so the application supplies it.
    pub(crate) record_move: MoveRecorder<G>,
}

impl<G: NodeGraph> EditorWorld<G> {
    /// The graph the operators act on (§68).
    pub fn graph(&self) -> &SharedGraph<G> {
        &self.graph
    }

    /// What is under the pointer, as the canvas last picked it.
    pub fn hover(&self) -> Option<CanvasHit> {
        self.hover
    }

    /// Where the pointer is, in canvas coordinates.
    pub fn pointer(&self) -> Point {
        self.pointer
    }

    /// Where the pointer is, in the editor's own coordinates — screen pixels.
    pub fn pointer_screen(&self) -> Point {
        self.pointer_screen
    }

    /// Screen pixels per canvas unit, as of the last event.
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Where the canvas puts ports.
    pub fn ports(&self) -> PortLayout {
        self.ports
    }

    /// The rubber band of a box select, while one runs.
    pub fn band(&self) -> Option<Rect> {
        self.band
    }

    /// The links a link drag is drawing, while one runs.
    pub fn link_preview(&self) -> Option<&LinkPreview> {
        self.link_preview.as_ref()
    }

    /// How big a node `node.add` makes when the binding does not say.
    pub fn new_node_size(&self) -> Size {
        self.new_node_size
    }

    /// Sets how big a node `node.add` makes.
    pub fn set_new_node_size(&mut self, size: Size) {
        self.new_node_size = size;
    }

    /// Sets what is under the pointer and where it is — what the editor does before each
    /// event, for a driver of its own (a test, a benchmark, a host seat) that picks for
    /// itself.
    pub fn set_pointer(&mut self, hover: Option<CanvasHit>, pos: Point) {
        self.hover = hover;
        self.pointer = pos;
    }

    /// Records that nodes moved past the operators' own setters — a step that put a whole
    /// snapshot back — so that the views follow.
    pub fn note_moved(&mut self, indices: impl IntoIterator<Item = usize>) {
        self.moved.extend(indices);
        self.dirty = true;
    }

    /// The nodes moved since the last time, taken — for a driver of its own that carries
    /// the moves into its canvas, as the editor does after each event.
    pub fn take_moved(&mut self) -> Vec<usize> {
        self.dirty = false;
        std::mem::take(&mut self.moved)
    }

    /// A world over `graph`, with nothing selected and moves journalled.
    pub fn new(graph: &SharedGraph<G>) -> Self {
        Self {
            graph: graph.clone(),
            selection_before: None,
            hover: None,
            pointer: Point::ORIGIN,
            pointer_screen: Point::ORIGIN,
            scale: 1.0,
            band: None,
            moved: Vec::new(),
            pan: Vec2::ZERO,
            zoom: Vec::new(),
            menu: None,
            link_preview: None,
            ports: PortLayout::default(),
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

    /// Writes what node `index` holds to the model and records that views must follow (§67).
    pub fn set_content(&mut self, index: usize, content: G::Content) {
        self.graph.borrow_mut().set_content(index, content);
        self.edits.push(Edit::ContentChanged { index });
        self.dirty = true;
    }

    /// Writes a node's rectangle to the model and records that views must follow.
    pub fn set_rect(&mut self, index: usize, rect: Rect) {
        self.graph.borrow_mut().set_node_rect(index, rect);
        self.edits.push(Edit::NodeResized { index, rect });
        self.dirty = true;
    }

    /// Writes a node's position to the model and records that views must follow.
    pub fn set_pos(&mut self, index: usize, pos: Point) {
        self.graph.borrow_mut().set_node_pos(index, pos);
        self.moved.push(index);
    }

    /// Adds a node at `rect` and returns the name the model gave it.
    pub fn add_node(&mut self, rect: Rect) -> usize {
        let index = self.graph.borrow_mut().insert_node(rect);
        self.added(index);
        index
    }

    /// Adds a node of kind `kind` at `rect` and returns its name; `None` for a kind the
    /// graph does not have (§64).
    pub fn add_kind(&mut self, rect: Rect, kind: &str) -> Option<usize> {
        let index = self.graph.borrow_mut().insert_kind(rect, kind)?;
        self.added(index);
        Some(index)
    }

    /// Adds a node at `rect` holding `content` — a copy — and returns its name.
    pub fn add_copy(&mut self, rect: Rect, content: G::Content) -> usize {
        let index = self.graph.borrow_mut().insert_copy(rect, content);
        self.added(index);
        index
    }

    /// Records a node the model just added, at the rectangle the *model* gave it.
    ///
    /// Not the one asked for: a graph may size a node by what it is (`calc` does), and the
    /// views told the asked-for rectangle showed the node at another size than the model
    /// held — every view, until the node was moved (§64).
    fn added(&mut self, index: usize) {
        let rect = self.graph.borrow().node_rect(index);
        self.edits.push(Edit::NodeAdded { index, rect });
        self.dirty = true;
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

    /// Connects `link`, first taking off what its input can no longer hold — the oldest links
    /// there, as many as [`NodeGraph::input_limit`] leaves no room for. Hands back the links
    /// taken off, or `None` if the graph refused the link, in which case nothing changed.
    ///
    /// What `link.drag` and `link.add` both connect through, so a link dropped on a full
    /// input replaces what was there whichever way it came, and the caller records the links
    /// taken off in the same step as the one put on.
    pub fn connect(&mut self, link: Link) -> Option<Vec<Link>> {
        let (limit, mut held) = {
            let graph = self.graph.borrow();
            let held: Vec<Link> = graph
                .links_of(link.to as usize)
                .into_iter()
                .filter(|other| other.to == link.to && other.to_port == link.to_port && *other != link)
                .collect();
            (graph.input_limit(link.to as usize, link.to_port), held)
        };
        if limit == 0 {
            return None;
        }
        // Oldest first: the graph lists them in the order they came.
        held.truncate((held.len() + 1).saturating_sub(limit));
        for &old in &held {
            self.remove_link(old);
        }
        if self.add_link(link) {
            return Some(held);
        }
        for &old in &held {
            self.add_link(old);
        }
        None
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

/// Turns a finished move into an undo step. See
/// [`NodeEditor::set_move_recorder`](crate::NodeEditor::set_move_recorder).
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

/// The first words of a copy as text, and the version this build reads and writes; the
/// graph's content format follows on the same line.
const CLIP_FORMAT: &str = "blazy-nodes 1";
/// The most nodes a copy as text is read with: a clipboard is input from another process.
const CLIP_MAX_NODES: usize = 10_000;

impl<C: Clone + 'static> Clip<C> {
    /// The copy as text, for the system clipboard: `None` if any node's content has no
    /// text form (§66).
    ///
    /// ```text
    /// blazy-nodes 1 <content format>
    /// node <x> <y> <width> <height> <content, escaped>
    /// link <from> <from port> <to> <to port>
    /// ```
    ///
    /// Nodes by position, as the copy names them; links by those positions. The content is
    /// the graph's own text with `\` and line breaks escaped, so it stays on its line.
    pub(crate) fn to_text<G: NodeGraph<Content = C>>(&self, graph: &G) -> Option<String> {
        let Some(format) = graph
            .content_format()
            .filter(|f| !f.is_empty() && !f.contains(char::is_whitespace))
        else {
            let written = self
                .nodes
                .first()
                .is_some_and(|(_, content)| graph.content_text(content).is_some());
            debug_assert!(
                !written,
                "the graph writes content text but names no format (one word): §66"
            );
            if written {
                tracing::warn!("the graph writes content text but names no format; the copy stays in the process");
            }
            return None;
        };
        let mut out = format!("{CLIP_FORMAT} {format}\n");
        for (rect, content) in &self.nodes {
            let text = graph.content_text(content)?;
            let text = text.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r");
            out.push_str(&format!(
                "node {} {} {} {} {text}\n",
                rect.x0,
                rect.y0,
                rect.width(),
                rect.height()
            ));
        }
        for link in &self.links {
            out.push_str(&format!(
                "link {} {} {} {}\n",
                link.from, link.from_port, link.to, link.to_port
            ));
        }
        Some(out)
    }

    /// Reads what [`to_text`](Self::to_text) wrote; `None` for anything else.
    ///
    /// Read as input: at most [`CLIP_MAX_NODES`] nodes, finite rectangles with a size, links
    /// between nodes the text holds, and contents the graph reads back.
    pub(crate) fn from_text<G: NodeGraph<Content = C>>(graph: &G, text: &str) -> Option<Self> {
        let mut lines = text.lines();
        // A copy is read only in the format it was written in: the library's version, and the
        // application's content.
        let format = graph.content_format()?;
        if lines.next()?.trim_end().strip_prefix(CLIP_FORMAT)?.strip_prefix(' ')? != format {
            return None;
        }
        let (mut nodes, mut links) = (Vec::new(), Vec::new());
        for line in lines.filter(|line| !line.trim().is_empty()) {
            if let Some(rest) = line.strip_prefix("node ") {
                let mut words = rest.splitn(5, ' ');
                let mut number = || words.next()?.parse::<f64>().ok().filter(|v| v.is_finite());
                let (x, y, width, height) = (number()?, number()?, number()?, number()?);
                if width <= 0.0 || height <= 0.0 || nodes.len() >= CLIP_MAX_NODES {
                    return None;
                }
                let content = graph.content_from_text(&unescape(words.next().unwrap_or(""))?)?;
                nodes.push((Rect::from_origin_size((x, y), (width, height)), content));
            } else {
                let rest = line.strip_prefix("link ")?;
                let mut words = rest.split(' ').map(|word| word.parse::<u32>().ok());
                let (from, from_port, to, to_port) = (words.next()??, words.next()??, words.next()??, words.next()??);
                if words.next().is_some() || from as usize >= nodes.len() || to as usize >= nodes.len() {
                    return None;
                }
                links.push(Link::between(
                    from as usize,
                    u16::try_from(from_port).ok()?,
                    to as usize,
                    u16::try_from(to_port).ok()?,
                ));
            }
        }
        // A link may name a node the text gives later only if it is written later, and the
        // writer writes every node first; a reader that allowed otherwise would check twice.
        Some(Self { nodes, links })
    }

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

/// An edit of what a node holds, made in its widget or a panel: before and after (§67).
pub(crate) struct ContentStep<C> {
    pub(crate) index: usize,
    pub(crate) before: C,
    pub(crate) after: C,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for ContentStep<G::Content> {
    fn name(&self) -> &'static str {
        "node.edit"
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        world.set_content(self.index, self.before.clone());
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        world.set_content(self.index, self.after.clone());
    }

    fn bytes(&self) -> usize {
        size_of::<usize>() + 2 * size_of::<G::Content>()
    }
}

/// Nodes given new rectangles in one gesture: each by its name, before and after.
pub(crate) struct ResizeStep {
    pub(crate) nodes: Vec<(usize, Rect, Rect)>,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for ResizeStep {
    fn name(&self) -> &'static str {
        "node.resize"
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        for &(index, from, _) in &self.nodes {
            world.set_rect(index, from);
        }
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        for &(index, _, to) in &self.nodes {
            world.set_rect(index, to);
        }
    }

    fn bytes(&self) -> usize {
        self.nodes.len() * (size_of::<usize>() + 2 * size_of::<Rect>())
    }
}

/// Undoes the escaping [`Clip::to_text`] does; `None` for an escape it never writes.
fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(out)
}

/// Links taken off and put on in one gesture: a link dragged off an input onto another
/// port, or onto nothing, or the links of an output carried to another output.
///
/// One step rather than a delete and an add, because the user did one thing; undo takes
/// the new links off and puts the old ones back. The two lists share no link: one dropped
/// back where it came from is taken out of both by the operator.
pub(crate) struct RelinkStep {
    pub(crate) removed: Vec<Link>,
    pub(crate) added: Vec<Link>,
}

impl<G: NodeGraph> Step<EditorWorld<G>> for RelinkStep {
    fn name(&self) -> &'static str {
        "link.drag"
    }

    fn undo(&mut self, world: &mut EditorWorld<G>) {
        for &link in &self.added {
            world.remove_link(link);
        }
        for &link in &self.removed {
            world.add_link(link);
        }
    }

    fn redo(&mut self, world: &mut EditorWorld<G>) {
        for &link in &self.removed {
            world.remove_link(link);
        }
        for &link in &self.added {
            world.add_link(link);
        }
    }

    fn bytes(&self) -> usize {
        (self.removed.len() + self.added.len()) * size_of::<Link>()
    }
}
