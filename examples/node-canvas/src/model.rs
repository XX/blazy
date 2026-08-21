//! The graph model: the source of truth for node state.
//!
//! With virtualisation a node's widget exists only while the node is on screen, so
//! state cannot live in the widget. This is not a workaround — it is the right
//! arrangement for an editor anyway, since the graph outlives any view of it and
//! has to be saved, undone and scripted independently of what is visible.

use std::cell::RefCell;
use std::rc::Rc;

use masonry::kurbo::{Point, Size};
use masonry::peniko::Color;

/// Node footprint in canvas units.
pub const NODE_SIZE: Size = Size::new(160.0, 96.0);

/// Spacing between nodes in the generated grid.
const GRID_STEP: f64 = 220.0;
/// Nodes per row in the generated grid.
const GRID_COLS: usize = 80;

/// One node's persistent state.
#[derive(Clone, Copy, Debug)]
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
}

impl GraphModel {
    /// Builds a deterministic grid of nodes.
    ///
    /// Deterministic on purpose: two benchmark runs must be comparable, so there is
    /// no randomness anywhere. The jitter is a cheap hash of the index, not an RNG.
    pub fn generated(count: usize) -> Self {
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
                    pos: Point::new(col as f64 * GRID_STEP + jitter_x, row as f64 * GRID_STEP + jitter_y),
                    tint,
                    value: ((h >> 5) % 100) as f64 / 100.0,
                    checked: h & 1 == 0,
                }
            })
            .collect();
        Self { nodes }
    }

    /// Returns the state of a node.
    pub fn node(&self, index: usize) -> NodeState {
        self.nodes[index]
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
