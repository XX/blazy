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

use crate::event::OpEvent;
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
