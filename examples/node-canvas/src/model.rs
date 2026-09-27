//! The graph model: the source of truth for node state.
//!
//! With virtualisation a node's widget exists only while the node is on screen, so
//! state cannot live in the widget. This is not a workaround — it is the right
//! arrangement for an editor anyway, since the graph outlives any view of it and
//! has to be saved, undone and scripted independently of what is visible.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use blazy::masonry::core::WidgetId;
use blazy::masonry::kurbo::{Point, Rect, Size};
use blazy::masonry::peniko::Color;
use blazy::node_editor::{Link, NodeGraph};

/// Node footprint in canvas units.
pub const NODE_SIZE: Size = Size::new(160.0, 96.0);

/// Spacing between nodes in the generated grid.
pub const GRID_STEP: f64 = 220.0;
/// Nodes per row in the generated grid.
pub const GRID_COLS: usize = 80;

/// One node's persistent state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeState {
    /// Position of the top-left corner, in canvas coordinates.
    pub pos: Point,
    /// Header tint, used to tell nodes apart when zoomed out.
    pub tint: Color,
    /// Value of the node's slider.
    pub value: f64,
    /// State of the node's checkbox.
    pub checked: bool,
}

/// The graph.
#[derive(Debug)]
pub struct GraphModel {
    /// Nodes by name. A hole is a name a removal freed (§43), and nothing renumbers the
    /// nodes that stayed: a selection, a link and an undo step are all written in names.
    ///
    /// `Option<NodeState>` rather than a flag beside the array because `checked` is a
    /// `bool` and lends the option its niche, so a hole costs nothing — which is what
    /// keeps §38.4's snapshot the same size it was.
    nodes: Vec<Option<NodeState>>,
    /// Names removals freed, handed out again before fresh ones.
    free: Vec<usize>,
    /// The edges of the graph, by name, with holes where removals were.
    ///
    /// In the model rather than in a canvas, and that is §43's other half: with two views
    /// of one graph, links held by a view are two copies of the topology, and deleting a
    /// link in one view would never reach the other — the defect §30 found for positions,
    /// waiting to happen again.
    links: Vec<Option<Link>>,
    /// Edge names a removal freed.
    free_links: Vec<usize>,
    /// The edges incident to each node, by name.
    ///
    /// The same shape the canvas needed for the same reason: without it, deleting a node
    /// and refusing a duplicate both walk every edge in the graph, and the milliseconds
    /// follow the graph even though the canvas's own counters do not (§43).
    by_node: HashMap<u32, Vec<usize>>,
    /// The canvases currently showing this graph.
    ///
    /// Strictly this is not model state — a document does not know what looks at it —
    /// and in an application it would live in whatever owns the views. It is here
    /// because everything that changes the graph already holds this handle and needs
    /// the list in the same breath: a change has to reach the other views *in the same
    /// frame*, and the only code able to do that is code holding a widget context, i.e.
    /// the canvas and the node (§30).
    views: Vec<WidgetId>,
    /// What each view has not been told about yet.
    ///
    /// The push fan-out of §30 reaches the canvases of *this* window and stops there:
    /// `mutate_later` names a widget in one `RenderRoot`'s arena, and a canvas in another
    /// window is not in it — the call is silently dropped, which is exactly how a second
    /// window came to show a node where it used to be. So the model also keeps what each
    /// view still owes, and a view pulls it when its window next runs. Applying a change
    /// twice is applying the same truth twice, so the fast path and this one do not have
    /// to know about each other.
    pending: Vec<(WidgetId, Vec<Change>)>,
}

impl GraphModel {
    /// Builds a deterministic grid of nodes.
    ///
    /// Deterministic on purpose: two benchmark runs must be comparable, so there is
    /// no randomness anywhere. The jitter is a cheap hash of the index, not an RNG.
    pub fn generated(count: usize) -> Self {
        Self::generated_with_step(count, GRID_STEP)
    }

    /// As [`generated`](Self::generated), with the grid spacing given.
    ///
    /// Exists for one measurement, and it is a measurement the shape of the whole
    /// level-of-detail policy rests on: what a zoom materialises is decided by how
    /// many nodes fit the viewport, which is spacing and node size, and *not* by how
    /// many nodes the graph has (§29.1). Halving the step is the only way to put four
    /// times as many nodes under the same viewport at the same zoom without changing
    /// anything else.
    pub fn generated_with_step(count: usize, step: f64) -> Self {
        let nodes = (0..count)
            .map(|i| {
                let col = i % GRID_COLS;
                let row = i / GRID_COLS;

                // A reproducible pseudo-random offset, so the grid does not look
                // like graph paper while staying identical between runs.
                let h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let jitter_x = ((h >> 33) % 61) as f64 - 30.0;
                let jitter_y = ((h >> 17) % 41) as f64 - 20.0;

                let tint = match i % 6 {
                    0 => Color::from_rgb8(0x6b, 0x4b, 0x8a),
                    1 => Color::from_rgb8(0x3c, 0x6e, 0x71),
                    2 => Color::from_rgb8(0x8a, 0x5a, 0x3c),
                    3 => Color::from_rgb8(0x44, 0x6b, 0x3c),
                    4 => Color::from_rgb8(0x8a, 0x3c, 0x51),
                    _ => Color::from_rgb8(0x3c, 0x4e, 0x8a),
                };

                Some(NodeState {
                    pos: Point::new(col as f64 * step + jitter_x, row as f64 * step + jitter_y),
                    tint,
                    value: ((h >> 5) % 100) as f64 / 100.0,
                    checked: h & 1 == 0,
                })
            })
            .collect();
        let mut model = Self {
            pending: Vec::new(),
            links: Vec::new(),
            free_links: Vec::new(),
            by_node: HashMap::new(),
            nodes,
            free: Vec::new(),
            views: Vec::new(),
        };
        for link in crate::generated_links(count) {
            model.file_link(link);
        }
        model
    }

    /// Files a link under a name and on both of its ends.
    fn file_link(&mut self, link: Link) -> usize {
        let name = match self.free_links.pop() {
            Some(name) => {
                self.links[name] = Some(link);
                name
            },
            None => {
                self.links.push(Some(link));
                self.links.len() - 1
            },
        };
        for end in [link.from, link.to] {
            self.by_node.entry(end).or_default().push(name);
        }
        name
    }

    /// Takes a link out from under its name and off both of its ends.
    fn unfile_link(&mut self, name: usize) -> Option<Link> {
        let link = self.links.get_mut(name)?.take()?;
        self.free_links.push(name);
        for end in [link.from, link.to] {
            if let Some(list) = self.by_node.get_mut(&end)
                && let Some(at) = list.iter().position(|&other| other == name)
            {
                list.swap_remove(at);
            }
        }
        Some(link)
    }

    /// The name of the link between two nodes, in either direction.
    fn link_named(&self, link: Link) -> Option<usize> {
        self.by_node
            .get(&link.from)?
            .iter()
            .copied()
            .find(|&name| self.links[name].is_some_and(|other| same_link(other, link)))
    }

    /// Returns the state of a node.
    ///
    /// # Panics
    ///
    /// Panics on a name nothing holds. Use [`try_node`](Self::try_node) where the name
    /// may be stale — it comes from a view, and a view is told about a removal a frame
    /// after the model knows.
    pub fn node(&self, index: usize) -> NodeState {
        self.try_node(index).expect("a live node")
    }

    /// The state of a node, or `None` for a name nothing holds.
    pub fn try_node(&self, index: usize) -> Option<NodeState> {
        self.nodes.get(index).copied().flatten()
    }

    /// How many nodes the graph holds. Holes are not nodes.
    pub fn len(&self) -> usize {
        self.nodes.iter().flatten().count()
    }

    /// How many names the graph uses, holes included: what a view has to mirror.
    pub fn names(&self) -> usize {
        self.nodes.len()
    }

    /// The graph's edges, as a view is built over them.
    ///
    /// Collected rather than borrowed: the edges are stored with holes, and a caller
    /// wants the graph rather than its name space. Called once per view, not per frame.
    pub fn links(&self) -> Vec<Link> {
        self.links.iter().flatten().copied().collect()
    }

    /// How many edges the graph holds.
    pub fn link_count(&self) -> usize {
        self.links.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A copy of every node's state.
    ///
    /// Here for two callers and they want opposite things from it: a test comparing
    /// "before undo" with "after redo" needs the whole state to compare, and the
    /// snapshot form of an undo step (§38.4) needs the whole state to hold — which is
    /// exactly why the journal form exists.
    pub fn snapshot(&self) -> Vec<Option<NodeState>> {
        self.nodes.clone()
    }

    /// Puts a snapshot back.
    ///
    /// Nodes beyond the snapshot's length are left alone, so restoring an older,
    /// shorter snapshot cannot silently truncate a graph that has grown.
    pub fn restore(&mut self, nodes: &[Option<NodeState>]) {
        let shared = self.nodes.len().min(nodes.len());
        self.nodes[..shared].copy_from_slice(&nodes[..shared]);
    }

    /// Records a node's new position.
    ///
    /// Position is state like any other, and by §20.2 state lives here rather than in
    /// the view: a canvas keeps its own copy of the geometry, and a second canvas over
    /// the same graph keeps another. Before this existed a drag moved one copy and the
    /// other views kept the old position for good.
    pub fn set_pos(&mut self, index: usize, pos: Point) {
        if let Some(Some(node)) = self.nodes.get_mut(index) {
            node.pos = pos;
            self.note(Change::Moved(index));
        }
    }

    /// Records that a canvas is showing this graph.
    pub fn register_view(&mut self, canvas: WidgetId) {
        if !self.views.contains(&canvas) {
            self.views.push(canvas);
            self.pending.push((canvas, Vec::new()));
        }
    }

    /// Records a change for every view, so a window that was not there when it happened
    /// can catch up.
    fn note(&mut self, change: Change) {
        for (_, owed) in &mut self.pending {
            owed.push(change);
        }
    }

    /// Whether any view is behind.
    ///
    /// What a driver asks before waking other windows: an idle window is idle on purpose
    /// (§36), and waking it on every event would undo that.
    pub fn has_pending(&self) -> bool {
        self.pending.iter().any(|(_, owed)| !owed.is_empty())
    }

    /// Takes what `view` has not caught up with.
    ///
    /// Empty almost always: a view that ran in the same window as the change has already
    /// applied it by the push path, and applying it again would only write the same
    /// truth. What this is for is the view that could not be reached at all.
    pub fn take_pending(&mut self, view: WidgetId) -> Vec<Change> {
        self.pending
            .iter_mut()
            .find(|(id, _)| *id == view)
            .map(|(_, owed)| std::mem::take(owed))
            .unwrap_or_default()
    }

    /// The canvases showing this graph, except `this` one.
    ///
    /// The exclusion is the caller's whole reason for asking: a view that has just
    /// applied a change does not need it applied again, and re-applying it to the
    /// widget the user is currently dragging is how a control loses its grip.
    pub fn other_views(&self, this: Option<WidgetId>, out: &mut Vec<WidgetId>) {
        out.extend(self.views.iter().copied().filter(|&id| Some(id) != this));
    }

    /// Records a slider change.
    pub fn set_value(&mut self, index: usize, value: f64) {
        if let Some(Some(node)) = self.nodes.get_mut(index) {
            node.value = value;
            self.note(Change::Edited(index));
        }
    }

    /// Records a checkbox change.
    pub fn set_checked(&mut self, index: usize, checked: bool) {
        if let Some(Some(node)) = self.nodes.get_mut(index) {
            node.checked = checked;
            self.note(Change::Edited(index));
        }
    }
}

/// What the node editor needs from this graph: the seam of `blazy::node_editor`.
///
/// Every node is [`NODE_SIZE`], so a rectangle is a position and a constant.
impl NodeGraph for GraphModel {
    fn node_count(&self) -> usize {
        self.len()
    }

    fn node_rect(&self, index: usize) -> Rect {
        let pos = self.try_node(index).map_or(Point::ORIGIN, |node| node.pos);
        Rect::from_origin_size(pos, NODE_SIZE)
    }

    fn set_node_pos(&mut self, index: usize, pos: Point) {
        self.set_pos(index, pos);
    }

    fn other_views(&self, this: WidgetId, out: &mut Vec<WidgetId>) {
        GraphModel::other_views(self, Some(this), out);
    }

    /// A new node takes a freed name if there is one, and looks like the node before it:
    /// the tint says nothing about identity and everything about telling nodes apart.
    fn insert_node(&mut self, rect: Rect) -> usize {
        let state = NodeState {
            pos: rect.origin(),
            tint: Color::from_rgb8(0x3c, 0x6e, 0x71),
            value: 0.5,
            checked: false,
        };
        let index = match self.free.pop() {
            Some(index) => {
                self.nodes[index] = Some(state);
                index
            },
            None => {
                self.nodes.push(Some(state));
                self.nodes.len() - 1
            },
        };
        self.note(Change::Added(index));
        index
    }

    fn restore_node(&mut self, index: usize, rect: Rect) {
        if self.nodes.len() <= index {
            self.nodes.resize(index + 1, None);
        }
        if let Some(at) = self.free.iter().position(|&free| free == index) {
            self.free.swap_remove(at);
        }
        let state = self.nodes[index].unwrap_or(NodeState {
            pos: rect.origin(),
            tint: Color::from_rgb8(0x3c, 0x6e, 0x71),
            value: 0.5,
            checked: false,
        });
        self.nodes[index] = Some(NodeState {
            pos: rect.origin(),
            ..state
        });
        self.note(Change::Added(index));
    }

    fn remove_node(&mut self, index: usize) -> Vec<Link> {
        if self.try_node(index).is_none() {
            return Vec::new();
        }
        self.nodes[index] = None;
        self.free.push(index);
        self.note(Change::Removed(index));
        // Its own links, through the adjacency: a scan of every edge in the graph is
        // what this used to be, and what made a delete cost the graph (§43).
        let names = self.by_node.remove(&(index as u32)).unwrap_or_default();
        names.into_iter().filter_map(|name| self.unfile_link(name)).collect()
    }

    fn insert_link(&mut self, link: Link) -> bool {
        let live = |i: u32| self.try_node(i as usize).is_some();
        if link.from == link.to || !live(link.from) || !live(link.to) {
            return false;
        }
        if self.link_named(link).is_some() {
            return false;
        }
        self.file_link(link);
        true
    }

    fn remove_link(&mut self, link: Link) {
        if let Some(name) = self.link_named(link) {
            self.unfile_link(name);
        }
    }
}

/// Two links are the same link whichever way round they are written.
fn same_link(a: Link, b: Link) -> bool {
    (a.from, a.to) == (b.from, b.to) || (a.from, a.to) == (b.to, b.from)
}

/// A change a view has not caught up with yet.
///
/// Everything a node widget copies out of the model when it is built, which is its
/// geometry (§43, because the canvas keeps its own) *and* its contents: a node that is
/// already on screen read the model once and keeps its own `value`, `checked` and tint,
/// so nothing about it follows the model by itself. A node that is not built needs
/// nothing — it reads the model when it next scrolls in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// The node moved, and the view has the old place.
    Moved(usize),
    /// The node appeared.
    Added(usize),
    /// The node went.
    Removed(usize),
    /// What the node holds changed — a slider, a checkbox — and a view showing it has
    /// the old copy.
    ///
    /// Inside one window the edit reaches the other views as a push
    /// (`GraphNode::broadcast`), and that push does not cross a window like any other
    /// (§44.3): `mutate_later` names a widget in one arena. Recorded here so the other
    /// window collects it on its next frame.
    Edited(usize),
}

/// Shared handle to the graph.
///
/// `Rc<RefCell<_>>` rather than a channel: the canvas, the node widgets and the app
/// all live on the UI thread, and a node writing its slider value back to the model
/// must be visible to the next `build` immediately, not one frame later.
pub type SharedGraph = blazy::node_editor::SharedGraph<GraphModel>;

/// Wraps a model in a shared handle.
pub fn share(model: GraphModel) -> SharedGraph {
    Rc::new(RefCell::new(model))
}
