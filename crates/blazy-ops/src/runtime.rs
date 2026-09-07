//! The runtime: a registry, a keymap, a modal stack and the counters.
//!
//! Not a widget and not a host hook — a plain object a *driver* hands events to. §38
//! is the reason: Masonry has a place where an event can be seen before the tree
//! (`Layer::capture_pointer_event`) and a place where it can be kept from the tree
//! (pointer capture, obtainable only during a press), and they are not the same place.
//! Keeping the state out of both means a driver can sit in either, or in the host in
//! front of `RenderRoot`, without the operators knowing which.
//!
//! [`Seat`] is how a driver says where an event came from, and it is what turns "the
//! modal operator got the events" from an assertion into [`OpCounters::tree_first`].

use masonry::core::keyboard::Modifiers;
use masonry::kurbo::Point;
use masonry::ui_events::pointer::PointerButton;

use crate::event::{Device, OpEvent, Sample};
use crate::keymap::{Keymap, Props, Scope};
use crate::undo::{Step, UndoStack};
use crate::{OpCounters, OpResult, Operator};

/// Where an event reached the runtime from.
///
/// The three seats §38 measured, and the distinction is not academic: only one of
/// them can keep an event from the widget tree, and only the other two know what the
/// event is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seat {
    /// From the host, in front of `RenderRoot::handle_pointer_event`.
    ///
    /// Can withhold the event from the tree completely — and knows nothing about what
    /// is under it without paying `RenderRoot::edit_widget`, which runs the whole
    /// rewrite battery (§38.2).
    Host,
    /// From a layer root's pre-tree hook, which can also keep the event from the tree.
    ///
    /// The seat §38.1 measured as useless and this fork made real: `Layer::capture_pointer_event`
    /// returns `Handled` now, so this place has the host's power *and* knows what is
    /// under the pointer. It differs from [`Host`](Self::Host) in one way that matters
    /// here: the widget tree is underneath it and still wants its right of first
    /// refusal, so an idle runtime does not act from this seat (§39.3).
    Layer,
    /// From inside the widget tree, targeted at the driver itself.
    ///
    /// What pointer capture produces, and what focus produces for keys: nothing below
    /// the driver was offered the event.
    Tree,
    /// From inside the widget tree, after a descendant was offered it first.
    ///
    /// Ordinary bubbling. Legitimate when nothing is running; a leak while a modal
    /// operator is, which is what [`OpCounters::tree_first`] counts.
    Bubbled,
}

impl Seat {
    /// Whether this seat sees an event before the widget tree does, and can keep it.
    pub fn is_pre_tree(self) -> bool {
        matches!(self, Self::Host | Self::Layer)
    }
}

/// What a driver should do with the event it just offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    /// The runtime took it. A pre-tree driver keeps it from the widget tree; a driver
    /// inside the tree marks it handled.
    Consumed,
    /// Nothing wanted it. Pass it on.
    Pass,
}

impl Feed {
    pub fn is_consumed(self) -> bool {
        self == Self::Consumed
    }
}

/// A press being held while the runtime waits to see what it becomes.
#[derive(Clone, Copy, Debug)]
struct Pending {
    button: PointerButton,
    /// Where the press was, in the driver's space — what the operator will be given.
    pos: Point,
    /// Where the press was, in screen pixels — what the threshold is measured on.
    screen: Point,
    mods: Modifiers,
    time_ns: u64,
    device: Device,
}

/// The last click, for deciding whether the next one is a double.
#[derive(Clone, Copy, Debug)]
struct LastClick {
    button: PointerButton,
    screen: Point,
    time_ns: u64,
    count: u8,
}

/// What the runtime did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// An operator took it. The driver should mark the event handled.
    Consumed,
    /// A modal operator was running and deliberately let this one past.
    PassedThrough,
    /// Nothing in the keymap matched, or everything that matched refused to run.
    NoBinding,
}

impl Dispatch {
    /// Whether the driver should stop the event here.
    pub fn is_consumed(self) -> bool {
        self == Self::Consumed
    }
}

/// Everything an operator is allowed to touch.
pub struct OpCtx<'a, W> {
    world: &'a mut W,
    event: Option<&'a OpEvent>,
    props: &'a Props,
    scope: &'a [&'static str],
    undo: &'a mut UndoStack<W>,
}

impl<W> OpCtx<'_, W> {
    /// The application's world.
    pub fn world(&self) -> &W {
        self.world
    }

    /// The application's world, to change.
    pub fn world_mut(&mut self) -> &mut W {
        self.world
    }

    /// The event that brought us here, if there was one.
    ///
    /// `None` on the [`exec`](crate::Operator::exec) path, which is the point of that
    /// path: an operator that cannot state what it does without an event cannot be
    /// scripted, tested or redone.
    pub fn event(&self) -> Option<&OpEvent> {
        self.event
    }

    /// The operator's arguments, from the binding or from the caller.
    pub fn props(&self) -> &Props {
        self.props
    }

    /// The context chain the event happened in, innermost first.
    pub fn scope(&self) -> Scope<'_> {
        Scope(self.scope)
    }

    /// Records a step that undoes what this operator just did.
    pub fn push_undo(&mut self, step: Box<dyn Step<W>>) {
        self.undo.push(step);
    }

    /// Undoes the most recent step, for an operator that is the undo operator.
    pub fn undo(&mut self) -> Option<&'static str> {
        self.undo.undo(self.world)
    }

    /// Redoes the most recently undone step.
    pub fn redo(&mut self) -> Option<&'static str> {
        self.undo.redo(self.world)
    }

    /// The history, to ask how deep it is.
    pub fn history(&self) -> &UndoStack<W> {
        self.undo
    }
}

/// One operator on the modal stack.
struct Modal {
    op: usize,
    props: Props,
}

/// The registry, the keymap, the modal stack and the history.
pub struct OpRuntime<W> {
    ops: Vec<Option<Box<dyn Operator<W>>>>,
    names: Vec<&'static str>,
    keymap: Keymap,
    stack: Vec<Modal>,
    undo: UndoStack<W>,
    counters: OpCounters,
    /// A press whose meaning is not decided yet (§39.3).
    pending: Option<Pending>,
    last_click: Option<LastClick>,
}

impl<W> OpRuntime<W> {
    pub fn new(keymap: Keymap) -> Self {
        Self {
            ops: Vec::new(),
            names: Vec::new(),
            keymap,
            stack: Vec::new(),
            undo: UndoStack::new(),
            counters: OpCounters::default(),
            pending: None,
            last_click: None,
        }
    }

    /// Registers an operator under its own name.
    ///
    /// A later registration of the same name replaces the earlier one, which is how a
    /// test swaps one operator for a stub without rebuilding the keymap.
    pub fn register(&mut self, op: impl Operator<W> + 'static) {
        let name = op.name();
        match self.names.iter().position(|known| *known == name) {
            Some(at) => self.ops[at] = Some(Box::new(op)),
            None => {
                self.names.push(name);
                self.ops.push(Some(Box::new(op)));
            },
        }
    }

    /// The keymap in force.
    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Replaces the keymap. What a user override, a workspace or a test does.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
    }

    /// The history, so an application can show its depth.
    pub fn history(&self) -> &UndoStack<W> {
        &self.undo
    }

    /// The history, to set a ceiling on it.
    pub fn history_mut(&mut self) -> &mut UndoStack<W> {
        &mut self.undo
    }

    /// How many modal operators are running.
    ///
    /// Nested rather than one, because §11 asks for a stack and because a transform
    /// started inside a modal tool is exactly what a stack is for. Zero between
    /// gestures, and a sweep that ends with it non-zero has found a gesture that
    /// never ended.
    pub fn modal_depth(&self) -> usize {
        self.stack.len()
    }

    /// The name of the operator currently receiving events, if any.
    pub fn modal_name(&self) -> Option<&'static str> {
        self.stack.last().map(|modal| self.names[modal.op])
    }

    pub fn counters(&self) -> OpCounters {
        self.counters
    }

    /// What the pre-tree seat can do today: watch.
    ///
    /// `Layer::capture_pointer_event` is called for every pointer event before the
    /// target is even computed — including events outside the layer's own rectangle —
    /// and it has no way to stop one. Upstream's own TODO lists "return flag to
    /// suppress event from reaching children" as a possible evolution; until it
    /// exists this seat can count what it sees and nothing more (§38.1), which is
    /// worth doing because the count is the evidence for the request.
    pub fn observe(&mut self, event: &OpEvent) {
        // The event is taken rather than ignored so that the day the flag exists this
        // becomes `dispatch` and nothing else about the seat changes.
        let _ = event;
        self.counters.seen_first += 1;
    }

    /// Whether a press is being held, waiting to become a click or a drag.
    pub fn is_holding(&self) -> bool {
        self.pending.is_some()
    }

    /// Offers one pointer event to the runtime, and says what the driver should do with it.
    ///
    /// This is [`dispatch`](Self::dispatch) plus the one thing a keymap cannot express on
    /// its own: whether a press was a click or a drag. The decision is made here, from
    /// the events themselves, exactly as Blender's window manager makes it before its
    /// keymap sees anything (§39.2) — so a binding says `Trigger::Click` and never
    /// carries a property telling somebody else's operator what a click is.
    ///
    /// The order is the whole design, and it is chosen so that a wrong guess costs
    /// nothing (§39.3):
    ///
    /// 1. a running modal operator gets the event, as before;
    /// 2. a **held press** takes it: a move past the threshold turns into [`OpEvent::Drag`], a release inside it into
    ///    [`OpEvent::Click`];
    /// 3. otherwise the keymap is consulted, and a press that some `Click`/`Drag` binding could still want starts a
    ///    hold instead of doing anything.
    ///
    /// The press itself is **never** withheld from the widget tree: a hold starts only
    /// after the tree has refused it. That is what makes the scheme safe where §38.1
    /// said buffering could not be — an event held in error is a *move*, and a move that
    /// the tree missed costs a hover, not a lost click.
    pub fn feed(&mut self, world: &mut W, event: &OpEvent, sample: Sample, scope: Scope<'_>, seat: Seat) -> Feed {
        // Everything passes the pre-tree seat, whether or not it stops there. Counted
        // first so that the number keeps meaning "what this seat saw" now that the seat
        // also acts.
        if seat == Seat::Layer {
            self.counters.seen_first += 1;
        }

        // A running operator owns the gesture wherever the event came from.
        if !self.stack.is_empty() {
            let consumed = self.dispatch(world, event, scope, seat).is_consumed();
            return self.answer(seat, consumed);
        }

        if self.pending.is_some()
            && let Some(feed) = self.resolve(world, event, sample, scope, seat)
        {
            return feed;
        }

        // Idle, and underneath this seat there is a widget tree that has not been asked
        // yet. A slider inside a node has the right of first refusal (§20 claim 3), so
        // this seat only counts; the bubbled route below will bring the event back.
        if seat == Seat::Layer {
            return Feed::Pass;
        }

        let consumed = self.dispatch(world, event, scope, seat).is_consumed();
        if consumed {
            return self.answer(seat, true);
        }

        // Nothing fired on the press itself. If some binding on this button is waiting
        // for a click or a drag, hold it and find out which.
        if let OpEvent::Press { button, pos, mods } = event
            && self.would_hold(world, *button, event, scope)
        {
            self.pending = Some(Pending {
                button: *button,
                pos: *pos,
                screen: sample.screen,
                mods: *mods,
                time_ns: sample.time_ns,
                device: sample.device,
            });
            self.counters.holds += 1;
            // Consumed rather than passed: the press belongs to a gesture now, and a
            // driver inside the tree marks it handled so nothing above acts on it too.
            return self.answer(seat, true);
        }

        Feed::Pass
    }

    /// Feeds the held press the event that may resolve it.
    ///
    /// `None` means "not for me": the hold is abandoned and the caller carries on with
    /// an ordinary dispatch. Abandoning is cheap by construction — the press went to the
    /// tree when it happened, so nothing is owed to anyone.
    fn resolve(
        &mut self,
        world: &mut W,
        event: &OpEvent,
        sample: Sample,
        scope: Scope<'_>,
        seat: Seat,
    ) -> Option<Feed> {
        let pending = self.pending?;
        match event {
            OpEvent::Move { .. } => {
                let travel = (sample.screen - pending.screen).hypot();
                if travel < self.keymap.thresholds().drag_for(pending.device) {
                    // Still undecided. The move is kept from the tree, which is the
                    // whole saving: a canvas that never sees it never picks on it.
                    return Some(self.answer(seat, true));
                }
                self.pending = None;
                self.counters.drags += 1;
                let drag = OpEvent::Drag {
                    button: pending.button,
                    pos: pending.pos,
                    screen: pending.screen,
                    mods: pending.mods,
                };
                let started = self.dispatch(world, &drag, scope, seat).is_consumed();
                // The move that resolved it goes to whatever started, or the drag would
                // lag one event behind the pointer for the rest of its life.
                if started && !self.stack.is_empty() {
                    self.dispatch(world, event, scope, seat);
                }
                Some(self.answer(seat, started))
            },
            OpEvent::Release { button, .. } if *button == pending.button => {
                self.pending = None;
                let count = self.click_count(&pending, sample);
                self.counters.clicks += 1;
                if count >= 2 {
                    self.counters.double_clicks += 1;
                }
                let click = OpEvent::Click {
                    button: pending.button,
                    pos: pending.pos,
                    screen: pending.screen,
                    mods: pending.mods,
                    count,
                };
                self.dispatch(world, &click, scope, seat);
                // The release is *not* withheld: the tree saw the press, and a widget
                // that saw a press and never sees its release is a widget stuck down.
                Some(Feed::Pass)
            },
            // Another button, a key, a cancel: this press is not going to resolve.
            _ => {
                self.pending = None;
                self.counters.holds_abandoned += 1;
                None
            },
        }
    }

    /// Whether the second click of a double click is what we have.
    fn click_count(&mut self, pending: &Pending, sample: Sample) -> u8 {
        let thresholds = self.keymap.thresholds();
        let window_ns = thresholds.double_click_ms.saturating_mul(1_000_000);
        let count = match self.last_click {
            Some(last)
                if last.button == pending.button
                    && pending.time_ns.saturating_sub(last.time_ns) <= window_ns
                    && (pending.screen - last.screen).hypot() <= thresholds.double_click_slop =>
            {
                last.count.saturating_add(1)
            },
            _ => 1,
        };
        self.last_click = Some(LastClick {
            button: pending.button,
            screen: pending.screen,
            time_ns: sample.time_ns.max(pending.time_ns),
            count,
        });
        count
    }

    /// Whether any binding on `button` is waiting for a click or a drag, and would run.
    ///
    /// The poll is asked here, at the press, and asked **again** when the gesture
    /// resolves — the answers can differ, because the pointer has moved in between, and
    /// the second one is the one that decides whether an operator runs. A yes here that
    /// turns into a no there costs a held gesture that does nothing; that is the whole
    /// price of asking early, and it is paid in moves the tree did not see (§39.4).
    fn would_hold(&mut self, world: &mut W, button: PointerButton, press: &OpEvent, scope: Scope<'_>) -> bool {
        let candidates: Vec<usize> = self
            .keymap
            .sections_for(scope)
            .filter(|binding| binding.pattern.wants_gesture(button))
            .filter_map(|binding| self.names.iter().position(|name| *name == binding.op))
            .collect();

        candidates.into_iter().any(|at| {
            let op = self.ops[at].take().expect("an operator is not running twice");
            let cx = OpCtx {
                world,
                event: Some(press),
                props: &Props::new(),
                scope: scope.0,
                undo: &mut self.undo,
            };
            self.counters.polled += 1;
            let allowed = op.poll(&cx);
            if !allowed {
                self.counters.refused += 1;
            }
            self.ops[at] = Some(op);
            allowed
        })
    }

    /// Records what a pre-tree seat kept from the tree, and answers the driver.
    fn answer(&mut self, seat: Seat, consumed: bool) -> Feed {
        if consumed {
            if seat.is_pre_tree() {
                self.counters.withheld += 1;
            }
            Feed::Consumed
        } else {
            Feed::Pass
        }
    }

    /// Offers an event to the modal operator, or to the keymap.
    pub fn dispatch(&mut self, world: &mut W, event: &OpEvent, scope: Scope<'_>, seat: Seat) -> Dispatch {
        if seat == Seat::Bubbled && !self.stack.is_empty() {
            // The tree was offered an event that belongs to a running operator. Not a
            // hypothetical: it is what happens to every modal operator started from a
            // key, because pointer capture can only be taken during a press (§38.1).
            self.counters.tree_first += 1;
        }

        if !self.stack.is_empty() {
            return self.deliver_modal(world, event, scope);
        }

        self.counters.lookups += 1;
        let candidates: Vec<(usize, Props)> = self
            .keymap
            .matches(scope, event)
            .filter_map(|binding| {
                self.names
                    .iter()
                    .position(|name| *name == binding.op)
                    .map(|at| (at, binding.props.clone()))
            })
            .collect();
        if candidates.is_empty() {
            return Dispatch::NoBinding;
        }
        self.counters.matched += 1;

        for (at, props) in candidates {
            match self.run(world, at, Some(event), &props, scope, Path::Invoke) {
                Some(OpResult::Running) => {
                    self.counters.modal_starts += 1;
                    self.stack.push(Modal { op: at, props });
                    return Dispatch::Consumed;
                },
                Some(OpResult::Finished | OpResult::Cancelled) => return Dispatch::Consumed,
                // The operator matched and declined the event: the next binding gets
                // its turn, which is what makes a keymap layered rather than a switch.
                Some(OpResult::PassThrough) | None => {},
            }
        }
        Dispatch::NoBinding
    }

    /// Runs an operator by name, from a script, a test or a redo.
    ///
    /// The same poll, the same context, the same history as the interactive path —
    /// only the event is missing. That is what makes "the key and the script do the
    /// same thing" a claim a test can check by comparing state.
    pub fn exec(&mut self, world: &mut W, name: &str, props: &Props) -> OpResult {
        let Some(at) = self.names.iter().position(|known| *known == name) else {
            return OpResult::PassThrough;
        };
        let props = props.clone();
        match self.run(world, at, None, &props, Scope(&[]), Path::Exec) {
            Some(OpResult::Running) => {
                self.counters.modal_starts += 1;
                self.stack.push(Modal { op: at, props });
                OpResult::Running
            },
            Some(result) => result,
            None => OpResult::Cancelled,
        }
    }

    /// Cancels every running operator, innermost first.
    ///
    /// What a window losing focus, or a driver being torn down, has to do: a modal
    /// stack with no way to empty it is a hang with extra steps.
    pub fn cancel_all(&mut self, world: &mut W) {
        while let Some(modal) = self.stack.pop() {
            let props = modal.props;
            let mut op = self.ops[modal.op].take().expect("an operator is not running twice");
            let mut cx = OpCtx {
                world,
                event: None,
                props: &props,
                scope: &[],
                undo: &mut self.undo,
            };
            let _ = op.modal(&mut cx);
            // Whatever it answers, it is off the stack: this is the caller saying the
            // gesture is over, not the operator being asked whether it would like to
            // continue.
            self.ops[modal.op] = Some(op);
            self.counters.modal_cancels += 1;
        }
    }

    /// Hands one event to the operator on top of the stack.
    fn deliver_modal(&mut self, world: &mut W, event: &OpEvent, scope: Scope<'_>) -> Dispatch {
        let modal = self.stack.last().expect("called with a non-empty stack");
        let at = modal.op;
        let props = modal.props.clone();
        self.counters.modal_events += 1;

        let mut op = self.ops[at].take().expect("an operator is not running twice");
        let mut cx = OpCtx {
            world,
            event: Some(event),
            props: &props,
            scope: scope.0,
            undo: &mut self.undo,
        };
        let result = op.modal(&mut cx);
        self.ops[at] = Some(op);

        match result {
            OpResult::Running => Dispatch::Consumed,
            OpResult::Finished => {
                self.stack.pop();
                self.counters.modal_finishes += 1;
                Dispatch::Consumed
            },
            OpResult::Cancelled => {
                self.stack.pop();
                self.counters.modal_cancels += 1;
                Dispatch::Consumed
            },
            OpResult::PassThrough => {
                self.counters.passthrough += 1;
                Dispatch::PassedThrough
            },
        }
    }

    /// Polls an operator and runs it, or does neither.
    ///
    /// Returns `None` when the poll refused. Both entry points come through here, so
    /// there is one place where "was the poll asked" can be true or false — which is
    /// what [`OpCounters::unpolled`] is a criterion on.
    fn run(
        &mut self,
        world: &mut W,
        at: usize,
        event: Option<&OpEvent>,
        props: &Props,
        scope: Scope<'_>,
        path: Path,
    ) -> Option<OpResult> {
        let mut op = self.ops[at].take().expect("an operator is not running twice");
        let mut cx = OpCtx {
            world,
            event,
            props,
            scope: scope.0,
            undo: &mut self.undo,
        };

        self.counters.polled += 1;
        let allowed = op.poll(&cx);
        let result = if allowed {
            match path {
                Path::Invoke => {
                    self.counters.invoked += 1;
                    Some(op.invoke(&mut cx))
                },
                Path::Exec => {
                    self.counters.executed += 1;
                    Some(op.exec(&mut cx))
                },
            }
        } else {
            self.counters.refused += 1;
            None
        };

        self.ops[at] = Some(op);
        result
    }
}

/// Which of the two entry points a run came through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    Invoke,
    Exec,
}

#[cfg(test)]
mod tests {
    use masonry::core::keyboard::Modifiers;
    use masonry::kurbo::Point;
    use masonry::ui_events::pointer::PointerButton;

    use super::*;
    use crate::event::Pattern;
    use crate::keymap::Binding;

    #[derive(Default, Debug, PartialEq)]
    struct World {
        value: i64,
        running_saw: usize,
        available: bool,
    }

    /// Adds `by` to the world. The scriptable kind: its whole input is its properties.
    struct Add;

    impl Operator<World> for Add {
        fn name(&self) -> &'static str {
            "test.add"
        }
        fn poll(&self, cx: &OpCtx<'_, World>) -> bool {
            cx.world().available
        }
        fn invoke(&mut self, cx: &mut OpCtx<'_, World>) -> OpResult {
            let by = cx.props().int("by", 1);
            cx.world_mut().value += by;
            OpResult::Finished
        }
    }

    /// A modal operator: counts the events it is given, ends on a release.
    struct Grab;

    impl Operator<World> for Grab {
        fn name(&self) -> &'static str {
            "test.grab"
        }
        fn invoke(&mut self, _cx: &mut OpCtx<'_, World>) -> OpResult {
            OpResult::Running
        }
        fn modal(&mut self, cx: &mut OpCtx<'_, World>) -> OpResult {
            cx.world_mut().running_saw += 1;
            match cx.event() {
                Some(OpEvent::Release { .. }) => OpResult::Finished,
                _ => OpResult::Running,
            }
        }
    }

    fn runtime() -> OpRuntime<World> {
        let keymap = crate::keymap::Keymap::new().with("test", vec![
            Binding::new(Pattern::key("a"), "test.add").with_props(Props::new().with_int("by", 2)),
            Binding::new(Pattern::press(PointerButton::Primary), "test.grab"),
        ]);
        let mut runtime = OpRuntime::new(keymap);
        runtime.register(Add);
        runtime.register(Grab);
        runtime
    }

    fn key(name: &str) -> OpEvent {
        OpEvent::Key {
            key: masonry::core::keyboard::Key::Character(name.into()),
            mods: Modifiers::empty(),
            down: true,
        }
    }

    fn press() -> OpEvent {
        OpEvent::Press {
            button: PointerButton::Primary,
            pos: Point::ORIGIN,
            mods: Modifiers::empty(),
        }
    }

    fn release() -> OpEvent {
        OpEvent::Release {
            button: PointerButton::Primary,
            pos: Point::ORIGIN,
            mods: Modifiers::empty(),
        }
    }

    const SCOPE: Scope<'static> = Scope(&["test"]);

    /// What a gesture-driven world records, so a test can ask what the runtime decided.
    #[derive(Default, Debug, PartialEq)]
    struct Gestures {
        clicks: Vec<u8>,
        drags: usize,
        /// Where the last gesture began, as the operator was told.
        origin: Point,
    }

    struct ClickOp;

    impl Operator<Gestures> for ClickOp {
        fn name(&self) -> &'static str {
            "test.click"
        }
        fn invoke(&mut self, cx: &mut OpCtx<'_, Gestures>) -> OpResult {
            let (count, pos) = match cx.event() {
                Some(OpEvent::Click { count, pos, .. }) => (*count, *pos),
                _ => (0, Point::ORIGIN),
            };
            cx.world_mut().clicks.push(count);
            cx.world_mut().origin = pos;
            OpResult::Finished
        }
    }

    struct DragOp;

    impl Operator<Gestures> for DragOp {
        fn name(&self) -> &'static str {
            "test.drag"
        }
        fn invoke(&mut self, cx: &mut OpCtx<'_, Gestures>) -> OpResult {
            if let Some(OpEvent::Drag { pos, .. }) = cx.event() {
                cx.world_mut().origin = *pos;
            }
            cx.world_mut().drags += 1;
            OpResult::Running
        }
        fn modal(&mut self, cx: &mut OpCtx<'_, Gestures>) -> OpResult {
            match cx.event() {
                Some(OpEvent::Release { .. }) => OpResult::Finished,
                _ => OpResult::Running,
            }
        }
    }

    fn gesture_runtime() -> OpRuntime<Gestures> {
        let keymap = crate::keymap::Keymap::new().with("test", vec![
            Binding::new(Pattern::drag(PointerButton::Primary), "test.drag"),
            Binding::new(Pattern::click(PointerButton::Primary), "test.click"),
        ]);
        let mut runtime = OpRuntime::new(keymap);
        runtime.register(ClickOp);
        runtime.register(DragOp);
        runtime
    }

    fn at(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    fn sample(x: f64, y: f64, ms: u64, device: Device) -> Sample {
        Sample {
            time_ns: ms * 1_000_000,
            device,
            screen: at(x, y),
        }
    }

    fn press_at(x: f64, y: f64) -> OpEvent {
        OpEvent::Press {
            button: PointerButton::Primary,
            pos: at(x, y),
            mods: Modifiers::empty(),
        }
    }

    fn move_to(x: f64, y: f64) -> OpEvent {
        OpEvent::Move {
            pos: at(x, y),
            mods: Modifiers::empty(),
        }
    }

    fn release_at(x: f64, y: f64) -> OpEvent {
        OpEvent::Release {
            button: PointerButton::Primary,
            pos: at(x, y),
            mods: Modifiers::empty(),
        }
    }

    /// A press and a release in the same place is a click, and the operator is told
    /// where the *press* was.
    #[test]
    fn a_press_that_does_not_travel_is_a_click() {
        let mut runtime = gesture_runtime();
        let mut world = Gestures::default();

        let held = runtime.feed(
            &mut world,
            &press_at(10.0, 10.0),
            sample(10.0, 10.0, 0, Device::Mouse),
            SCOPE,
            Seat::Host,
        );
        assert_eq!(held, Feed::Consumed, "the press is held, not acted on");
        assert!(runtime.is_holding());
        assert_eq!(world, Gestures::default(), "and nothing has run yet");

        runtime.feed(
            &mut world,
            &move_to(11.0, 10.0),
            sample(11.0, 10.0, 10, Device::Mouse),
            SCOPE,
            Seat::Host,
        );
        assert!(runtime.is_holding(), "a pixel is not a drag");

        runtime.feed(
            &mut world,
            &release_at(11.0, 10.0),
            sample(11.0, 10.0, 20, Device::Mouse),
            SCOPE,
            Seat::Host,
        );
        assert_eq!(world.clicks, vec![1]);
        assert_eq!(world.origin, at(10.0, 10.0), "the click is where the press was");
        assert_eq!(world.drags, 0);
        assert!(!runtime.is_holding());
    }

    /// Travel past the threshold makes it a drag, and the drag also starts from the
    /// press: the pixels it took to notice are part of the movement.
    #[test]
    fn a_press_that_travels_is_a_drag() {
        let mut runtime = gesture_runtime();
        let mut world = Gestures::default();
        runtime.feed(
            &mut world,
            &press_at(10.0, 10.0),
            sample(10.0, 10.0, 0, Device::Mouse),
            SCOPE,
            Seat::Host,
        );
        runtime.feed(
            &mut world,
            &move_to(30.0, 10.0),
            sample(30.0, 10.0, 10, Device::Mouse),
            SCOPE,
            Seat::Host,
        );

        assert_eq!(world.drags, 1);
        assert_eq!(world.origin, at(10.0, 10.0), "the drag begins at the press");
        assert!(world.clicks.is_empty(), "and it is not also a click");
        assert_eq!(runtime.modal_depth(), 1);

        runtime.feed(
            &mut world,
            &release_at(30.0, 10.0),
            sample(30.0, 10.0, 20, Device::Mouse),
            SCOPE,
            Seat::Host,
        );
        assert_eq!(runtime.modal_depth(), 0);
        assert!(world.clicks.is_empty(), "a drag never ends in a click");
    }

    /// The same travel, two devices, two answers. A finger that has moved 8 px has not
    /// decided anything yet; a mouse that has moved 8 px has.
    #[test]
    fn the_drag_threshold_follows_the_device() {
        for (device, drags) in [(Device::Mouse, 1), (Device::Touch, 0)] {
            let mut runtime = gesture_runtime();
            let mut world = Gestures::default();
            runtime.feed(
                &mut world,
                &press_at(0.0, 0.0),
                sample(0.0, 0.0, 0, device),
                SCOPE,
                Seat::Host,
            );
            runtime.feed(
                &mut world,
                &move_to(8.0, 0.0),
                sample(8.0, 0.0, 10, device),
                SCOPE,
                Seat::Host,
            );
            assert_eq!(world.drags, drags, "{device:?} at 8 px");
        }
    }

    /// Two clicks close in time and place are a double click; either one apart is not.
    #[test]
    fn a_double_click_is_close_in_both_time_and_place() {
        let click = |runtime: &mut OpRuntime<Gestures>, world: &mut Gestures, x: f64, ms: u64| {
            runtime.feed(
                world,
                &press_at(x, 0.0),
                sample(x, 0.0, ms, Device::Mouse),
                SCOPE,
                Seat::Host,
            );
            runtime.feed(
                world,
                &release_at(x, 0.0),
                sample(x, 0.0, ms + 5, Device::Mouse),
                SCOPE,
                Seat::Host,
            );
        };

        let mut runtime = gesture_runtime();
        let mut world = Gestures::default();
        click(&mut runtime, &mut world, 0.0, 0);
        click(&mut runtime, &mut world, 0.0, 100);
        assert_eq!(world.clicks, vec![1, 2], "same place, inside the window");

        let mut world = Gestures::default();
        click(&mut runtime, &mut world, 0.0, 1_000);
        click(&mut runtime, &mut world, 0.0, 2_000);
        assert_eq!(world.clicks, vec![1, 1], "too slow");

        let mut world = Gestures::default();
        click(&mut runtime, &mut world, 0.0, 3_000);
        click(&mut runtime, &mut world, 40.0, 3_050);
        assert_eq!(world.clicks, vec![1, 1], "too far");
    }

    /// A press nothing is waiting for is not held: with no click or drag binding on
    /// that button there is nothing to find out.
    #[test]
    fn a_press_no_binding_wants_is_not_held() {
        let mut runtime = gesture_runtime();
        let mut world = Gestures::default();
        let event = OpEvent::Press {
            button: PointerButton::Secondary,
            pos: at(0.0, 0.0),
            mods: Modifiers::empty(),
        };
        let feed = runtime.feed(
            &mut world,
            &event,
            sample(0.0, 0.0, 0, Device::Mouse),
            SCOPE,
            Seat::Host,
        );
        assert_eq!(feed, Feed::Pass);
        assert!(!runtime.is_holding());
    }

    /// A held press that is interrupted resolves into nothing, and takes nothing with
    /// it: the press already went to the tree when it happened.
    #[test]
    fn an_interrupted_hold_is_abandoned() {
        let mut runtime = gesture_runtime();
        let mut world = Gestures::default();
        runtime.feed(
            &mut world,
            &press_at(0.0, 0.0),
            sample(0.0, 0.0, 0, Device::Mouse),
            SCOPE,
            Seat::Host,
        );
        assert!(runtime.is_holding());

        runtime.feed(&mut world, &key("a"), Sample::default(), SCOPE, Seat::Host);
        assert!(!runtime.is_holding());
        assert_eq!(world, Gestures::default(), "no click, no drag, nothing");
        assert_eq!(runtime.counters().holds_abandoned, 1);
    }

    /// The seat decides who may act, and the layer seat may not act on an idle runtime:
    /// underneath it there is a widget tree that has not been asked yet.
    #[test]
    fn the_layer_seat_does_not_act_before_the_tree() {
        let mut runtime = gesture_runtime();
        let mut world = Gestures::default();
        let feed = runtime.feed(
            &mut world,
            &press_at(0.0, 0.0),
            sample(0.0, 0.0, 0, Device::Mouse),
            SCOPE,
            Seat::Layer,
        );
        assert_eq!(feed, Feed::Pass);
        assert!(!runtime.is_holding(), "the bubbled route will bring it back");
        assert_eq!(runtime.counters().seen_first, 1, "it did see it");
    }

    /// The claim §11 is built on: one operator, two ways in, one result.
    #[test]
    fn a_key_and_a_script_do_the_same_thing() {
        let mut runtime = runtime();
        let mut from_key = World {
            available: true,
            ..World::default()
        };
        runtime.dispatch(&mut from_key, &key("a"), SCOPE, Seat::Tree);

        let mut from_script = World {
            available: true,
            ..World::default()
        };
        runtime.exec(&mut from_script, "test.add", &Props::new().with_int("by", 2));

        assert_eq!(from_key, from_script);
        assert_eq!(from_key.value, 2);
    }

    /// Poll is asked on both paths, and refusing means the world is untouched.
    #[test]
    fn poll_refuses_on_both_paths() {
        let mut runtime = runtime();
        let mut world = World::default();
        assert_eq!(
            runtime.dispatch(&mut world, &key("a"), SCOPE, Seat::Tree),
            Dispatch::NoBinding
        );
        assert_eq!(runtime.exec(&mut world, "test.add", &Props::new()), OpResult::Cancelled);
        assert_eq!(world.value, 0);
        let counters = runtime.counters();
        assert_eq!(counters.refused, 2);
        assert_eq!(counters.invoked, 0);
        assert_eq!(counters.executed, 0);
        assert_eq!(counters.unpolled, 0);
    }

    /// A modal operator gets the events until it says otherwise, and the stack empties.
    #[test]
    fn a_modal_operator_keeps_the_events_until_it_finishes() {
        let mut runtime = runtime();
        let mut world = World::default();
        assert!(runtime.dispatch(&mut world, &press(), SCOPE, Seat::Tree).is_consumed());
        assert_eq!(runtime.modal_depth(), 1);
        assert_eq!(runtime.modal_name(), Some("test.grab"));

        for _ in 0..3 {
            let moved = OpEvent::Move {
                pos: Point::ORIGIN,
                mods: Modifiers::empty(),
            };
            assert!(runtime.dispatch(&mut world, &moved, SCOPE, Seat::Tree).is_consumed());
        }
        // A key the keymap knows goes to the running operator, not to the keymap:
        // that is what modality means.
        assert!(runtime.dispatch(&mut world, &key("a"), SCOPE, Seat::Tree).is_consumed());
        assert_eq!(world.value, 0, "the keymap did not get a look in");

        assert!(
            runtime
                .dispatch(&mut world, &release(), SCOPE, Seat::Tree)
                .is_consumed()
        );
        assert_eq!(runtime.modal_depth(), 0);
        assert_eq!(world.running_saw, 5);
        assert_eq!(runtime.counters().modal_finishes, 1);
    }

    /// The leak counter: an event that reached the tree first while an operator was
    /// running. Counted rather than prevented, because today it cannot be prevented
    /// for an operator that started from a key (§38.1).
    #[test]
    fn an_event_that_reached_the_tree_first_is_counted() {
        let mut runtime = runtime();
        let mut world = World::default();
        runtime.dispatch(&mut world, &press(), SCOPE, Seat::Tree);
        assert_eq!(runtime.counters().tree_first, 0);

        let moved = OpEvent::Move {
            pos: Point::ORIGIN,
            mods: Modifiers::empty(),
        };
        runtime.dispatch(&mut world, &moved, SCOPE, Seat::Bubbled);
        assert_eq!(runtime.counters().tree_first, 1);

        // …and an event bubbling with nothing running is not a leak, it is ordinary.
        runtime.dispatch(&mut world, &release(), SCOPE, Seat::Tree);
        runtime.dispatch(&mut world, &moved, SCOPE, Seat::Bubbled);
        assert_eq!(runtime.counters().tree_first, 1);
    }

    /// A gesture that never ends is a hang; the driver has to be able to end it.
    #[test]
    fn cancel_all_empties_the_stack() {
        let mut runtime = runtime();
        let mut world = World::default();
        runtime.dispatch(&mut world, &press(), SCOPE, Seat::Tree);
        assert_eq!(runtime.modal_depth(), 1);
        runtime.cancel_all(&mut world);
        assert_eq!(runtime.modal_depth(), 0);
        assert_eq!(runtime.counters().modal_cancels, 1);
    }

    /// The keymap is data, so rebinding is data too — no operator changes.
    #[test]
    fn rebinding_changes_which_operator_runs() {
        let mut runtime = runtime();
        let mut world = World {
            available: true,
            ..World::default()
        };
        runtime.set_keymap(crate::keymap::Keymap::new().with("test", vec![
            Binding::new(Pattern::key("q"), "test.add").with_props(Props::new().with_int("by", 7)),
        ]));
        runtime.dispatch(&mut world, &key("a"), SCOPE, Seat::Tree);
        assert_eq!(world.value, 0, "the old binding is gone");
        runtime.dispatch(&mut world, &key("q"), SCOPE, Seat::Tree);
        assert_eq!(world.value, 7);
    }
}
