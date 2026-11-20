//! Operators, keymap and modality.
//!
//! An *operator* is one named unit of user-visible work: run from a key, a button, a
//! menu or a script, it says whether it can run now, and while it is *modal* it receives
//! events instead of the widget under the pointer. The keymap is data: a context, an
//! event pattern, an operator name and its properties.
//!
//! Not a widget: the state lives in a plain object ([`runtime::OpRuntime`]) that a driver
//! hands events to, from whichever seat it sits in ([`runtime::Seat`]). So the same
//! operators run in a test without widgets, and a key and a script do the same thing.
//!
//! ## The shape of a use
//!
//! ```
//! use blazy_ops::event::{OpEvent, Pattern};
//! use blazy_ops::keymap::{Binding, Keymap, Props, Scope};
//! use blazy_ops::runtime::{OpCtx, OpRuntime, Seat};
//! use blazy_ops::{OpResult, Operator};
//! use masonry::core::keyboard::{Key, Modifiers};
//!
//! // The world is the application's: the model, the selection, nothing of ours.
//! #[derive(Default)]
//! struct World {
//!     dx: f64,
//! }
//!
//! struct MoveOp;
//! impl Operator<World> for MoveOp {
//!     fn name(&self) -> &'static str {
//!         "node.move"
//!     }
//!     fn invoke(&mut self, cx: &mut OpCtx<'_, World>) -> OpResult {
//!         cx.world_mut().dx += cx.props().float("dx", 0.0);
//!         OpResult::Finished
//!     }
//! }
//!
//! // The keymap is data: a context, a pattern, an operator's name and its arguments.
//! let keymap = Keymap::new().with("canvas", vec![
//!     Binding::new(Pattern::key("g"), "node.move").with_props(Props::new().with_float("dx", 30.0)),
//! ]);
//! let mut ops = OpRuntime::new(keymap);
//! ops.register(MoveOp);
//!
//! let mut world = World::default();
//! // …from a widget's event handler:
//! let key = OpEvent::Key {
//!     key: Key::Character("g".into()),
//!     mods: Modifiers::empty(),
//!     down: true,
//! };
//! ops.dispatch(&mut world, &key, Scope(&["canvas"]), Seat::Bubbled);
//! // …and from a test or a script, through the same poll and the same history:
//! ops.exec(&mut world, "node.move", &Props::new().with_float("dx", 30.0));
//! assert_eq!(world.dx, 60.0);
//! ```
//!
//! The world type `W` is the application's: everything an operator may touch, and no
//! widgets.

#![warn(missing_docs, unreachable_pub)]

pub mod event;
mod keyfile;
pub mod keymap;
pub mod runtime;
pub mod undo;

use crate::runtime::OpCtx;

/// What an operator did with the event it was given, as in Blender.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpResult {
    /// The operator is modal: it stays on the stack and gets the following events.
    Running,
    /// Done. Whatever it changed stays changed.
    Finished,
    /// Aborted. The operator must have put the world back itself.
    Cancelled,
    /// Not this operator's event. The runtime keeps looking, and a modal operator
    /// returning this stays on the stack.
    PassThrough,
}

/// One named unit of user-visible work, over the application's world `W`.
pub trait Operator<W> {
    /// The name the keymap, a menu or a script refers to this operator by.
    fn name(&self) -> &'static str;

    /// Whether the operator can run right now; asked before every run, on both paths, once
    /// per candidate binding. Keep it cheap.
    fn poll(&self, cx: &OpCtx<'_, W>) -> bool {
        let _ = cx;
        true
    }

    /// Runs the operator from the interface, with the event that started it.
    fn invoke(&mut self, cx: &mut OpCtx<'_, W>) -> OpResult;

    /// Runs the operator from its properties alone (a script, a menu, a test). Defaults to
    /// [`invoke`](Self::invoke); an operator reading the pointer overrides it.
    fn exec(&mut self, cx: &mut OpCtx<'_, W>) -> OpResult {
        self.invoke(cx)
    }

    /// Handles one event while this operator is on the modal stack.
    ///
    /// The event is in `cx.event()`. Returning [`OpResult::Running`] keeps the
    /// operator on the stack; anything else takes it off.
    ///
    /// **No event means the gesture was cut off**
    /// ([`OpRuntime::cancel_all`](runtime::OpRuntime::cancel_all)): the operator leaves the
    /// stack whatever it answers, and must clean up now. Answering `Running` is counted
    /// ([`OpCounters::cut_off_running`]).
    fn modal(&mut self, cx: &mut OpCtx<'_, W>) -> OpResult {
        let _ = cx;
        OpResult::Finished
    }
}

/// Cumulative counters, for spotting work — and leaks — that should not be happening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpCounters {
    /// Times a poll was asked.
    pub polled: u64,
    /// Times a poll said no, and the operator did not run.
    pub refused: u64,
    /// Times an operator was actually invoked from the interface.
    pub invoked: u64,
    /// Times an operator was executed from a script, a test or a redo.
    pub executed: u64,
    /// Operators that ran without their poll being asked; zero by construction.
    pub unpolled: u64,
    /// Events delivered to a modal operator.
    pub modal_events: u64,
    /// Modal operators started.
    pub modal_starts: u64,
    /// Modal operators that ran to completion.
    pub modal_finishes: u64,
    /// Modal operators that were cancelled.
    pub modal_cancels: u64,
    /// Operators cut off by [`OpRuntime::cancel_all`](runtime::OpRuntime::cancel_all)
    /// that answered as if the gesture went on (and so did not clean up).
    pub cut_off_running: u64,
    /// Events a modal operator deliberately let past.
    pub passthrough: u64,
    /// Events that reached the widget tree before the runtime while a modal operator was
    /// running: the leak counter.
    pub tree_first: u64,
    /// Events the pre-tree seat saw, whether or not it was allowed to act on them.
    pub seen_first: u64,
    /// Events a pre-tree seat kept from the widget tree.
    pub withheld: u64,
    /// Presses held while the runtime waited to see what they became.
    pub holds: u64,
    /// Held presses that resolved into neither a click nor a drag, and were dropped.
    pub holds_abandoned: u64,
    /// Presses that turned out to be clicks.
    pub clicks: u64,
    /// Clicks that were the second of a double click.
    pub double_clicks: u64,
    /// Presses that turned out to be drags.
    pub drags: u64,
    /// Keymap lookups.
    pub lookups: u64,
    /// Keymap lookups that found at least one binding.
    pub matched: u64,
    /// Undo steps applied.
    pub undone: u64,
    /// Redo steps applied.
    pub redone: u64,
}
