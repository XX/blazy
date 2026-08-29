//! Selection, box-select and grab, as operators over the graph.
//!
//! What this module is for is the shape rather than the gestures: every one of them
//! could be forty lines inside a widget's `on_pointer_event`, and that is exactly what
//! `rnd/architecture.md` §11 calls "scattering `is_dragging: bool` over the widgets".
//! Here a gesture is an [`Operator`]: it is named, it says whether it can run, it can
//! be started by a key, by a button or by a script, and while it is modal it gets the
//! events instead of whatever is under the pointer.
//!
//! **Nothing here touches a widget.** An operator changes [`GraphModel`](crate::model::GraphModel) — the truth,
//! by §30 — and records in [`EditorWorld::moved`] which nodes a view has to follow.
//! The driver ([`crate::editor::NodeEditor`]) carries that into the canvas afterwards.
//! That separation is what makes [`Operator::exec`] worth having: the same operators
//! run in a test with no widget tree at all, so "the key and the script do the same
//! thing" is checked by comparing model state rather than pixels.
//!
//! The keymap is Blender's classic one, because it exercises both ways a modal
//! operator can start and the difference between them is the finding of §38.1: a
//! press-started operator takes Masonry's pointer capture and the tree never sees
//! another event, while a key-started one (`G`, `B`) cannot — capture is only offered
//! during a press — and every event reaches the tree first.

use std::collections::BTreeSet;

use blazy_canvas::CanvasHit;
use blazy_ops::event::{OpEvent, Pattern};
use blazy_ops::keymap::{Binding, Keymap, Props};
use blazy_ops::runtime::{OpCtx, OpRuntime};
use blazy_ops::undo::Step;
use blazy_ops::{OpResult, Operator};
use masonry::core::keyboard::{Key, Modifiers, NamedKey};
use masonry::kurbo::{Point, Rect, Vec2};
use masonry::ui_events::pointer::PointerButton;

use crate::model::{NODE_SIZE, NodeState, SharedGraph};

/// The keymap context a node canvas's bindings live in.
pub const CANVAS_CONTEXT: &str = "canvas";

/// The context chain of an event over the canvas, innermost first.
///
/// Two levels rather than §11's four because this example has no areas and no
/// regions; `area-screen` is where the middle of the chain becomes real. The shape is
/// the point: the canvas shadows the window, and the window still catches undo.
pub const CANVAS_SCOPE: [&str; 2] = [CANVAS_CONTEXT, "window"];

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

/// Everything the operators are allowed to touch.
///
/// Deliberately not a widget in sight: the model, the selection, where the pointer is
/// and what it is over. A driver fills the last two in before it dispatches, and reads
/// [`moved`](Self::moved) afterwards.
pub struct EditorWorld {
    /// The graph. The source of truth, shared with every view of it (§30).
    pub graph: SharedGraph,
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
    /// Set when something changed that only affects pixels.
    pub dirty: bool,
    /// Which shape undo steps take.
    pub undo_mode: UndoMode,
}

impl EditorWorld {
    pub fn new(graph: &SharedGraph) -> Self {
        Self {
            graph: graph.clone(),
            selection: BTreeSet::new(),
            hover: None,
            pointer: Point::ORIGIN,
            pointer_screen: Point::ORIGIN,
            band: None,
            moved: Vec::new(),
            pan: Vec2::ZERO,
            dirty: false,
            undo_mode: UndoMode::default(),
        }
    }

    /// The node under the pointer, if the pointer is over one.
    pub fn hovered_node(&self) -> Option<usize> {
        self.hover.and_then(CanvasHit::node)
    }

    /// Nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.graph.borrow().len()
    }

    /// A node's rectangle, in canvas coordinates.
    pub fn node_rect(&self, index: usize) -> Rect {
        Rect::from_origin_size(self.graph.borrow().node(index).pos, NODE_SIZE)
    }

    /// Writes a node's position to the model and records that views must follow.
    pub fn set_pos(&mut self, index: usize, pos: Point) {
        self.graph.borrow_mut().set_pos(index, pos);
        self.moved.push(index);
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
        (0..graph.len())
            .filter(|&index| {
                // Not `Rect::area`, which is the product of two lengths and comes out
                // positive when the boxes miss in both axes.
                let overlap = Rect::from_origin_size(graph.node(index).pos, NODE_SIZE).intersect(rect);
                overlap.width() > 0.0 && overlap.height() > 0.0
            })
            .collect()
    }
}

/// A move, as a journal entry: which nodes, from where, to where.
struct MoveStep {
    nodes: Vec<usize>,
    from: Vec<Point>,
    to: Vec<Point>,
}

impl Step<EditorWorld> for MoveStep {
    fn name(&self) -> &'static str {
        "node.move"
    }

    fn undo(&mut self, world: &mut EditorWorld) {
        for (&index, &pos) in self.nodes.iter().zip(&self.from) {
            world.set_pos(index, pos);
        }
    }

    fn redo(&mut self, world: &mut EditorWorld) {
        for (&index, &pos) in self.nodes.iter().zip(&self.to) {
            world.set_pos(index, pos);
        }
    }

    fn bytes(&self) -> usize {
        self.nodes.len() * (size_of::<usize>() + 2 * size_of::<Point>())
    }
}

/// The same move, as a snapshot of the whole model. Measured against the journal in
/// §38.4 and not otherwise used.
struct SnapshotStep {
    before: Vec<NodeState>,
    after: Vec<NodeState>,
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
fn restore(world: &mut EditorWorld, nodes: &[NodeState]) {
    let changed: Vec<usize> = {
        let graph = world.graph.borrow();
        (0..nodes.len().min(graph.len()))
            .filter(|&index| graph.node(index).pos != nodes[index].pos)
            .collect()
    };
    world.graph.borrow_mut().restore(nodes);
    world.moved.extend(changed);
}

/// A selection change.
struct SelectStep {
    before: BTreeSet<usize>,
    after: BTreeSet<usize>,
}

impl Step<EditorWorld> for SelectStep {
    fn name(&self) -> &'static str {
        "node.select"
    }

    fn undo(&mut self, world: &mut EditorWorld) {
        world.select(self.before.clone());
    }

    fn redo(&mut self, world: &mut EditorWorld) {
        world.select(self.after.clone());
    }

    fn bytes(&self) -> usize {
        (self.before.len() + self.after.len()) * size_of::<usize>()
    }
}

/// Selects the node under the pointer.
#[derive(Default)]
pub struct SelectOp;

impl Operator<EditorWorld> for SelectOp {
    fn name(&self) -> &'static str {
        "node.select"
    }

    /// Only over a node — or over one named by a property.
    ///
    /// The other binding on the same button, the box select, is what a press on empty
    /// canvas reaches, and it reaches it *because* this refuses. The second half of
    /// the condition is what keeps the scripted path honest: a poll that asked about
    /// the pointer alone would refuse every call that does not come from one, and
    /// "the key and the script do the same thing" would be true only of the key.
    fn poll(&self, cx: &OpCtx<'_, EditorWorld>) -> bool {
        cx.world().hovered_node().is_some() || cx.props().int("index", -1) >= 0
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        let Some(index) = cx.world().hovered_node() else {
            return OpResult::PassThrough;
        };
        select(cx, index)
    }

    /// From a script: the node comes from a property instead of from the pointer.
    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        let index = cx.props().int("index", -1);
        if index < 0 {
            return OpResult::Cancelled;
        }
        select(cx, index as usize)
    }
}

fn select(cx: &mut OpCtx<'_, EditorWorld>, index: usize) -> OpResult {
    let extend = cx.props().bool("extend", false);
    let mut after = if extend {
        cx.world().selection.clone()
    } else {
        BTreeSet::new()
    };
    if extend {
        // Shift-click toggles, as it does everywhere: the second one takes it out.
        if !after.insert(index) {
            after.remove(&index);
        }
    } else {
        after.insert(index);
    }
    set_selection(cx, after);
    OpResult::Finished
}

/// Replaces the selection with `after`, and records the step if it changed.
///
/// Shared by everything that selects, because "the selection changed" and "the change
/// is undoable" have to stay one decision: an operator that set the selection without
/// the step would leave a hole in the history that undo walks straight past.
fn set_selection(cx: &mut OpCtx<'_, EditorWorld>, after: BTreeSet<usize>) {
    let before = cx.world().selection.clone();
    if cx.world_mut().select(after.clone()) {
        cx.push_undo(Box::new(SelectStep { before, after }));
    }
}

/// Drags a rubber band and selects what it covers.
///
/// Two ways in, and they are the two §38.1 is about. A press starts it tracking
/// immediately and the driver takes pointer capture, so nothing else sees another
/// event. `B` starts it *waiting* for a press, with no capture to be had — every event
/// of that gesture reaches the widget tree before the runtime, and is counted.
#[derive(Default)]
pub struct BoxSelectOp {
    anchor: Point,
    tracking: bool,
    /// The button holding the band, if a button started it.
    ///
    /// Whichever one it was: the keymap binds this to two of them, and an operator
    /// that ended on a fixed button would hang on the other. A gesture that cannot end
    /// is the failure `a_gesture_leaves_nothing_running` exists for, and this is how it
    /// was found.
    button: Option<PointerButton>,
}

impl Operator<EditorWorld> for BoxSelectOp {
    fn name(&self) -> &'static str {
        "node.box_select"
    }

    /// Only where there is no node: a press on a node selects it instead.
    fn poll(&self, cx: &OpCtx<'_, EditorWorld>) -> bool {
        cx.world().hovered_node().is_none()
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        self.anchor = cx.world().pointer;
        self.button = match cx.event() {
            Some(OpEvent::Press { button, .. }) => Some(*button),
            _ => None,
        };
        self.tracking = self.button.is_some();
        if self.tracking {
            cx.world_mut().band = Some(Rect::from_points(self.anchor, self.anchor));
            cx.world_mut().dirty = true;
        }
        OpResult::Running
    }

    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        let rect = Rect::new(
            cx.props().float("x0", 0.0),
            cx.props().float("y0", 0.0),
            cx.props().float("x1", 0.0),
            cx.props().float("y1", 0.0),
        );
        finish_box(cx, rect);
        OpResult::Finished
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        let event = cx.event().cloned();
        match event {
            // Started from `B`: the first press is where the band begins.
            Some(OpEvent::Press { button, .. }) if !self.tracking => {
                self.anchor = cx.world().pointer;
                self.tracking = true;
                self.button = Some(button);
                cx.world_mut().band = Some(Rect::from_points(self.anchor, self.anchor));
                cx.world_mut().dirty = true;
                OpResult::Running
            },
            Some(OpEvent::Move { .. }) if self.tracking => {
                let corner = cx.world().pointer;
                cx.world_mut().band = Some(Rect::from_points(self.anchor, corner));
                cx.world_mut().dirty = true;
                OpResult::Running
            },
            // The button that started it, coming up.
            Some(OpEvent::Release { button, .. }) if self.tracking && self.button == Some(button) => {
                let rect = cx.world().band.unwrap_or_default();
                cx.world_mut().band = None;
                finish_box(cx, rect);
                OpResult::Finished
            },
            // Escape, or the other button: both mean "not this".
            Some(OpEvent::Key {
                key: Key::Named(NamedKey::Escape),
                down: true,
                ..
            }) => {
                cx.world_mut().band = None;
                cx.world_mut().dirty = true;
                OpResult::Cancelled
            },
            Some(OpEvent::Press { button, .. }) if self.button != Some(button) => {
                cx.world_mut().band = None;
                cx.world_mut().dirty = true;
                OpResult::Cancelled
            },
            // A move before the press, or a key nobody claimed: the gesture is still
            // ours, and the event is nobody's.
            _ => OpResult::Running,
        }
    }
}

fn finish_box(cx: &mut OpCtx<'_, EditorWorld>, rect: Rect) {
    let extend = cx.props().bool("extend", false);
    let mut after = cx.world().nodes_in(rect);
    if extend {
        after.extend(cx.world().selection.iter().copied());
    }
    set_selection(cx, after);
}

/// Moves the selection with the pointer — Blender's grab.
///
/// Two ways in, and they end differently because that is what each one means. Dragging
/// with the primary button confirms when the button comes up; `G` confirms on the next
/// press, because there is no button held to release. The difference is one field, and
/// it is the difference between a gesture that feels like dragging and one that feels
/// like a mode.
#[derive(Default)]
pub struct MoveOp {
    anchor: Point,
    nodes: Vec<usize>,
    from: Vec<Point>,
    /// Whether the button that started this is still down.
    from_press: bool,
}

impl Operator<EditorWorld> for MoveOp {
    fn name(&self) -> &'static str {
        "node.move"
    }

    /// Something to move, and the binding decides what counts.
    ///
    /// The poll a criterion is written on, and it is what tells the two bindings on the
    /// primary button apart: over a node this takes the press, over empty canvas it
    /// refuses and the pan gets its turn. That is what `under_pointer` is for — without
    /// it a press on empty canvas would start moving the *selection* the moment there
    /// was one, and the view would stop panning as soon as the user selected anything.
    /// `G` carries no such property, because a grab from the keyboard is precisely the
    /// one that moves what is selected wherever the pointer happens to be — and with
    /// nothing selected it must refuse rather than start and find itself idle.
    fn poll(&self, cx: &OpCtx<'_, EditorWorld>) -> bool {
        if cx.props().bool("under_pointer", false) {
            return cx.world().hovered_node().is_some();
        }
        cx.world().hovered_node().is_some() || !cx.world().selection.is_empty()
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        self.from_press = matches!(cx.event(), Some(OpEvent::Press { .. }));
        // Dragging an unselected node takes it, as every editor does: the alternative
        // is a drag that silently moves something else.
        if let Some(index) = cx.world().hovered_node()
            && self.from_press
            && !cx.world().selection.contains(&index)
        {
            select(cx, index);
        }
        self.anchor = cx.world().pointer;
        self.grab(cx);
        OpResult::Running
    }

    /// From a script: the whole move at once, from properties.
    ///
    /// The same end state as the interactive drag that finished at the same delta,
    /// and that equality is a test rather than an intention.
    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        self.grab(cx);
        let delta = Vec2::new(cx.props().float("dx", 0.0), cx.props().float("dy", 0.0));
        self.apply(cx, delta);
        self.commit(cx);
        OpResult::Finished
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        let event = cx.event().cloned();
        match event {
            Some(OpEvent::Move { .. }) => {
                let delta = cx.world().pointer - self.anchor;
                self.apply(cx, delta);
                OpResult::Running
            },
            // The button that started it coming up, for a drag; the next press, for a
            // grab started from the keyboard.
            Some(OpEvent::Release {
                button: PointerButton::Primary,
                ..
            }) if self.from_press => {
                self.commit(cx);
                OpResult::Finished
            },
            Some(OpEvent::Press {
                button: PointerButton::Primary,
                ..
            }) if !self.from_press => {
                self.commit(cx);
                OpResult::Finished
            },
            Some(OpEvent::Key {
                key: Key::Named(NamedKey::Enter),
                down: true,
                ..
            }) => {
                self.commit(cx);
                OpResult::Finished
            },
            Some(OpEvent::Key {
                key: Key::Named(NamedKey::Escape),
                down: true,
                ..
            })
            | Some(OpEvent::Press {
                button: PointerButton::Secondary,
                ..
            }) => {
                // Cancelling is the operator's own job: the runtime keeps no snapshot
                // on anyone's behalf, and the operator is the only thing that knows
                // what "before" was.
                self.apply(cx, Vec2::ZERO);
                OpResult::Cancelled
            },
            _ => OpResult::Running,
        }
    }
}

impl MoveOp {
    /// Records what is being moved and where it started.
    fn grab(&mut self, cx: &mut OpCtx<'_, EditorWorld>) {
        self.nodes = cx.world().selection.iter().copied().collect();
        self.from = self
            .nodes
            .iter()
            .map(|&index| cx.world().node_rect(index).origin())
            .collect();
    }

    /// Puts every grabbed node at its start plus `delta`.
    ///
    /// From the start rather than from the last position, so a drag never accumulates
    /// rounding and a cancel is `apply(ZERO)`.
    fn apply(&mut self, cx: &mut OpCtx<'_, EditorWorld>, delta: Vec2) {
        for (&index, &start) in self.nodes.iter().zip(&self.from) {
            cx.world_mut().set_pos(index, start + delta);
        }
    }

    /// Writes the step to the history.
    fn commit(&mut self, cx: &mut OpCtx<'_, EditorWorld>) {
        let to: Vec<Point> = self
            .nodes
            .iter()
            .map(|&index| cx.world().node_rect(index).origin())
            .collect();
        if to == self.from {
            return;
        }
        match cx.world().undo_mode {
            UndoMode::Journal => cx.push_undo(Box::new(MoveStep {
                nodes: std::mem::take(&mut self.nodes),
                from: std::mem::take(&mut self.from),
                to,
            })),
            UndoMode::Snapshot => {
                // The whole model, twice, for a step that moved the selection. The
                // measurement in §38.4 is what this exists for.
                let after = cx.world().graph.borrow().snapshot();
                let mut before = after.clone();
                for (&index, &pos) in self.nodes.iter().zip(&self.from) {
                    before[index].pos = pos;
                }
                cx.push_undo(Box::new(SnapshotStep { before, after }));
            },
        }
    }
}

/// How far the pointer may travel and still count as a click, in screen pixels.
///
/// Three, which is what every editor uses and what a hand does on a mouse: a press and
/// release at "the same place" is a couple of pixels apart, and a threshold of zero
/// would turn half the clicks into one-pixel drags.
const CLICK_SLOP: f64 = 3.0;

/// Moves the view with the pointer.
///
/// An operator that changes no model state at all, which is why it is here: the view is
/// the widget's, and an operator may not touch a widget (§38.3). So it leaves the
/// movement in [`EditorWorld::pan`] and the driver carries it into the canvas, exactly
/// as it carries a moved node. In screen units, because the view moves under the
/// pointer while this runs.
///
/// **It also decides that a press which never moved was a click**, and with
/// `click_deselects` clears the selection. That belongs here and not in the keymap, and
/// the reason is §38.1: pointer capture is granted during the press and at no other
/// time, so an operator that may need to hold the pointer has to start on the press —
/// before anyone can know whether the gesture will turn out to be a drag. Whoever holds
/// the gesture is therefore the only one who can say, at the end, what it was.
/// [`MoveOp`] does the same thing from the other side: a press on a node that never
/// moved leaves the node selected and nothing in the history.
#[derive(Default)]
pub struct PanOp {
    anchor: Point,
    total: Vec2,
}

impl Operator<EditorWorld> for PanOp {
    fn name(&self) -> &'static str {
        "view.pan"
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        self.anchor = cx.world().pointer_screen;
        self.total = Vec2::ZERO;
        OpResult::Running
    }

    /// From a script: the whole movement at once.
    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        let delta = Vec2::new(cx.props().float("dx", 0.0), cx.props().float("dy", 0.0));
        cx.world_mut().pan += delta;
        OpResult::Finished
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        let event = cx.event().cloned();
        match event {
            Some(OpEvent::Move { .. }) => {
                let pos = cx.world().pointer_screen;
                let delta = pos - self.anchor;
                self.anchor = pos;
                self.total += delta;
                cx.world_mut().pan += delta;
                OpResult::Running
            },
            Some(OpEvent::Release {
                button: PointerButton::Primary,
                ..
            }) => {
                // A press that went nowhere was a click on empty canvas, and a click on
                // empty canvas means "nothing".
                if cx.props().bool("click_deselects", false) && self.total.hypot() < CLICK_SLOP {
                    set_selection(cx, BTreeSet::new());
                }
                OpResult::Finished
            },
            Some(OpEvent::Key {
                key: Key::Named(NamedKey::Escape),
                down: true,
                ..
            })
            | Some(OpEvent::Press {
                button: PointerButton::Secondary,
                ..
            }) => {
                // Putting the view back is this operator's own job: the runtime keeps
                // no snapshot on anyone's behalf.
                cx.world_mut().pan -= self.total;
                OpResult::Cancelled
            },
            _ => OpResult::Running,
        }
    }
}

/// Undo, as an operator, so the keymap reaches it the way it reaches everything else.
#[derive(Default)]
pub struct UndoOp;

impl Operator<EditorWorld> for UndoOp {
    fn name(&self) -> &'static str {
        "ed.undo"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld>) -> bool {
        cx.history().depth() > 0
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        cx.undo();
        cx.world_mut().dirty = true;
        OpResult::Finished
    }
}

/// Redo.
#[derive(Default)]
pub struct RedoOp;

impl Operator<EditorWorld> for RedoOp {
    fn name(&self) -> &'static str {
        "ed.redo"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld>) -> bool {
        cx.history().redo_depth() > 0
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld>) -> OpResult {
        cx.redo();
        cx.world_mut().dirty = true;
        OpResult::Finished
    }
}

/// The keymap the example opens with — Blender's classic one, as data.
///
/// The left button drags: the node under the pointer if there is one, the view if
/// there is not — and a left press on empty canvas that never moved clears the
/// selection, which is the `click_deselects` property on that binding. The right button
/// selects: the node under the pointer if there is one, a rubber band if there is not. None of that is written as a
/// branch — **each button carries two bindings and their operators' polls tell them apart**, which is the
/// mechanism §11 describes and the whole reason `poll` exists.
///
/// `G` and `B` start the same two operators from the keyboard. They are kept because
/// they are Blender's, and because a modal operator started by a key cannot take
/// pointer capture: the difference between the two kinds of start is the finding of
/// §38.1, and an example with only one kind would hide it.
pub fn default_keymap() -> Keymap {
    Keymap::new()
        .with(CANVAS_CONTEXT, vec![
            // Left: drag the node under the pointer, or drag the view.
            Binding::new(Pattern::press(PointerButton::Primary), "node.move")
                .with_props(Props::new().with_bool("under_pointer", true)),
            Binding::new(Pattern::press(PointerButton::Primary), "view.pan")
                .with_props(Props::new().with_bool("click_deselects", true)),
            // Shift-left: add to the selection, by node or by band.
            Binding::new(
                Pattern::press(PointerButton::Primary).with_mods(Modifiers::SHIFT),
                "node.select",
            )
            .with_props(Props::new().with_bool("extend", true)),
            Binding::new(
                Pattern::press(PointerButton::Primary).with_mods(Modifiers::SHIFT),
                "node.box_select",
            )
            .with_props(Props::new().with_bool("extend", true)),
            // Right: select the node under the pointer, or drag a band. A band that
            // ends where it began selects nothing, which is how a click on empty
            // canvas clears the selection without an operator of its own.
            Binding::new(Pattern::press(PointerButton::Secondary), "node.select"),
            Binding::new(Pattern::press(PointerButton::Secondary), "node.box_select"),
            Binding::new(
                Pattern::press(PointerButton::Secondary).with_mods(Modifiers::SHIFT),
                "node.select",
            )
            .with_props(Props::new().with_bool("extend", true)),
            // The keyboard half.
            Binding::new(Pattern::key("b"), "node.box_select"),
            Binding::new(Pattern::key("g"), "node.move"),
        ])
        .with("window", vec![
            Binding::new(Pattern::key("z").with_mods(Modifiers::CONTROL), "ed.undo"),
            Binding::new(
                Pattern::key("z").with_mods(Modifiers::CONTROL | Modifiers::SHIFT),
                "ed.redo",
            ),
        ])
}

/// A runtime with the example's operators registered and the default keymap in force.
pub fn runtime() -> OpRuntime<EditorWorld> {
    let mut runtime = OpRuntime::new(default_keymap());
    runtime.register(SelectOp);
    runtime.register(BoxSelectOp::default());
    runtime.register(MoveOp::default());
    runtime.register(PanOp::default());
    runtime.register(UndoOp);
    runtime.register(RedoOp);
    runtime
}
