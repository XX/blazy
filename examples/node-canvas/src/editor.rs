//! The node editor, as this example uses it.
//!
//! The widget is `blazy::node_editor::NodeEditor` over this example's graph, and it
//! lived here until §42 moved it into the library. What stays is the one thing that is
//! the example's own: the statistics overlay is always on, because Phase 0 is a
//! measurement, not a demo — numbers that only appear in a log are numbers nobody
//! checks while dragging a node around — and because the tests read it back.

use blazy::canvas::CanvasLayer;
use blazy::node_editor::SessionHandle;

use crate::model::{GraphModel, SharedGraph};

/// The node editor over this example's graph.
pub type NodeEditor = blazy::node_editor::NodeEditor<GraphModel>;

/// The last line of the statistics overlay: what the example's keymap does.
pub const HUD_CAPTION: &str = "left-drag a node or the view - right-click selects, right-drag boxes - \
     G moves, B boxes, Shift+A adds, X deletes, F links, Ctrl+Z undoes";

/// An editor with the canvas's own gestures and the statistics overlay.
pub fn new(canvas: CanvasLayer) -> NodeEditor {
    NodeEditor::new(canvas).with_hud(HUD_CAPTION)
}

/// An editor with the operator layer and the statistics overlay.
pub fn with_ops(canvas: CanvasLayer, graph: &SharedGraph) -> NodeEditor {
    NodeEditor::with_ops(canvas, graph).with_hud(HUD_CAPTION)
}

/// The same, over a session that already exists.
///
/// What an area rebuilt in another window is made of: a new widget showing the state the
/// old one showed (the detach task, decision 1).
pub fn with_session(canvas: CanvasLayer, session: SessionHandle<GraphModel>) -> NodeEditor {
    NodeEditor::with_session(canvas, session).with_hud(HUD_CAPTION)
}
