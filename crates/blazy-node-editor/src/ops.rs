//! The node editor's operators and their default keymap (Blender's classic one).
//!
//! An operator changes the graph and leaves in the world what the views have to follow;
//! it never touches a widget. So the same operators run in a test without a widget tree,
//! and a key, a menu and a script do the same thing.

use std::collections::BTreeSet;
use std::rc::Rc;

use blazy_canvas::{CanvasHit, PortSide};
use blazy_ops::event::{OpEvent, Pattern};
use blazy_ops::keymap::{Binding, Keymap, Props, Scope};
use blazy_ops::runtime::{OpCtx, OpRuntime};
use blazy_ops::{OpResult, Operator};
use masonry::core::keyboard::{Key, Modifiers, NamedKey};
use masonry::kurbo::{Point, Rect, Size, Vec2};
use masonry::ui_events::pointer::PointerButton;

use crate::world::{AddStep, Clip, CopyStep, DeleteStep, LinkStep, RelinkStep, ResizeStep, SelectStep};
use crate::{EDGE_PAN, EditorWorld, Link, LinkPreview, MoveRecord, NodeGraph};

/// The keymap context a node canvas's bindings live in.
pub const CANVAS_CONTEXT: &str = "canvas";

/// The context chain of an event over the canvas, innermost first: the canvas, the window,
/// the screen. A key bound in two of them belongs to the first.
pub const CANVAS_SCOPE: [&str; 3] = [CANVAS_CONTEXT, "window", crate::SCREEN_CONTEXT];

/// Selects the node under the pointer.
#[derive(Default)]
pub struct SelectOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for SelectOp {
    fn name(&self) -> &'static str {
        "node.select"
    }

    /// Over a node, for a node named by `index`, or with `deselect_all` (a click on
    /// nothing). Refusing elsewhere lets a drag on empty canvas reach the box select.
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
        cx.world().selection()
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
fn set_selection<G: NodeGraph>(cx: &mut OpCtx<'_, EditorWorld<G>>, after: BTreeSet<usize>) {
    let before = cx.world().selection();
    if cx.world_mut().select(after.clone()) {
        cx.push_undo(Box::new(SelectStep { before, after }));
    }
}

/// Drags a rubber band and selects what it covers: from a drag, or from `B` and then a
/// press.
#[derive(Default)]
pub struct BoxSelectOp {
    anchor: Point,
    tracking: bool,
    /// The button holding the band, if a button started it (the keymap binds two).
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
            // Escape, the other button, or no event at all (cut off): cancel.
            Some(OpEvent::Key {
                key: Key::Named(NamedKey::Escape),
                down: true,
                ..
            })
            | None => {
                cx.world_mut().band = None;
                cx.world_mut().dirty = true;
                OpResult::Cancelled
            },
            Some(OpEvent::Press { button, .. }) if self.button != Some(button) => {
                cx.world_mut().band = None;
                cx.world_mut().dirty = true;
                OpResult::Cancelled
            },
            // A move before the press, or an unclaimed key.
            _ => OpResult::Running,
        }
    }
}

fn finish_box<G: NodeGraph>(cx: &mut OpCtx<'_, EditorWorld<G>>, rect: Rect) {
    let extend = cx.props().bool("extend", false);
    let mut after = cx.world().nodes_in(rect);
    if extend {
        after.extend(cx.world().selection().iter().copied());
    }
    set_selection(cx, after);
}

/// Moves the selection with the pointer — Blender's grab. A drag confirms on release;
/// `G` confirms on the next press.
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

    /// Something to move: a drag moves the node it started on (never from empty canvas,
    /// which pans); a key or a script moves the selection.
    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        match cx.event() {
            Some(OpEvent::Drag { .. } | OpEvent::Click { .. } | OpEvent::Press { .. }) => {
                cx.world().hovered_node().is_some()
            },
            _ => cx.world().hovered_node().is_some() || !cx.world().selection().is_empty(),
        }
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        self.from_press = matches!(cx.event(), Some(OpEvent::Drag { .. } | OpEvent::Press { .. }));
        // Dragging an unselected node selects it.
        if let Some(index) = cx.world().hovered_node()
            && self.from_press
            && !cx.world().is_selected(index)
        {
            select(cx, index);
        }
        self.start(cx);
        OpResult::Running
    }

    /// From a script: the whole move at once, from `dx` and `dy`.
    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        self.grab(cx);
        let delta = Vec2::new(cx.props().float("dx", 0.0), cx.props().float("dy", 0.0));
        self.apply(cx, delta);
        self.commit(cx);
        OpResult::Finished
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        match self.follow(cx) {
            Gesture::Running => OpResult::Running,
            Gesture::Confirmed => {
                self.commit(cx);
                OpResult::Finished
            },
            Gesture::Cancelled => OpResult::Cancelled,
        }
    }
}

/// What one event did to a grab.
enum Gesture {
    Running,
    Confirmed,
    /// The nodes are back where they started.
    Cancelled,
}

impl MoveOp {
    /// Starts a grab of the selection from the event that started the operator.
    fn start<G: NodeGraph>(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) {
        self.anchor = match cx.event() {
            Some(OpEvent::Drag { pos, .. }) => *pos,
            _ => cx.world().pointer,
        };
        self.grab(cx);
    }

    /// Follows one event of the gesture: moves the nodes, or says it is over. No event
    /// (cut off) cancels. Shared with `node.duplicate`, which records the end differently.
    fn follow<G: NodeGraph>(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> Gesture {
        let event = cx.event().cloned();
        match event {
            Some(OpEvent::Move { .. }) => {
                let delta = cx.world().pointer - self.anchor;
                self.apply(cx, delta);
                Gesture::Running
            },
            // The release, for a drag; the next press, for a grab from a key.
            Some(OpEvent::Release {
                button: PointerButton::Primary,
                ..
            }) if self.from_press => Gesture::Confirmed,
            Some(OpEvent::Press {
                button: PointerButton::Primary,
                ..
            }) if !self.from_press => Gesture::Confirmed,
            Some(OpEvent::Key {
                key: Key::Named(NamedKey::Enter),
                down: true,
                ..
            }) => Gesture::Confirmed,
            Some(OpEvent::Key {
                key: Key::Named(NamedKey::Escape),
                down: true,
                ..
            })
            | Some(OpEvent::Press {
                button: PointerButton::Secondary,
                ..
            })
            | None => {
                // The runtime keeps no snapshot: putting things back is the operator's job.
                self.apply(cx, Vec2::ZERO);
                Gesture::Cancelled
            },
            _ => Gesture::Running,
        }
    }

    /// Records what is being moved and where it started.
    fn grab<G: NodeGraph>(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) {
        self.nodes = cx.world().selection().iter().copied().collect();
        self.from = self
            .nodes
            .iter()
            .map(|&index| cx.world().node_rect(index).origin())
            .collect();
    }

    /// Puts every grabbed node at its start plus `delta` (so a cancel is `apply(ZERO)`).
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

/// Moves the view with the pointer, in screen units. Leaves the movement in the world for
/// the driver: the view is the widget's.
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
        // From the press, not the pointer: a drag is recognised a few pixels late.
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
                // Putting the view back is the operator's job.
                cx.world_mut().pan -= self.total;
                OpResult::Cancelled
            },
            // Cut off: the view stays where it was taken (it is not the graph).
            None => OpResult::Finished,
            _ => OpResult::Running,
        }
    }
}

/// Zooms the view about the pointer: by the scroll at
/// [`WHEEL_ZOOM_RATE`](blazy_canvas::WHEEL_ZOOM_RATE) from the wheel, by `factor` from a key
/// or a script. No undo step, and other views do not follow.
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
            // Rolled towards the user is out.
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

/// Adds a node where the pointer is (or at `x`/`y`), and selects it.
///
/// With `kind` set, the node is of that kind ([`NodeGraph::insert_kind`]); a kind the
/// graph does not list is refused in the poll.
#[derive(Default)]
pub struct AddNodeOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for AddNodeOp {
    fn name(&self) -> &'static str {
        "node.add"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        cx.props().text("kind").is_none_or(|kind| {
            cx.world()
                .graph
                .borrow()
                .node_kinds()
                .iter()
                .any(|known| known.name == kind)
        })
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let pointer = cx.world().pointer;
        let default = cx.world().new_node_size;
        let origin = Point::new(cx.props().float("x", pointer.x), cx.props().float("y", pointer.y));
        let size = Size::new(
            cx.props().float("width", default.width),
            cx.props().float("height", default.height),
        );
        let asked = Rect::from_origin_size(origin, size);
        let selection = cx.world().selection();
        let index = match cx.props().text("kind").map(str::to_owned) {
            Some(kind) => match cx.world_mut().add_kind(asked, &kind) {
                Some(index) => index,
                None => {
                    // Listed but not made: the graph's `insert_kind` is missing or refused.
                    tracing::warn!(kind, "the graph lists this kind and did not add a node of it");
                    return OpResult::Cancelled;
                },
            },
            None => cx.world_mut().add_node(asked),
        };
        // The rectangle the model gave the node.
        let rect = cx.world().node_rect(index);
        let content = cx.world().graph.borrow().content(index);
        // Not through `set_selection`, which records a step of its own.
        cx.world_mut().select(BTreeSet::from([index]));
        cx.push_undo(Box::new(AddStep {
            index,
            rect,
            content,
            selection,
        }));
        OpResult::Finished
    }
}

/// Deletes the selection, or the node under the pointer if nothing is selected, with
/// their links, in one step.
#[derive(Default)]
pub struct DeleteNodeOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for DeleteNodeOp {
    fn name(&self) -> &'static str {
        "node.delete"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        !cx.world().selection().is_empty() || cx.world().hovered_node().is_some() || cx.props().int("index", -1) >= 0
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let named = cx.props().int("index", -1);
        let doomed: Vec<usize> = if named >= 0 {
            vec![named as usize]
        } else if cx.world().selection().is_empty() {
            cx.world().hovered_node().into_iter().collect()
        } else {
            cx.world().selection().iter().copied().collect()
        };
        if doomed.is_empty() {
            return OpResult::Cancelled;
        }

        let selection = cx.world().selection();
        let nodes = doomed
            .into_iter()
            .map(|index| cx.world_mut().remove_node(index))
            .collect();
        cx.push_undo(Box::new(DeleteStep { nodes, selection }));
        OpResult::Finished
    }
}

/// Copies the selection and the links between it to the graph's clipboard, and to the
/// system clipboard when the world has one. No undo step.
#[derive(Default)]
pub struct CopyOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for CopyOp {
    fn name(&self) -> &'static str {
        "node.copy"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        !cx.world().selection().is_empty()
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let selection = cx.world().selection();
        let graph = cx.world().graph.borrow();
        let clip = Clip::of(&*graph, &selection);
        // As text too, when the graph has a text form.
        let text = clip.to_text(&*graph);
        // Without a system clipboard, the text waits in the registry
        // (`Views::take_outbound_clip`).
        if let (Some(clipboard), Some(text)) = (cx.world().clipboard(), text.clone()) {
            clipboard.write(text);
        }
        graph.views().set_clip(Rc::new(clip), text);
        if cx.world().clipboard().is_some() {
            let _ = graph.views().take_outbound_clip();
        }
        OpResult::Finished
    }
}

/// The copy to paste: what the system clipboard holds if it is a copy this graph reads,
/// else this process's own.
fn clip<G: NodeGraph>(world: &EditorWorld<G>) -> Option<Rc<Clip<G::Content>>> {
    // The system clipboard first, when the world has one.
    if let Some(text) = world.clipboard().and_then(crate::SystemClipboard::read) {
        world.graph.borrow().views().offer_clip_text(text);
    }
    let graph = world.graph.borrow();
    let views = graph.views();
    if let Some(text) = views.take_offered()
        && let Some(copy) = Clip::from_text(&*graph, &text)
    {
        views.set_clip(Rc::new(copy), Some(text));
        // Already on the system clipboard: it came from there.
        let _ = views.take_outbound_clip();
    }
    views.clip()?.downcast().ok()
}

/// Pastes the copy centred on the pointer (or on `x`/`y`) and selects it, in one step.
#[derive(Default)]
pub struct PasteOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for PasteOp {
    fn name(&self) -> &'static str {
        "node.paste"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        clip(cx.world()).is_some_and(|clip| !clip.nodes.is_empty())
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let Some(clip) = clip(cx.world()) else {
            return OpResult::Cancelled;
        };
        let pointer = cx.world().pointer;
        let at = Point::new(cx.props().float("x", pointer.x), cx.props().float("y", pointer.y));
        let step = clip.paste_into(cx.world_mut(), at - clip.bounds().center(), "node.paste");
        cx.push_undo(Box::new(step));
        OpResult::Finished
    }
}

/// Duplicates the selection in place and grabs the copies — Blender's
/// `node.duplicate_move`, as one undo step.
///
/// Cancelling the grab leaves the copies where they were made, as in Blender. From a
/// script or a menu the copies are moved by `dx` and `dy` instead.
pub struct DuplicateOp<C> {
    grab: MoveOp,
    pending: Option<CopyStep<C>>,
}

impl<C> Default for DuplicateOp<C> {
    fn default() -> Self {
        Self {
            grab: MoveOp::default(),
            pending: None,
        }
    }
}

impl<G: NodeGraph> Operator<EditorWorld<G>> for DuplicateOp<G::Content> {
    fn name(&self) -> &'static str {
        "node.duplicate"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        !cx.world().selection().is_empty()
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let clip = Clip::of(&*cx.world().graph.borrow(), &cx.world().selection());
        self.pending = Some(clip.paste_into(cx.world_mut(), Vec2::ZERO, "node.duplicate"));
        self.grab.from_press = matches!(cx.event(), Some(OpEvent::Drag { .. } | OpEvent::Press { .. }));
        self.grab.start(cx);
        OpResult::Running
    }

    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let clip = Clip::of(&*cx.world().graph.borrow(), &cx.world().selection());
        let delta = Vec2::new(cx.props().float("dx", 0.0), cx.props().float("dy", 0.0));
        let step = clip.paste_into(cx.world_mut(), delta, "node.duplicate");
        cx.push_undo(Box::new(step));
        OpResult::Finished
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        if let Gesture::Running = self.grab.follow(cx) {
            return OpResult::Running;
        }
        let Some(mut step) = self.pending.take() else {
            return OpResult::Cancelled;
        };
        // Where the copies ended up, for redo.
        for (index, rect, _) in &mut step.nodes {
            *rect = cx.world().node_rect(*index);
        }
        cx.push_undo(Box::new(step));
        OpResult::Finished
    }
}

/// The two ends a link operator is about: named by the binding, or the selection.
fn ends<G: NodeGraph>(cx: &OpCtx<'_, EditorWorld<G>>) -> Option<Link> {
    let (from, to) = (cx.props().int("from", -1), cx.props().int("to", -1));
    if from >= 0 && to >= 0 {
        // Ports too, when named; output 0 and input 0 otherwise.
        let port = |name| u16::try_from(cx.props().int(name, 0)).unwrap_or(0);
        return Some(Link::between(
            from as usize,
            port("from_port"),
            to as usize,
            port("to_port"),
        ));
    }
    let selection = cx.world().selection();
    let mut selected = selection.iter().copied();
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
        // A refused link records nothing; a full input gives up its oldest link.
        let Some(removed) = cx.world_mut().connect(link) else {
            return OpResult::Cancelled;
        };
        if removed.is_empty() {
            cx.push_undo(Box::new(LinkStep { link, added: true }));
        } else {
            cx.push_undo(Box::new(RelinkStep {
                removed,
                added: vec![link],
            }));
        }
        OpResult::Finished
    }
}

/// Drags a link out of a port onto another port, or takes links off a port — Blender's
/// `node.link`. What it carries depends on where it started:
///
/// * **An input with a link** — the link comes off the input and its input end follows the pointer, from the output it
///   still leaves. The last of them, if the graph lets an input take several.
/// * **An output with links, and `detach`** (`Ctrl` in the default keymap) — every link of the output comes off, and
///   their output ends follow the pointer together.
/// * **Anything else** — a new link out of the port.
///
/// Releasing on a port that takes them makes the links; anywhere else a link taken off
/// stays off. One step either way. `Escape`, the other button, or a cut-off puts back
/// what was taken off.
#[derive(Default)]
pub struct LinkDragOp {
    /// The side the ends that stay put are on; the pointer's end goes to the other.
    fixed: Option<PortSide>,
    /// The ends that stay put, by node and port.
    anchors: Vec<(usize, u16)>,
    /// The links taken off when the drag started, to be put back if it is cancelled.
    detached: Vec<Link>,
}

impl<G: NodeGraph> Operator<EditorWorld<G>> for LinkDragOp {
    fn name(&self) -> &'static str {
        "link.drag"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        matches!(cx.world().hover, Some(CanvasHit::Port { .. }))
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let Some(CanvasHit::Port { index, side, port, .. }) = cx.world().hover else {
            return OpResult::Cancelled;
        };
        let detach = cx.props().bool("detach", false);
        let taken = taken_off(&*cx.world().graph.borrow(), index, side, port, detach);
        (self.fixed, self.anchors) = match (side, taken.first()) {
            (PortSide::Input, Some(&link)) => (Some(PortSide::Output), vec![(link.from as usize, link.from_port)]),
            (PortSide::Output, Some(_)) => (
                Some(PortSide::Input),
                taken.iter().map(|link| (link.to as usize, link.to_port)).collect(),
            ),
            (_, None) => (Some(side), vec![(index, port)]),
        };
        self.detached = taken;
        for &link in &self.detached {
            cx.world_mut().remove_link(link);
        }
        cx.world_mut().track_hover = true;
        self.show(cx);
        OpResult::Running
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        if self.fixed.is_none() {
            return OpResult::Cancelled;
        }
        let event = cx.event().cloned();
        match event {
            Some(OpEvent::Move { .. }) => {
                self.show(cx);
                OpResult::Running
            },
            Some(OpEvent::Release {
                button: PointerButton::Primary,
                ..
            }) => self.let_go(cx),
            // Cut off by the runtime, too.
            Some(OpEvent::Key {
                key: Key::Named(NamedKey::Escape),
                down: true,
                ..
            })
            | Some(OpEvent::Press {
                button: PointerButton::Secondary,
                ..
            })
            | None => {
                end_drag(cx);
                for link in std::mem::take(&mut self.detached) {
                    cx.world_mut().add_link(link);
                }
                self.fixed = None;
                OpResult::Cancelled
            },
            _ => OpResult::Running,
        }
    }
}

/// How far inside a node's left or right edge a drag resizes it, in screen pixels
/// (Blender's `NODE_RESIZE_MARGIN`).
pub const RESIZE_EDGE_PX: f64 = 6.0;

/// The edge of a node a resize drags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeEdge {
    /// The left edge: the node's right edge stays put and its corner moves.
    Left,
    /// The right edge: the node's corner stays put.
    Right,
}

/// The node and edge a drag starting at `at` would resize, if any: a strip inside the left
/// or right edge of a resizable node. A port on the edge wins; a node too narrow on screen
/// has no edges.
pub(crate) fn edge_under<G: NodeGraph>(world: &EditorWorld<G>, at: Point) -> Option<(usize, NodeEdge)> {
    let Some(CanvasHit::Node { index, .. }) = world.hover else {
        return None;
    };
    world.graph.borrow().width_limits(index)?;
    let rect = world.node_rect(index);
    let margin = RESIZE_EDGE_PX / world.scale.max(f64::EPSILON);
    if rect.width() < 4.0 * margin {
        return None;
    }
    if at.x >= rect.x1 - margin && at.x <= rect.x1 {
        Some((index, NodeEdge::Right))
    } else if at.x >= rect.x0 && at.x < rect.x0 + margin {
        Some((index, NodeEdge::Left))
    } else {
        None
    }
}

/// Resizes nodes by an edge — Blender's `node.resize`.
///
/// Width only, as in Blender, where only a frame's height can be dragged: by the right edge
/// the corner stays, by the left the right edge does. Every selected node that may resize is
/// resized together, by the edge the drag started on, each held to its own
/// [`NodeGraph::width_limits`]; a node dragged by its edge while unselected is selected first,
/// as `node.move` takes an unselected node. One step in the history (`node.resize`), with the
/// rectangles before and after. `Escape`, the other button or the runtime cutting the
/// gesture off puts every rectangle back and leaves no step.
///
/// From a script: the selection, by `dx`, on the right edge — or the left with `left` set.
#[derive(Default)]
pub struct ResizeOp {
    edge: Option<NodeEdge>,
    /// Where the drag started, in canvas units along x.
    anchor: f64,
    /// Each node being resized: its name, its rectangle before, and the widths it may take.
    nodes: Vec<(usize, Rect, (f64, f64))>,
}

impl<G: NodeGraph> Operator<EditorWorld<G>> for ResizeOp {
    fn name(&self) -> &'static str {
        "node.resize"
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        match cx.event() {
            Some(OpEvent::Drag { pos, .. } | OpEvent::Press { pos, .. }) => edge_under(cx.world(), *pos).is_some(),
            // A script names its nodes by the selection.
            _ => !cx.world().selection().is_empty(),
        }
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let at = match cx.event() {
            Some(OpEvent::Drag { pos, .. } | OpEvent::Press { pos, .. }) => *pos,
            _ => cx.world().pointer,
        };
        let Some((index, edge)) = edge_under(cx.world(), at) else {
            return OpResult::Cancelled;
        };
        if !cx.world().is_selected(index) {
            set_selection(cx, BTreeSet::from([index]));
        }
        self.start(cx, edge, at.x);
        OpResult::Running
    }

    fn exec(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let edge = if cx.props().bool("left", false) {
            NodeEdge::Left
        } else {
            NodeEdge::Right
        };
        self.start(cx, edge, 0.0);
        self.apply(cx, cx.props().float("dx", 0.0));
        self.commit(cx)
    }

    fn modal(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let event = cx.event().cloned();
        match event {
            Some(OpEvent::Move { .. }) => {
                let dx = cx.world().pointer.x - self.anchor;
                self.apply(cx, dx);
                OpResult::Running
            },
            Some(OpEvent::Release {
                button: PointerButton::Primary,
                ..
            }) => self.commit(cx),
            Some(OpEvent::Key {
                key: Key::Named(NamedKey::Escape),
                down: true,
                ..
            })
            | Some(OpEvent::Press {
                button: PointerButton::Secondary,
                ..
            })
            | None => {
                self.apply(cx, 0.0);
                self.nodes.clear();
                OpResult::Cancelled
            },
            _ => OpResult::Running,
        }
    }
}

impl ResizeOp {
    /// Records what is being resized, by which edge, from where.
    fn start<G: NodeGraph>(&mut self, cx: &OpCtx<'_, EditorWorld<G>>, edge: NodeEdge, anchor: f64) {
        self.edge = Some(edge);
        self.anchor = anchor;
        let world = cx.world();
        let graph = world.graph.borrow();
        self.nodes = world
            .selection()
            .into_iter()
            .filter_map(|index| Some((index, graph.node_rect(index), graph.width_limits(index)?)))
            .collect();
    }

    /// Every node at its starting rectangle, widened by `dx` at the edge being dragged.
    fn apply<G: NodeGraph>(&self, cx: &mut OpCtx<'_, EditorWorld<G>>, dx: f64) {
        let Some(edge) = self.edge else {
            return;
        };
        for &(index, from, limits) in &self.nodes {
            let to = resized(from, edge, dx, limits);
            if cx.world().node_rect(index) != to {
                cx.world_mut().set_rect(index, to);
            }
        }
    }

    /// Writes the step, if any rectangle changed, and ends.
    fn commit<G: NodeGraph>(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let nodes: Vec<(usize, Rect, Rect)> = std::mem::take(&mut self.nodes)
            .into_iter()
            .map(|(index, from, _)| (index, from, cx.world().node_rect(index)))
            .filter(|&(_, from, to)| from != to)
            .collect();
        if nodes.is_empty() {
            return OpResult::Cancelled;
        }
        cx.push_undo(Box::new(ResizeStep { nodes }));
        OpResult::Finished
    }
}

/// `from` resized by `dx` at `edge`, its width held to `limits`.
fn resized(from: Rect, edge: NodeEdge, dx: f64, (min, max): (f64, f64)) -> Rect {
    match edge {
        NodeEdge::Right => {
            let width = (from.width() + dx).clamp(min, max);
            Rect::new(from.x0, from.y0, from.x0 + width, from.y1)
        },
        NodeEdge::Left => {
            let width = (from.width() - dx).clamp(min, max);
            Rect::new(from.x1 - width, from.y0, from.x1, from.y1)
        },
    }
}

/// The links a link drag from port `port` of `side` of node `index` takes off: the last
/// link into an input, every link out of an output with `detach`, none for a new link.
pub(crate) fn taken_off<G: NodeGraph>(graph: &G, index: usize, side: PortSide, port: u16, detach: bool) -> Vec<Link> {
    let mut on_port: Vec<Link> = graph
        .links_of(index)
        .into_iter()
        .filter(|link| match side {
            PortSide::Input => link.to as usize == index && link.to_port == port,
            PortSide::Output => link.from as usize == index && link.from_port == port,
        })
        .collect();
    match side {
        PortSide::Input => on_port.split_off(on_port.len().saturating_sub(1)),
        PortSide::Output if detach => on_port,
        PortSide::Output => Vec::new(),
    }
}

/// The port under the pointer if a drag from it with `mods` would start a link drag (some
/// `link.drag` binding matches), for the editor to light. `None` while an operator runs.
/// Polls are not run: that would be a poll per move.
pub(crate) fn port_hint<G: NodeGraph>(
    runtime: &OpRuntime<EditorWorld<G>>,
    world: &EditorWorld<G>,
    mods: Modifiers,
) -> Option<(usize, PortSide, u16)> {
    if runtime.modal_depth() > 0 {
        return None;
    }
    let Some(CanvasHit::Port { index, side, port, pos }) = world.hover else {
        return None;
    };
    let drags = [
        PointerButton::Primary,
        PointerButton::Secondary,
        PointerButton::Auxiliary,
    ]
    .map(|button| OpEvent::Drag {
        button,
        pos,
        screen: pos,
        mods,
    });
    runtime
        .keymap()
        .sections_for(Scope(&CANVAS_SCOPE))
        .any(|binding| &*binding.op == "link.drag" && drags.iter().any(|drag| binding.pattern.matches(drag)))
        .then_some((index, side, port))
}

/// A port by node, side and number, and where it is in canvas coordinates.
type PortAt = (usize, PortSide, u16, Point);

impl LinkDragOp {
    /// The links the drag would make on the port under the pointer; empty if they cannot go
    /// there. An anchor on the hovered node is skipped, as in Blender.
    fn targets<G: NodeGraph>(&self, world: &EditorWorld<G>) -> (Vec<Link>, Option<PortAt>) {
        let (Some(fixed), Some(CanvasHit::Port { index, side, port, pos })) = (self.fixed, world.hover) else {
            return (Vec::new(), None);
        };
        if side == fixed {
            return (Vec::new(), None);
        }
        let links: Vec<Link> = self
            .anchors
            .iter()
            .filter(|&&(node, _)| node != index)
            .map(|&(node, anchor)| match fixed {
                PortSide::Output => Link::between(node, anchor, index, port),
                PortSide::Input => Link::between(index, port, node, anchor),
            })
            .collect();
        let at = (!links.is_empty()).then_some((index, side, port, pos));
        (links, at)
    }

    /// Puts the curves in the world, to the port under the pointer or to the pointer.
    fn show<G: NodeGraph>(&self, cx: &mut OpCtx<'_, EditorWorld<G>>) {
        let Some(fixed) = self.fixed else {
            return;
        };
        let world = cx.world();
        let (links, at) = self.targets(world);
        let refused = {
            let graph = world.graph.borrow();
            links.iter().any(|&link| !graph.accepts_link(link))
        };
        let end = at.map_or(world.pointer, |(.., pos)| pos);
        let target = at.map(|(index, side, port, _)| (index, side, port));
        let curves = {
            let graph = world.graph.borrow();
            self.anchors
                .iter()
                .map(|&(node, port)| {
                    let anchor = world.ports.position(graph.node_rect(node), fixed, port);
                    match fixed {
                        PortSide::Output => (anchor, end),
                        PortSide::Input => (end, anchor),
                    }
                })
                .collect()
        };
        // A new link has one anchor, the port it is drawn out of.
        let source = match (self.detached.is_empty(), self.anchors.as_slice()) {
            (true, &[(node, port)]) => Some((node, fixed, port)),
            _ => None,
        };
        let world = cx.world_mut();
        world.link_preview = Some(LinkPreview {
            curves,
            target,
            source,
            refused,
        });
        world.dirty = true;
    }

    /// Lets go: makes the links on a port that takes them, and records what changed.
    fn let_go<G: NodeGraph>(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let (mut links, _) = self.targets(cx.world());
        if links.iter().any(|&link| !cx.world().graph.borrow().accepts_link(link)) {
            links.clear();
        }
        end_drag(cx);
        self.fixed = None;
        let mut removed = std::mem::take(&mut self.detached);
        // Through `connect`: a full input gives up its oldest link.
        let mut added = Vec::new();
        for link in links {
            if let Some(displaced) = cx.world_mut().connect(link) {
                removed.extend(displaced);
                added.push(link);
            }
        }
        // A link dropped back where it came from is no change, and in neither list.
        added.retain(|link| match removed.iter().position(|old| old == link) {
            Some(at) => {
                removed.swap_remove(at);
                false
            },
            None => true,
        });
        match (removed.is_empty(), added.as_slice()) {
            (true, []) => OpResult::Cancelled,
            // A new link, as `link.add` records one.
            (true, &[link]) => {
                cx.push_undo(Box::new(LinkStep { link, added: true }));
                OpResult::Finished
            },
            _ => {
                cx.push_undo(Box::new(RelinkStep { removed, added }));
                OpResult::Finished
            },
        }
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

/// Undo.
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
/// A button carries several bindings, told apart by their operators' polls: a drag moves
/// the node under the pointer if there is one, and the view otherwise. `G` and `B` start
/// the same operators from the keyboard.
pub fn default_keymap() -> Keymap {
    Keymap::new()
        .with(CANVAS_CONTEXT, vec![
            // Left drag: a link from a port, the node, or the view; the polls tell them
            // apart, the port first. `edge_pan` on the two that carry something, as in
            // Blender.
            Binding::new(Pattern::drag(PointerButton::Primary), "link.drag")
                .with_props(Props::new().with_bool(EDGE_PAN, true)),
            // `Ctrl`-drag on an output takes all its links off.
            Binding::new(
                Pattern::drag(PointerButton::Primary).with_mods(Modifiers::CONTROL),
                "link.drag",
            )
            .with_props(Props::new().with_bool("detach", true).with_bool(EDGE_PAN, true)),
            // A node's edge resizes it: after the links, before `node.move`.
            Binding::new(Pattern::drag(PointerButton::Primary), "node.resize"),
            Binding::new(Pattern::drag(PointerButton::Primary), "node.move")
                .with_props(Props::new().with_bool(EDGE_PAN, true)),
            Binding::new(Pattern::drag(PointerButton::Primary), "view.pan"),
            // Left click: the node under the pointer, or nothing.
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
            Binding::new(Pattern::key("g"), "node.move").with_props(Props::new().with_bool(EDGE_PAN, true)),
            // Structure. `X` deletes the link under the pointer first, else the selection.
            // `Shift+A` opens the `Add` menu for a graph with kinds, else adds a node.
            Binding::new(Pattern::key("a").with_mods(Modifiers::SHIFT), ADD_MENU),
            Binding::new(Pattern::key("a").with_mods(Modifiers::SHIFT), "node.add"),
            Binding::new(Pattern::key("x"), "link.delete"),
            Binding::new(Pattern::key("x"), "node.delete"),
            Binding::new(Pattern::key("f"), "link.add"),
            // Copies, on Blender's keys (inside a text field the field takes them).
            Binding::new(Pattern::key("d").with_mods(Modifiers::SHIFT), "node.duplicate")
                .with_props(Props::new().with_bool(EDGE_PAN, true)),
            Binding::new(Pattern::key("c").with_mods(Modifiers::CONTROL), "node.copy"),
            Binding::new(Pattern::key("v").with_mods(Modifiers::CONTROL), "node.paste"),
            // The view: the wheel, and `=`/`-` for a keyboard without a numpad.
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

/// The operator that opens [`screen_menu`].
pub const SCREEN_MENU: &str = "menu.screen";

/// The screen's operations, as a menu: each refused — and shown refused — where a
/// driver keeps no screen, or where the area cannot do it.
pub fn screen_menu() -> crate::Menu {
    use crate::ScreenAction;
    crate::Menu::new("Screen")
        .with("Split side by side", ScreenAction::SplitHorizontal.name(), Props::new())
        .with(
            "Split one above the other",
            ScreenAction::SplitVertical.name(),
            Props::new(),
        )
        .with("Join with neighbour", ScreenAction::Join.name(), Props::new())
        .with("Swap with neighbour", ScreenAction::Swap.name(), Props::new())
        .with("Maximize", ScreenAction::ToggleMaximize.name(), Props::new())
        .with("Detach into a window", ScreenAction::Detach.name(), Props::new())
        .with("New window", ScreenAction::NewWindow.name(), Props::new())
        .with("Sidebar", ScreenAction::ToggleSidebar.name(), Props::new())
        .with("Save", ScreenAction::SaveWorkspace.name(), Props::new())
        .with("Open", ScreenAction::LoadWorkspace.name(), Props::new())
}

/// The operator that opens the `Add` menu: one entry per kind the graph lists.
pub const ADD_MENU: &str = "menu.add";

/// Opens the `Add` menu — Blender's `Shift+A` — from [`NodeGraph::node_kinds`]. Refused
/// for a graph with no kinds.
#[derive(Default)]
pub struct AddMenuOp;

impl<G: NodeGraph> Operator<EditorWorld<G>> for AddMenuOp {
    fn name(&self) -> &'static str {
        ADD_MENU
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        !cx.world().graph.borrow().node_kinds().is_empty()
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        let kinds = cx.world().graph.borrow().node_kinds();
        let menu = kinds.into_iter().fold(crate::Menu::new("Add"), |menu, kind| {
            menu.with(kind.label, "node.add", Props::new().with_text("kind", kind.name))
        });
        cx.world_mut().menu = Some(menu);
        OpResult::Finished
    }
}

/// The editor's menu. Entries carry the properties the keymap binds them with, so each
/// shows its shortcut.
pub fn node_menu() -> crate::Menu {
    crate::Menu::new("Node")
        // With kinds, the second opens the list; without, it is refused and the first adds.
        .with("Add node", "node.add", Props::new())
        .with("Add", ADD_MENU, Props::new())
        .with("Delete", "node.delete", Props::new())
        // The binding's properties, so the entry shows `Shift+D`.
        .with("Duplicate", "node.duplicate", Props::new().with_bool(EDGE_PAN, true))
        .with("Copy", "node.copy", Props::new())
        .with("Paste", "node.paste", Props::new())
        .with("Link selected", "link.add", Props::new())
        .with("Box select", "node.box_select", Props::new())
        .with("Zoom in", "view.zoom", Props::new().with_float("factor", ZoomOp::STEP))
        .with(
            "Zoom out",
            "view.zoom",
            Props::new().with_float("factor", 1.0 / ZoomOp::STEP),
        )
        // A menu of their own: a menu does not scroll, and nine more would not fit.
        .with("Screen", SCREEN_MENU, Props::new())
        .with("Undo", "ed.undo", Props::new())
        .with("Redo", "ed.redo", Props::new())
}

/// A runtime with these operators registered and [`default_keymap`] in force.
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
    runtime.register(ResizeOp::default());
    runtime.register(PanOp::default());
    runtime.register(ZoomOp);
    runtime.register(LinkDragOp::default());
    runtime.register(crate::MenuOp::new(NODE_MENU, node_menu()));
    runtime.register(AddMenuOp);
    runtime.register(crate::MenuOp::new(SCREEN_MENU, screen_menu()));
    for action in crate::ScreenAction::ALL {
        runtime.register(crate::ScreenOp::new(action));
    }
    runtime.register(AddNodeOp);
    runtime.register(DeleteNodeOp);
    runtime.register(DuplicateOp::<G::Content>::default());
    runtime.register(CopyOp);
    runtime.register(PasteOp);
    runtime.register(AddLinkOp);
    runtime.register(DeleteLinkOp);
    runtime.register(UndoOp);
    runtime.register(RedoOp);
    runtime
}
