//! Selection, box-select, grab and pan, as operators over the graph.
//!
//! What this module is for is the shape rather than the gestures: every one of them
//! could be forty lines inside a widget's `on_pointer_event`, and that is exactly what
//! `rnd/architecture.md` §11 calls "scattering `is_dragging: bool` over the widgets".
//! Here a gesture is an [`Operator`]: it is named, it says whether it can run, it can
//! be started by a key, by a button or by a script, and while it is modal it gets the
//! events instead of whatever is under the pointer.
//!
//! **Nothing here touches a widget.** An operator changes the graph — the truth, by
//! §30 — and records in [`EditorWorld::moved`] which nodes a view has to follow. The
//! driver ([`NodeEditor`](crate::NodeEditor)) carries that into the canvas afterwards.
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

use blazy_canvas::{CanvasHit, PortSide};
use blazy_ops::event::{OpEvent, Pattern};
use blazy_ops::keymap::{Binding, Keymap, Props};
use blazy_ops::runtime::{OpCtx, OpRuntime};
use blazy_ops::{OpResult, Operator};
use masonry::core::keyboard::{Key, Modifiers, NamedKey};
use masonry::kurbo::{Point, Rect, Size, Vec2};
use masonry::ui_events::pointer::PointerButton;

use crate::world::{AddStep, DeleteStep, LinkStep, SelectStep};
use crate::{EditorWorld, Link, MoveRecord, NodeGraph};

/// The keymap context a node canvas's bindings live in.
pub const CANVAS_CONTEXT: &str = "canvas";

/// The context chain of an event over the canvas, innermost first.
///
/// Two levels rather than §11's four: a node editor knows it is inside a window and
/// nothing about the areas and regions an application may put around it. The shape is
/// the point: the canvas shadows the window, and the window still catches undo.
pub const CANVAS_SCOPE: [&str; 2] = [CANVAS_CONTEXT, "window"];

/// Selects the node under the pointer.
#[derive(Default)]
pub struct SelectOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for SelectOp {
    fn name(&self) -> &'static str {
        "node.select"
    }

    /// Only over a node — or over one named by a property, or asked to clear.
    ///
    /// The other binding on the same button, the box select, is what a drag on empty
    /// canvas reaches, and it reaches it *because* this refuses. The `index` half keeps
    /// the scripted path honest: a poll that asked about the pointer alone would refuse
    /// every call that does not come from one, and "the key and the script do the same
    /// thing" would be true only of the key. The `deselect_all` half is Blender's
    /// property of the same name — a click on nothing means "nothing selected", and it
    /// is the operator's business what a click means, not the keymap's (§39.1).
    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        cx.world().hovered_node().is_some()
            || cx.props().int("index", -1) >= 0
            || cx.props().bool("deselect_all", false)
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let Some(index) = cx.world().hovered_node() else {
            if cx.props().bool("deselect_all", false) {
                set_selection(cx, BTreeSet::new());
                return OpResult::Finished;
            }
            return OpResult::PassThrough;
        };
        select(cx, index)
    }

    /// From a script: the node comes from a property instead of from the pointer.
    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let index = cx.props().int("index", -1);
        if index < 0 {
            return OpResult::Cancelled;
        }
        select(cx, index as usize)
    }
}

fn select<G: NodeGraph>(cx: &mut OpCtx<'_, EditorWorld<G>>, index: usize) -> OpResult {
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
fn set_selection<G: NodeGraph>(cx: &mut OpCtx<'_, EditorWorld<G>>, after: BTreeSet<usize>) {
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

impl<G: NodeGraph> Operator<EditorWorld<G>> for BoxSelectOp {
    fn name(&self) -> &'static str {
        "node.box_select"
    }

    /// Only where there is no node: a press on a node selects it instead.
    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        cx.world().hovered_node().is_none()
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        self.anchor = match cx.event() {
            Some(OpEvent::Drag { pos, .. }) => *pos,
            _ => cx.world().pointer,
        };
        self.button = match cx.event() {
            Some(OpEvent::Drag { button, .. } | OpEvent::Press { button, .. }) => Some(*button),
            _ => None,
        };
        self.tracking = self.button.is_some();
        if self.tracking {
            cx.world_mut().band = Some(Rect::from_points(self.anchor, self.anchor));
            cx.world_mut().dirty = true;
        }
        OpResult::Running
    }

    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let rect = Rect::new(
            cx.props().float("x0", 0.0),
            cx.props().float("y0", 0.0),
            cx.props().float("x1", 0.0),
            cx.props().float("y1", 0.0),
        );
        finish_box(cx, rect);
        OpResult::Finished
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
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

fn finish_box<G: NodeGraph>(cx: &mut OpCtx<'_, EditorWorld<G>>, rect: Rect) {
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

impl<G: NodeGraph> Operator<EditorWorld<G>> for MoveOp {
    fn name(&self) -> &'static str {
        "node.move"
    }

    /// Something to move, and **the event says what** — no property required.
    ///
    /// This is what `under_pointer` used to buy, and why it is gone (§39.1). A pointer
    /// gesture names its target by pointing at it: a drag that started on empty canvas
    /// is not a request to move the selection, or the view would stop panning the moment
    /// the user selected anything. A grab from `G`, or from a script, has no pointer to
    /// mean anything by, and moves what is selected. The keymap says neither; the shape
    /// of the event does.
    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        match cx.event() {
            Some(OpEvent::Drag { .. } | OpEvent::Click { .. } | OpEvent::Press { .. }) => {
                cx.world().hovered_node().is_some()
            },
            _ => cx.world().hovered_node().is_some() || !cx.world().selection.is_empty(),
        }
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        self.from_press = matches!(cx.event(), Some(OpEvent::Drag { .. } | OpEvent::Press { .. }));
        // Dragging an unselected node takes it, as every editor does: the alternative
        // is a drag that silently moves something else.
        if let Some(index) = cx.world().hovered_node()
            && self.from_press
            && !cx.world().selection.contains(&index)
        {
            select(cx, index);
        }
        self.anchor = match cx.event() {
            Some(OpEvent::Drag { pos, .. }) => *pos,
            _ => cx.world().pointer,
        };
        self.grab(cx);
        OpResult::Running
    }

    /// From a script: the whole move at once, from properties.
    ///
    /// The same end state as the interactive drag that finished at the same delta,
    /// and that equality is a test rather than an intention.
    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        self.grab(cx);
        let delta = Vec2::new(cx.props().float("dx", 0.0), cx.props().float("dy", 0.0));
        self.apply(cx, delta);
        self.commit(cx);
        OpResult::Finished
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
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
    fn grab<G: NodeGraph>(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) {
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
    fn apply<G: NodeGraph>(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>, delta: Vec2) {
        for (&index, &start) in self.nodes.iter().zip(&self.from) {
            cx.world_mut().set_pos(index, start + delta);
        }
    }

    /// Writes the step to the history.
    fn commit<G: NodeGraph>(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) {
        let to: Vec<Point> = self
            .nodes
            .iter()
            .map(|&index| cx.world().node_rect(index).origin())
            .collect();
        if to == self.from {
            return;
        }
        let record = MoveRecord {
            nodes: std::mem::take(&mut self.nodes),
            from: std::mem::take(&mut self.from),
            to,
        };
        let step = (cx.world().record_move)(cx.world(), record);
        cx.push_undo(step);
    }
}

/// Moves the view with the pointer.
///
/// An operator that changes no model state at all, which is why it is here: the view is
/// the widget's, and an operator may not touch a widget (§38.3). So it leaves the
/// movement in [`EditorWorld::pan`] and the driver carries it into the canvas, exactly
/// as it carries a moved node. In screen units, because the view moves under the
/// pointer while this runs.
///
/// **It used to decide what a click was, and no longer does** (§39.1). Under §38 pointer
/// capture was granted during the press and at no other time, so an operator that might
/// have to hold the pointer had to start on the press — before anyone could know what
/// the gesture would become — and whoever held it was the only one who could say
/// afterwards what it had been. That is why this operator carried `click_deselects`.
/// With the layer seat able to withhold events, the runtime resolves the gesture before
/// any operator starts, and a click on empty canvas is its own binding on
/// `node.select`.
#[derive(Default)]
pub struct PanOp {
    anchor: Point,
    total: Vec2,
}

impl<G: NodeGraph> Operator<EditorWorld<G>> for PanOp {
    fn name(&self) -> &'static str {
        "view.pan"
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        // Where the *press* was, not where the pointer is now: a drag is recognised a
        // few pixels after it started, and those pixels are part of the movement. This
        // is what `OpEvent::Drag` carries the gesture's origin for (§39.3).
        self.anchor = match cx.event() {
            Some(OpEvent::Drag { screen, .. }) => *screen,
            _ => cx.world().pointer_screen,
        };
        self.total = Vec2::ZERO;
        OpResult::Running
    }

    /// From a script: the whole movement at once.
    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let delta = Vec2::new(cx.props().float("dx", 0.0), cx.props().float("dy", 0.0));
        cx.world_mut().pan += delta;
        OpResult::Finished
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
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
            }) => OpResult::Finished,
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

/// Zooms the view about the pointer.
///
/// What the canvas used to do on the wheel by itself, past the keymap — so the wheel
/// could be neither rebound nor scripted (`issues/keymap file and zoom operator.md`). From
/// the wheel the factor follows the scroll, at the canvas's own rate
/// ([`WHEEL_ZOOM_RATE`](blazy_canvas::WHEEL_ZOOM_RATE)), so rebinding nothing changes
/// nothing about how it feels; from a key or a script it is the `factor` property.
///
/// Like `view.pan` it changes the view and not the model, so it writes no undo step and
/// the other views of the graph do not follow it (§30).
#[derive(Default)]
pub struct ZoomOp;

impl ZoomOp {
    /// The factor a key binding zooms by when it does not say.
    pub const STEP: f64 = 1.25;
}

impl<G: NodeGraph> Operator<EditorWorld<G>> for ZoomOp {
    fn name(&self) -> &'static str {
        "view.zoom"
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let factor = match cx.event() {
            // Rolled towards the user is out, as the canvas has always had it.
            Some(OpEvent::Scroll { dy, .. }) => {
                let rate = cx.props().float("rate", blazy_canvas::WHEEL_ZOOM_RATE);
                (-dy * rate).exp()
            },
            _ => cx.props().float("factor", Self::STEP),
        };
        // About the pointer, in screen units: the canvas point under it stays under it.
        let origin = cx.world().pointer_screen;
        zoom(cx, origin, factor)
    }

    /// From a script: `factor`, about `x`/`y` in the driver's own units, or about the
    /// pointer when they are not given.
    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let pointer = cx.world().pointer_screen;
        let origin = Point::new(cx.props().float("x", pointer.x), cx.props().float("y", pointer.y));
        let factor = cx.props().float("factor", Self::STEP);
        zoom(cx, origin, factor)
    }
}

/// Leaves a zoom for the driver, unless it would do nothing.
fn zoom<G: NodeGraph>(cx: &mut OpCtx<'_, EditorWorld<G>>, origin: Point, factor: f64) -> OpResult {
    if !factor.is_finite() || factor <= 0.0 || factor == 1.0 {
        return OpResult::Cancelled;
    }
    cx.world_mut().zoom.push((origin, factor));
    OpResult::Finished
}

/// Adds a node where the pointer is, and selects it.
///
/// The size is [`EditorWorld::new_node_size`] unless the binding says otherwise, and the
/// place is the pointer unless it does: a menu entry and a script both run the same
/// operator, and neither has a pointer to mean anything by (§11).
#[derive(Default)]
pub struct AddNodeOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for AddNodeOp {
    fn name(&self) -> &'static str {
        "node.add"
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let pointer = cx.world().pointer;
        let default = cx.world().new_node_size;
        let origin = Point::new(cx.props().float("x", pointer.x), cx.props().float("y", pointer.y));
        let size = Size::new(
            cx.props().float("width", default.width),
            cx.props().float("height", default.height),
        );
        let rect = Rect::from_origin_size(origin, size);
        let selection = cx.world().selection.clone();
        let index = cx.world_mut().add_node(rect);
        // Selected here rather than through `set_selection`, because that records a step
        // of its own and this is one action: see `AddStep`.
        cx.world_mut().select(BTreeSet::from([index]));
        cx.push_undo(Box::new(AddStep { index, rect, selection }));
        OpResult::Finished
    }
}

/// Deletes the selection, or the node under the pointer if nothing is selected.
///
/// Every link that ended on a deleted node goes with it, and comes back with it: that is
/// one action to the user, so it is one step in the history.
#[derive(Default)]
pub struct DeleteNodeOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for DeleteNodeOp {
    fn name(&self) -> &'static str {
        "node.delete"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        !cx.world().selection.is_empty() || cx.world().hovered_node().is_some() || cx.props().int("index", -1) >= 0
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let named = cx.props().int("index", -1);
        let doomed: Vec<usize> = if named >= 0 {
            vec![named as usize]
        } else if cx.world().selection.is_empty() {
            cx.world().hovered_node().into_iter().collect()
        } else {
            cx.world().selection.iter().copied().collect()
        };
        if doomed.is_empty() {
            return OpResult::Cancelled;
        }

        let selection = cx.world().selection.clone();
        let nodes: Vec<(usize, Rect, Vec<Link>)> = doomed
            .into_iter()
            .map(|index| {
                let (rect, links) = cx.world_mut().remove_node(index);
                (index, rect, links)
            })
            .collect();
        cx.push_undo(Box::new(DeleteStep { nodes, selection }));
        OpResult::Finished
    }
}

/// The two ends a link operator is about: named by the binding, or the selection.
fn ends<G: NodeGraph>(cx: &OpCtx<'_, EditorWorld<G>>) -> Option<Link> {
    let (from, to) = (cx.props().int("from", -1), cx.props().int("to", -1));
    if from >= 0 && to >= 0 {
        // Ports as well, when a script names them; output 0 and input 0 when it does not,
        // which is what a link between two whole nodes always meant.
        let port = |name| u16::try_from(cx.props().int(name, 0)).unwrap_or(0);
        return Some(Link::between(
            from as usize,
            port("from_port"),
            to as usize,
            port("to_port"),
        ));
    }
    let mut selected = cx.world().selection.iter().copied();
    match (selected.next(), selected.next(), selected.next()) {
        (Some(from), Some(to), None) => Some(Link::new(from, to)),
        _ => None,
    }
}

/// The link under the pointer, as the canvas last picked it.
fn hovered_link<G: NodeGraph>(cx: &OpCtx<'_, EditorWorld<G>>) -> Option<Link> {
    match cx.world().hover {
        Some(CanvasHit::Link { link, .. }) => Some(link),
        _ => None,
    }
}

/// Links two nodes: the ones a binding names, or the two that are selected.
#[derive(Default)]
pub struct AddLinkOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for AddLinkOp {
    fn name(&self) -> &'static str {
        "link.add"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        ends(cx).is_some()
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let Some(link) = ends(cx) else {
            return OpResult::Cancelled;
        };
        // The graph refuses an end that is not there and a link it already holds, and
        // either way there is nothing to put in the history.
        if !cx.world_mut().add_link(link) {
            return OpResult::Cancelled;
        }
        cx.push_undo(Box::new(LinkStep { link, added: true }));
        OpResult::Finished
    }
}

/// Drags a link out of a port and drops it on another node's port.
///
/// Modal, started by a drag that began on a port (bound ahead of `node.move`, whose poll
/// would otherwise take the press, since a port is part of its node). While it runs it
/// asks the driver to pick on every move (`EditorWorld::track_hover`) — the pointer is the
/// operator's, the canvas below sees no events and publishes no hover — and leaves the
/// curve to draw in `EditorWorld::link_preview`. On release over a port of the other side
/// of another node, the link goes into the graph through the same path `link.add` takes,
/// undo step included; anywhere else nothing happens. `Escape` or the other button
/// cancels.
#[derive(Default)]
pub struct LinkDragOp {
    /// The port the drag started from: node, side, number, and where it is.
    start: Option<(usize, PortSide, u16, Point)>,
}

impl<G: NodeGraph> Operator<EditorWorld<G>> for LinkDragOp {
    fn name(&self) -> &'static str {
        "link.drag"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        matches!(cx.world().hover, Some(CanvasHit::Port { .. }))
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let Some(CanvasHit::Port { index, side, port, pos }) = cx.world().hover else {
            return OpResult::Cancelled;
        };
        self.start = Some((index, side, port, pos));
        let pointer = cx.world().pointer;
        let world = cx.world_mut();
        world.track_hover = true;
        world.link_preview = Some(preview(side, pos, pointer));
        world.dirty = true;
        OpResult::Running
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let Some((node, side, port, at)) = self.start else {
            return OpResult::Cancelled;
        };
        let event = cx.event().cloned();
        match event {
            Some(OpEvent::Move { .. }) => {
                let pointer = cx.world().pointer;
                // To the port under the pointer if it would take the link, so the curve
                // shows where it will land; to the pointer otherwise.
                let end = match cx.world().hover {
                    Some(CanvasHit::Port {
                        index,
                        side: other,
                        pos,
                        ..
                    }) if index != node && other != side => pos,
                    _ => pointer,
                };
                let world = cx.world_mut();
                world.link_preview = Some(preview(side, at, end));
                world.dirty = true;
                OpResult::Running
            },
            Some(OpEvent::Release {
                button: PointerButton::Primary,
                ..
            }) => {
                let target = cx.world().hover;
                end_drag(cx);
                let Some(CanvasHit::Port {
                    index,
                    side: other,
                    port: other_port,
                    ..
                }) = target
                else {
                    return OpResult::Cancelled;
                };
                if index == node || other == side {
                    return OpResult::Cancelled;
                }
                // Out of an output, into an input, whichever end the drag started from.
                let link = match side {
                    PortSide::Output => Link::between(node, port, index, other_port),
                    PortSide::Input => Link::between(index, other_port, node, port),
                };
                if !cx.world_mut().add_link(link) {
                    return OpResult::Cancelled;
                }
                cx.push_undo(Box::new(LinkStep { link, added: true }));
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
                end_drag(cx);
                OpResult::Cancelled
            },
            _ => OpResult::Running,
        }
    }
}

/// The preview of a link being dragged, from its output end to its input end.
fn preview(side: PortSide, port: Point, other: Point) -> (Point, Point) {
    match side {
        PortSide::Output => (port, other),
        PortSide::Input => (other, port),
    }
}

/// Takes the drag's traces out of the world, whatever the drag ended in.
fn end_drag<G: NodeGraph>(cx: &mut OpCtx<'_, EditorWorld<G>>) {
    let world = cx.world_mut();
    world.track_hover = false;
    world.link_preview = None;
    world.dirty = true;
}

/// Deletes the link under the pointer, or the one a binding names.
#[derive(Default)]
pub struct DeleteLinkOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for DeleteLinkOp {
    fn name(&self) -> &'static str {
        "link.delete"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        hovered_link(cx).is_some() || ends(cx).is_some()
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let Some(link) = hovered_link(cx).or_else(|| ends(cx)) else {
            return OpResult::Cancelled;
        };
        delete_link(cx, link)
    }

    /// From a script there is no pointer, so only a named pair will do.
    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let Some(link) = ends(cx) else {
            return OpResult::Cancelled;
        };
        delete_link(cx, link)
    }
}

fn delete_link<G: NodeGraph>(cx: &mut OpCtx<'_, EditorWorld<G>>, link: Link) -> OpResult {
    cx.world_mut().remove_link(link);
    cx.push_undo(Box::new(LinkStep { link, added: false }));
    OpResult::Finished
}

/// Undo, as an operator, so the keymap reaches it the way it reaches everything else.
#[derive(Default)]
pub struct UndoOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for UndoOp {
    fn name(&self) -> &'static str {
        "ed.undo"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        cx.history().depth() > 0
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        cx.undo();
        cx.world_mut().dirty = true;
        OpResult::Finished
    }
}

/// Redo.
#[derive(Default)]
pub struct RedoOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for RedoOp {
    fn name(&self) -> &'static str {
        "ed.redo"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        cx.history().redo_depth() > 0
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        cx.redo();
        cx.world_mut().dirty = true;
        OpResult::Finished
    }
}

/// The keymap a node editor opens with — Blender's classic one, as data.
///
/// **Click and drag are triggers now** (§39.1), and the two sentences that used to be
/// properties on somebody else's binding are two rows a user could move to another
/// button: "drag on empty canvas pans the view" and "click on empty canvas deselects".
/// Each button still carries several bindings whose operators' polls tell them apart —
/// dragging moves the node under the pointer if there is one and the view if there is
/// not — which is the mechanism §11 describes and the whole reason `poll` exists.
///
/// `G` and `B` start the same two operators from the keyboard. They are kept because
/// they are Blender's, and because a modal operator started by a key cannot take pointer
/// capture: the difference between the two kinds of start is the finding of §38.1, and
/// an example with only one kind would hide it. What §39 changed is that it no longer
/// *costs* anything — the layer seat withholds the events either way.
pub fn default_keymap() -> Keymap {
    Keymap::new()
        .with(CANVAS_CONTEXT, vec![
            // Left drag: a link out of the port under the pointer, the node under the
            // pointer, or the view. Three rows, and the polls tell them apart — the port
            // first, because a port is part of its node and `node.move` would take it.
            Binding::new(Pattern::drag(PointerButton::Primary), "link.drag"),
            Binding::new(Pattern::drag(PointerButton::Primary), "node.move"),
            Binding::new(Pattern::drag(PointerButton::Primary), "view.pan"),
            // Left click: the node under the pointer, or nothing at all — the sentence
            // that used to live inside `view.pan` as `click_deselects`.
            Binding::new(Pattern::click(PointerButton::Primary), "node.select")
                .with_props(Props::new().with_bool("deselect_all", true)),
            // Shift-left: add to the selection, by node or by band.
            Binding::new(
                Pattern::click(PointerButton::Primary).with_mods(Modifiers::SHIFT),
                "node.select",
            )
            .with_props(Props::new().with_bool("extend", true)),
            Binding::new(
                Pattern::drag(PointerButton::Primary).with_mods(Modifiers::SHIFT),
                "node.box_select",
            )
            .with_props(Props::new().with_bool("extend", true)),
            // Right: click selects the node under the pointer, drag pulls a rubber band.
            Binding::new(Pattern::click(PointerButton::Secondary), "node.select"),
            Binding::new(Pattern::drag(PointerButton::Secondary), "node.box_select"),
            Binding::new(
                Pattern::click(PointerButton::Secondary).with_mods(Modifiers::SHIFT),
                "node.select",
            )
            .with_props(Props::new().with_bool("extend", true)),
            // The keyboard half.
            Binding::new(Pattern::key("b"), "node.box_select"),
            Binding::new(Pattern::key("g"), "node.move"),
            // Structure (§43). `X` is two bindings on one key and the polls tell them
            // apart, exactly as the mouse buttons do: a link under the pointer goes
            // first, because pointing at something is how a gesture names its target.
            Binding::new(Pattern::key("a").with_mods(Modifiers::SHIFT), "node.add"),
            Binding::new(Pattern::key("x"), "link.delete"),
            Binding::new(Pattern::key("x"), "node.delete"),
            Binding::new(Pattern::key("f"), "link.add"),
            // The view. The wheel used to be the canvas's own and nobody's to rebind; `=`
            // and `-` are the keys of a laptop with no numpad, where Blender's own are.
            Binding::new(Pattern::scroll(), "view.zoom"),
            Binding::new(Pattern::key("="), "view.zoom").with_props(Props::new().with_float("factor", ZoomOp::STEP)),
            Binding::new(Pattern::key("-"), "view.zoom")
                .with_props(Props::new().with_float("factor", 1.0 / ZoomOp::STEP)),
            // The editor's menu, under the pointer — Blender's old `W`.
            Binding::new(Pattern::key("w"), NODE_MENU),
        ])
        .with("window", vec![
            Binding::new(Pattern::key("z").with_mods(Modifiers::CONTROL), "ed.undo"),
            Binding::new(
                Pattern::key("z").with_mods(Modifiers::CONTROL | Modifiers::SHIFT),
                "ed.redo",
            ),
        ])
}

/// The operator that opens [`node_menu`].
pub const NODE_MENU: &str = "menu.node";

/// The editor's menu: what can be done to the graph and the view from where the pointer is.
///
/// Every entry is an operator the keymap also binds, with the properties it binds it with,
/// so the menu shows each one's shortcut — and teaches it.
pub fn node_menu() -> crate::Menu {
    crate::Menu::new("Node")
        .with("Add node", "node.add", Props::new())
        .with("Delete", "node.delete", Props::new())
        .with("Link selected", "link.add", Props::new())
        .with("Box select", "node.box_select", Props::new())
        .with("Zoom in", "view.zoom", Props::new().with_float("factor", ZoomOp::STEP))
        .with(
            "Zoom out",
            "view.zoom",
            Props::new().with_float("factor", 1.0 / ZoomOp::STEP),
        )
        .with("Undo", "ed.undo", Props::new())
        .with("Redo", "ed.redo", Props::new())
}

/// A runtime with these operators registered and [`default_keymap`] in force.
///
/// A starting point rather than the only arrangement: a runtime with more operators, or
/// a keymap read from a file, goes to [`NodeEditor::with_runtime`](crate::NodeEditor::with_runtime).
pub fn runtime<G: NodeGraph>() -> OpRuntime<EditorWorld<G>> {
    runtime_with(default_keymap())
}

/// The same operators, with `keymap` in force — one read from a file, or the defaults
/// with a user's overrides laid over them (`Keymap::patched`).
pub fn runtime_with<G: NodeGraph>(keymap: Keymap) -> OpRuntime<EditorWorld<G>> {
    let mut runtime = OpRuntime::new(keymap);
    runtime.register(SelectOp);
    runtime.register(BoxSelectOp::default());
    runtime.register(MoveOp::default());
    runtime.register(PanOp::default());
    runtime.register(ZoomOp);
    runtime.register(LinkDragOp::default());
    runtime.register(crate::MenuOp::new(NODE_MENU, node_menu()));
    runtime.register(AddNodeOp);
    runtime.register(DeleteNodeOp);
    runtime.register(AddLinkOp);
    runtime.register(DeleteLinkOp);
    runtime.register(UndoOp);
    runtime.register(RedoOp);
    runtime
}
