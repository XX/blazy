//! Operators, keymap and modality — the mechanism behind `rnd/architecture.md` §11.
//!
//! An *operator* is one named unit of user-visible work. It can be run from a key,
//! from a button, from a test or from a script, it says for itself whether it is
//! available right now, and while it is *modal* it receives events instead of the
//! widget under the pointer. The keymap that starts it is **data**: a context, an
//! event pattern, an operator name and its properties.
//!
//! ## What this crate is not
//!
//! It is not a widget, and it does not know Masonry's routing exists. That is
//! deliberate and it is the finding of §38: there is no single seat in Masonry from
//! which a layer can both see an event before the tree *and* keep it from the tree.
//! So the state — the registry, the keymap, the modal stack, the undo journal — lives
//! in a plain object that a driver hands events to, and a driver may sit in the widget
//! tree, in a layer's `capture_pointer_event` hook, or in the host in front of
//! `RenderRoot::handle_pointer_event`. [`runtime::Seat`] is how the runtime is told
//! which one an event came through, and it is what makes "the tree saw it first"
//! countable rather than arguable.
//!
//! The consequence for a caller is the useful part: the same operators run in a test
//! with no widgets at all. [`Operator::exec`] and [`Operator::invoke`] act on the same
//! world through the same context, so "the key and the script do the same thing" is a
//! claim a test can check by comparing state rather than pixels.
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
//! The world type `W` is the application's: everything an operator may touch goes
//! through it, and nothing else does. Widgets cannot be touched from an operator at
//! all — an operator changes the model and leaves the driver to carry the consequences
//! into the tree (§30 already requires the model to be the truth).

#![warn(missing_docs, unreachable_pub)]

pub mod event;
mod keyfile;
pub mod keymap;
pub mod runtime;
pub mod undo;

use crate::runtime::OpCtx;

/// What an operator did with the event it was given.
///
/// The four cases are §11's, and they are Blender's: a transform that follows the
/// mouse is `Running` until it is confirmed (`Finished`) or aborted (`Cancelled`),
/// and `PassThrough` is how a modal operator says "this one is not mine" without
/// giving up its place on the stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpResult {
    /// The operator is modal: it stays on the stack and gets the following events.
    Running,
    /// Done. Whatever it changed stays changed.
    Finished,
    /// Aborted. An operator that returns this must have put the world back itself —
    /// the runtime does not snapshot anything on its behalf.
    Cancelled,
    /// Not this operator's event. The runtime keeps looking, and a modal operator
    /// returning this stays on the stack.
    PassThrough,
}

/// One named unit of user-visible work.
///
/// `W` is the application's world — the model, the selection, whatever else an
/// operator is allowed to touch. It is a type parameter rather than a trait object of
/// ours because the alternative is a vocabulary of domain verbs in this crate, and
/// this crate has no business knowing what a node is.
pub trait Operator<W> {
    /// The name the keymap, a menu or a script refers to this operator by.
    ///
    /// A string rather than a type, for the same reason Blender uses one: a keymap
    /// that is data has to survive being written to a file and read back.
    fn name(&self) -> &'static str;

    /// Whether the operator can run right now.
    ///
    /// Asked before every run, on both paths — [`invoke`](Self::invoke) and
    /// [`exec`](Self::exec) — so that "the key and the script do the same thing"
    /// includes refusing in the same circumstances. Cheap: it is asked once per
    /// candidate binding on every matching event.
    fn poll(&self, cx: &OpCtx<'_, W>) -> bool {
        let _ = cx;
        true
    }

    /// Runs the operator from the interface, with the event that started it.
    fn invoke(&mut self, cx: &mut OpCtx<'_, W>) -> OpResult;

    /// Runs the operator from a script, a test or a redo, from its properties alone.
    ///
    /// The default forwards to [`invoke`](Self::invoke), which is right for an
    /// operator whose whole input is its properties, and wrong for one that reads the
    /// pointer — that one has to state what it does without an event.
    fn exec(&mut self, cx: &mut OpCtx<'_, W>) -> OpResult {
        self.invoke(cx)
    }

    /// Handles one event while this operator is on the modal stack.
    ///
    /// The event is in `cx.event()`. Returning [`OpResult::Running`] keeps the
    /// operator on the stack; anything else takes it off.
    fn modal(&mut self, cx: &mut OpCtx<'_, W>) -> OpResult {
        let _ = cx;
        OpResult::Finished
    }
}

/// Cumulative counters, for spotting work — and leaks — that should not be happening.
///
/// Counters rather than timings, for the reason `rnd/architecture.md` §20.9 gives and
/// §37.4 repeated: a claim about an interaction system that can be stated as a count
/// has to be, because a count is the same number on a laptop and on a CI runner.
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
    /// Operators that ran without their poll being asked.
    ///
    /// Structurally zero: both paths ask. It is counted rather than asserted because
    /// a third path is exactly the sort of thing a later change adds, and a criterion
    /// on this number notices while an assertion in a doc comment does not.
    pub unpolled: u64,
    /// Events delivered to a modal operator.
    pub modal_events: u64,
    /// Modal operators started.
    pub modal_starts: u64,
    /// Modal operators that ran to completion.
    pub modal_finishes: u64,
    /// Modal operators that were cancelled.
    pub modal_cancels: u64,
    /// Events a modal operator deliberately let past.
    pub passthrough: u64,
    /// Events that reached the widget tree before the runtime, while a modal operator
    /// was running.
    ///
    /// The leak counter, and the one §38 is written on. Zero for a modal operator
    /// that started from a press — Masonry's pointer capture makes the driver the
    /// target — and *not* zero for one started from a key, because capture cannot be
    /// taken outside a press. See [`runtime::Seat`].
    pub tree_first: u64,
    /// Events the pre-tree seat saw, whether or not it was allowed to act on them.
    pub seen_first: u64,
    /// Events a pre-tree seat kept from the widget tree.
    ///
    /// The mirror of [`tree_first`](Self::tree_first), and the number §39 is written on:
    /// with the flag this fork adds to `Layer::capture_pointer_event`, a gesture the
    /// runtime owns costs the tree nothing at all.
    pub withheld: u64,
    /// Presses held while the runtime waited to see what they became.
    pub holds: u64,
    /// Held presses that resolved into neither a click nor a drag, and were dropped.
    ///
    /// Not an error: another button, a key or a cancel arrived first. It is counted
    /// because a hold that is abandoned often means a keymap asking to hold presses
    /// nothing is waiting for.
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
