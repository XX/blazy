//! The graph model: the source of truth for node state.
//!
//! A node's widget exists only while the node is on screen, so its state lives here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use blazy::masonry::kurbo::{Point, Rect, Size};
use blazy::masonry::peniko::Color;
use blazy::node_editor::{Link, NodeGraph, Views};

/// Node footprint in canvas units.
pub const NODE_SIZE: Size = Size::new(160.0, 96.0);

/// Inputs every node has: one for the slider, one for the checkbox.
pub const INPUTS: u16 = 2;
/// Outputs every node has.
pub const OUTPUTS: u16 = 1;

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
    /// Nodes by name; a hole is a name a removal freed (nothing renumbers the rest).
    nodes: Vec<Option<NodeState>>,
    /// Names removals freed, handed out again before fresh ones.
    free: Vec<usize>,
    /// The edges of the graph, by name, with holes where removals were.
    links: Vec<Option<Link>>,
    /// Edge names a removal freed.
    free_links: Vec<usize>,
    /// The edges incident to each node, by name, so an edit does not walk every edge.
    by_node: HashMap<u32, Vec<usize>>,
    /// The canvases currently showing this graph, and what each has not been told yet.
    views: Views,
}

impl GraphModel {
    /// Builds a deterministic grid of nodes (the jitter is a hash of the index), so two
    /// benchmark runs are comparable.
    pub fn generated(count: usize) -> Self {
        Self::generated_with_step(count, GRID_STEP)
    }

    /// As [`generated`](Self::generated), with the grid spacing given: for the density
    /// sweep, since density and not graph size decides what a zoom materialises.
    pub fn generated_with_step(count: usize, step: f64) -> Self {
        let nodes = (0..count)
            .map(|i| {
                let col = i % GRID_COLS;
                let row = i / GRID_COLS;

                // A reproducible pseudo-random offset.
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
            links: Vec::new(),
            free_links: Vec::new(),
            by_node: HashMap::new(),
            nodes,
            free: Vec::new(),
            views: Views::new(),
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
    /// Panics on a name nothing holds; use [`try_node`](Self::try_node) for a name from a view.
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

    /// The graph's edges, without the holes, for building a view.
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

    /// A copy of every node's state (for tests, and the snapshot form of undo).
    pub fn snapshot(&self) -> Vec<Option<NodeState>> {
        self.nodes.clone()
    }

    /// Puts a snapshot back; nodes beyond its length are left alone.
    pub fn restore(&mut self, nodes: &[Option<NodeState>]) {
        let shared = self.nodes.len().min(nodes.len());
        self.nodes[..shared].copy_from_slice(&nodes[..shared]);
    }

    /// Records a node's new position.
    pub fn set_pos(&mut self, index: usize, pos: Point) {
        if let Some(Some(node)) = self.nodes.get_mut(index) {
            // Not recorded for the views: whoever moved the node records it.
            node.pos = pos;
        }
    }

    /// The registry of this graph's views.
    pub fn views(&self) -> &Views {
        &self.views
    }

    /// Records a slider change.
    pub fn set_value(&mut self, index: usize, value: f64) {
        if let Some(Some(node)) = self.nodes.get_mut(index) {
            node.value = value;
        }
    }

    /// Records a checkbox change.
    pub fn set_checked(&mut self, index: usize, checked: bool) {
        if let Some(Some(node)) = self.nodes.get_mut(index) {
            node.checked = checked;
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

    fn names(&self) -> usize {
        GraphModel::names(self)
    }

    fn node_rect(&self, index: usize) -> Rect {
        let pos = self.try_node(index).map_or(Point::ORIGIN, |node| node.pos);
        Rect::from_origin_size(pos, NODE_SIZE)
    }

    fn set_node_pos(&mut self, index: usize, pos: Point) {
        self.set_pos(index, pos);
    }

    /// Every node has the one size; only the corner is written.
    fn set_node_rect(&mut self, index: usize, rect: Rect) {
        debug_assert_eq!(rect.size(), NODE_SIZE, "a node of this graph is not resized");
        self.set_pos(index, rect.origin());
    }

    fn views(&self) -> &Views {
        &self.views
    }

    /// A new node takes a freed name if there is one.
    fn insert_node(&mut self, rect: Rect) -> usize {
        let state = NodeState {
            pos: rect.origin(),
            tint: Color::from_rgb8(0x3c, 0x6e, 0x71),
            value: 0.5,
            checked: false,
        };
        // The editor records the insertion for the views.
        match self.free.pop() {
            Some(index) => {
                self.nodes[index] = Some(state);
                index
            },
            None => {
                self.nodes.push(Some(state));
                self.nodes.len() - 1
            },
        }
    }

    /// Back under its own name, holding what it held when it went.
    fn restore_node(&mut self, index: usize, rect: Rect, content: NodeState) {
        if self.nodes.len() <= index {
            self.nodes.resize(index + 1, None);
        }
        if let Some(at) = self.free.iter().position(|&free| free == index) {
            self.free.swap_remove(at);
        }
        self.nodes[index] = Some(NodeState {
            pos: rect.origin(),
            ..content
        });
    }

    fn remove_node(&mut self, index: usize) -> Vec<Link> {
        if self.try_node(index).is_none() {
            return Vec::new();
        }
        self.nodes[index] = None;
        self.free.push(index);
        // Its own links, through the adjacency.
        let names = self.by_node.remove(&(index as u32)).unwrap_or_default();
        names.into_iter().filter_map(|name| self.unfile_link(name)).collect()
    }

    fn insert_link(&mut self, link: Link) -> bool {
        let live = |i: u32| self.try_node(i as usize).is_some();
        if link.from == link.to || !live(link.from) || !live(link.to) {
            return false;
        }
        // Every node has the same ports: one output, and an input per control.
        if link.from_port >= OUTPUTS || link.to_port >= INPUTS {
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

    /// The whole state; its `pos` is the rectangle's, and a copy put elsewhere takes the
    /// rectangle it is given.
    type Content = NodeState;

    fn content(&self, index: usize) -> NodeState {
        self.node(index)
    }

    /// The node's state put back, where it stands now: its place is the canvas's to say.
    fn set_content(&mut self, index: usize, content: NodeState) {
        if let Some(Some(node)) = self.nodes.get_mut(index) {
            *node = NodeState {
                pos: node.pos,
                ..content
            };
        }
    }

    fn insert_copy(&mut self, rect: Rect, content: NodeState) -> usize {
        let index = self.insert_node(rect);
        self.nodes[index] = Some(NodeState {
            pos: rect.origin(),
            ..content
        });
        index
    }

    fn links_of(&self, index: usize) -> Vec<Link> {
        self.by_node
            .get(&(index as u32))
            .into_iter()
            .flatten()
            .filter_map(|&name| self.links[name])
            .collect()
    }
}

/// Two links are the same link whichever way round they are written.
fn same_link(a: Link, b: Link) -> bool {
    a == b || a == b.reversed()
}

/// Shared handle to the graph.
pub type SharedGraph = blazy::node_editor::SharedGraph<GraphModel>;

/// Wraps a model in a shared handle.
pub fn share(model: GraphModel) -> SharedGraph {
    Rc::new(RefCell::new(model))
}
