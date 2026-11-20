//! The node editor's operators, as this example uses them.
//!
//! `blazy::node_editor`'s operators over this example's graph, plus the snapshot form of an
//! undo step, measured against the journal.

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

/// How undo steps are recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UndoMode {
    /// One step holds what the operator touched, and how to put it back.
    #[default]
    Journal,
    /// One step holds the whole model, before and after: here to be measured.
    Snapshot,
}

/// Sets how an editor records its moves, for the measurement.
pub fn set_undo_mode(editor: &mut WidgetMut<'_, NodeEditor>, mode: UndoMode) {
    NodeEditor::set_move_recorder(editor, match mode {
        UndoMode::Journal => blazy::node_editor::journal,
        UndoMode::Snapshot => snapshot,
    });
}

/// The snapshot [`MoveRecorder`](blazy::node_editor::MoveRecorder): the whole model,
/// twice, for a step that moved the selection.
fn snapshot(world: &EditorWorld, record: MoveRecord) -> Box<dyn Step<EditorWorld>> {
    let after = world.graph().borrow().snapshot();
    let mut before = after.clone();
    for (&index, &pos) in record.nodes.iter().zip(&record.from) {
        if let Some(node) = before[index].as_mut() {
            node.pos = pos;
        }
    }
    Box::new(SnapshotStep { before, after })
}

/// The same move, as a snapshot of the whole model; measured against the journal only.
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
        let graph = world.graph().borrow();
        (0..nodes.len().min(graph.names()))
            .filter(|&index| graph.try_node(index).map(|node| node.pos) != nodes[index].map(|node| node.pos))
            .collect()
    };
    world.graph().borrow_mut().restore(nodes);
    world.note_moved(changed);
}

/// A runtime with the node editor's operators registered and its keymap in force.
pub fn runtime() -> OpRuntime<EditorWorld> {
    blazy::node_editor::ops::runtime()
}
