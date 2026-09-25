//! The node editor's operators, as this example uses them.
//!
//! The operators, their keymap and the world they act on are
//! `blazy::node_editor`'s since §42; this module names them over this example's graph
//! and keeps the one thing that is the example's own: the snapshot form of an undo
//! step, which exists to be measured against the journal (§38.4) and needs to know
//! what a node is.

use blazy::masonry::core::WidgetMut;
use blazy::node_editor::MoveRecord;
pub use blazy::node_editor::ops::{
    BoxSelectOp, CANVAS_CONTEXT, CANVAS_SCOPE, MoveOp, PanOp, RedoOp, SelectOp, UndoOp, default_keymap,
};
use blazy::ops::runtime::OpRuntime;
use blazy::ops::undo::Step;

use crate::editor::NodeEditor;
use crate::model::{GraphModel, NodeState};

/// Everything the operators may touch, over this example's graph.
pub type EditorWorld = blazy::node_editor::EditorWorld<GraphModel>;

/// How undo steps are recorded — the two shapes §38.4 measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UndoMode {
    /// One step holds what the operator touched, and how to put it back.
    #[default]
    Journal,
    /// One step holds the whole model, before and after.
    ///
    /// Here to be measured, not to be used: it costs the graph per step where the
    /// journal costs the selection. Keeping it in the code is what makes the
    /// comparison reproducible rather than a remembered number.
    Snapshot,
}

/// Sets how an editor records its moves, for the measurement in §38.4.
pub fn set_undo_mode(editor: &mut WidgetMut<'_, NodeEditor>, mode: UndoMode) {
    NodeEditor::set_move_recorder(editor, match mode {
        UndoMode::Journal => blazy::node_editor::journal,
        UndoMode::Snapshot => snapshot,
    });
}

/// The snapshot [`MoveRecorder`](blazy::node_editor::MoveRecorder): the whole model,
/// twice, for a step that moved the selection.
///
/// Called when the move is confirmed, with the graph in its final state — the same
/// moment and the same work as when this lived inside the operator, so §38.4's numbers
/// still measure what they measured.
fn snapshot(world: &EditorWorld, record: MoveRecord) -> Box<dyn Step<EditorWorld>> {
    let after = world.graph.borrow().snapshot();
    let mut before = after.clone();
    for (&index, &pos) in record.nodes.iter().zip(&record.from) {
        if let Some(node) = before[index].as_mut() {
            node.pos = pos;
        }
    }
    Box::new(SnapshotStep { before, after })
}

/// The same move, as a snapshot of the whole model. Measured against the journal in
/// §38.4 and not otherwise used.
struct SnapshotStep {
    before: Vec<Option<NodeState>>,
    after: Vec<Option<NodeState>>,
}

impl Step<EditorWorld> for SnapshotStep {
    fn name(&self) -> &'static str {
        "node.move"
    }

    fn undo(&mut self, world: &mut EditorWorld) {
        restore(world, &self.before);
    }

    fn redo(&mut self, world: &mut EditorWorld) {
        restore(world, &self.after);
    }

    fn bytes(&self) -> usize {
        (self.before.len() + self.after.len()) * size_of::<NodeState>()
    }
}

/// Puts a whole snapshot back, and tells the views about every node that moved.
fn restore(world: &mut EditorWorld, nodes: &[Option<NodeState>]) {
    let changed: Vec<usize> = {
        let graph = world.graph.borrow();
        (0..nodes.len().min(graph.names()))
            .filter(|&index| graph.try_node(index).map(|node| node.pos) != nodes[index].map(|node| node.pos))
            .collect()
    };
    world.graph.borrow_mut().restore(nodes);
    world.moved.extend(changed);
}

/// A runtime with the node editor's operators registered and its keymap in force.
pub fn runtime() -> OpRuntime<EditorWorld> {
    blazy::node_editor::ops::runtime()
}
