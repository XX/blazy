//! The graph model: the source of truth for node state.
//!
//! With virtualisation a node's widget exists only while the node is on screen, so
//! state cannot live in the widget. This is not a workaround — it is the right
//! arrangement for an editor anyway, since the graph outlives any view of it and
//! has to be saved, undone and scripted independently of what is visible.

use std::cell::RefCell;
use std::rc::Rc;

use masonry::core::WidgetId;
use masonry::kurbo::{Point, Size};
use masonry::peniko::Color;

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
    nodes: Vec<NodeState>,
    /// The canvases currently showing this graph.
    ///
    /// Strictly this is not model state — a document does not know what looks at it —
    /// and in an application it would live in whatever owns the views. It is here
    /// because everything that changes the graph already holds this handle and needs
    /// the list in the same breath: a change has to reach the other views *in the same
    /// frame*, and the only code able to do that is code holding a widget context, i.e.
    /// the canvas and the node (§30).
    views: Vec<WidgetId>,
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

                NodeState {
                    pos: Point::new(col as f64 * step + jitter_x, row as f64 * step + jitter_y),
                    tint,
                    value: ((h >> 5) % 100) as f64 / 100.0,
                    checked: h & 1 == 0,
                }
            })
            .collect();
        Self {
            nodes,
            views: Vec::new(),
        }
    }

    /// Returns the state of a node.
    pub fn node(&self, index: usize) -> NodeState {
        self.nodes[index]
    }

    /// How many nodes the graph holds.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// A copy of every node's state.
    ///
    /// Here for two callers and they want opposite things from it: a test comparing
    /// "before undo" with "after redo" needs the whole state to compare, and the
    /// snapshot form of an undo step (§38.4) needs the whole state to hold — which is
    /// exactly why the journal form exists.
    pub fn snapshot(&self) -> Vec<NodeState> {
        self.nodes.clone()
    }

    /// Puts a snapshot back.
    ///
    /// Nodes beyond the snapshot's length are left alone, so restoring an older,
    /// shorter snapshot cannot silently truncate a graph that has grown.
    pub fn restore(&mut self, nodes: &[NodeState]) {
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
        if let Some(node) = self.nodes.get_mut(index) {
            node.pos = pos;
        }
    }

    /// Records that a canvas is showing this graph.
    pub fn register_view(&mut self, canvas: WidgetId) {
        if !self.views.contains(&canvas) {
            self.views.push(canvas);
        }
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
        if let Some(node) = self.nodes.get_mut(index) {
            node.value = value;
        }
    }

    /// Records a checkbox change.
    pub fn set_checked(&mut self, index: usize, checked: bool) {
        if let Some(node) = self.nodes.get_mut(index) {
            node.checked = checked;
        }
    }
}

/// Shared handle to the graph.
///
/// `Rc<RefCell<_>>` rather than a channel: the canvas, the node widgets and the app
/// all live on the UI thread, and a node writing its slider value back to the model
/// must be visible to the next `build` immediately, not one frame later.
pub type SharedGraph = Rc<RefCell<GraphModel>>;

/// Wraps a model in a shared handle.
pub fn share(model: GraphModel) -> SharedGraph {
    Rc::new(RefCell::new(model))
}
