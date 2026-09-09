//! Undo as a journal of steps.
//!
//! §30 put the truth in the model, so undo lives there too — the question this module
//! answers is *what a step is*. Two shapes were measured (§38.4): a **snapshot** of
//! the model per step, and a **journal** of what each operator changed. The journal
//! is what this is, because the snapshot is priced per node and the journal is priced
//! per *touched* node, and a drag touches the selection while a graph holds twenty
//! thousand.
//!
//! What the shape costs a caller is one method per operator that changes anything:
//! [`Step::undo`] and [`Step::redo`] have to put the world back exactly. A snapshot
//! needs none, which is the trade — and why the number rather than the taste decides.
//!
//! [`Step::bytes`] is not bookkeeping for its own sake: it is what makes a ceiling
//! possible, and a ceiling is what keeps a journal of a long session from being the
//! same problem as a snapshot.

use std::collections::VecDeque;

/// One reversible change.
///
/// `&mut self` on both halves so a step may keep whatever it needs to invert itself
/// and swap it back and forth rather than storing both sides twice.
pub trait Step<W> {
    /// What this step is called, for a history panel and for a test's assertion.
    fn name(&self) -> &'static str;

    /// Puts the world back the way it was before the step.
    fn undo(&mut self, world: &mut W);

    /// Applies the step again.
    fn redo(&mut self, world: &mut W);

    /// Roughly what this step costs to hold, in bytes.
    ///
    /// Roughly is enough: it decides when to drop the oldest step, and being out by a
    /// pointer per entry changes nothing about that decision.
    fn bytes(&self) -> usize {
        std::mem::size_of::<usize>()
    }
}

/// The history: what has been done, and what has been undone.
pub struct UndoStack<W> {
    /// A deque rather than a `Vec`, and the ceiling is the reason: steps are pushed and
    /// undone at the back, and [`trim`](Self::trim) drops them at the *front*, which on
    /// a `Vec` is a move of the whole history per dropped step.
    done: VecDeque<Box<dyn Step<W>>>,
    undone: Vec<Box<dyn Step<W>>>,
    bytes: usize,
    limit: usize,
    dropped: u64,
}

impl<W> Default for UndoStack<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W> UndoStack<W> {
    /// An empty history with no ceiling. See [`with_limit`](Self::with_limit).
    pub fn new() -> Self {
        Self {
            done: VecDeque::new(),
            undone: Vec::new(),
            bytes: 0,
            limit: usize::MAX,
            dropped: 0,
        }
    }

    /// A stack that drops its oldest steps once it holds more than `bytes`.
    ///
    /// No default ceiling is offered. What a session may spend on history is an
    /// application's decision and depends on what its steps hold; a library number
    /// here would be a guess dressed as a policy.
    #[must_use]
    pub fn with_limit(mut self, bytes: usize) -> Self {
        self.limit = bytes;
        self
    }

    /// Records a step that has already been applied.
    ///
    /// Drops the redo branch, as every editor does: once the history has been
    /// departed from, what was undone is no longer reachable.
    pub fn push(&mut self, step: Box<dyn Step<W>>) {
        self.bytes += step.bytes();
        self.done.push_back(step);
        self.forget_undone();
        self.trim();
    }

    /// Undoes the most recent step. Returns its name, or `None` if there was none.
    pub fn undo(&mut self, world: &mut W) -> Option<&'static str> {
        let mut step = self.done.pop_back()?;
        step.undo(world);
        let name = step.name();
        self.undone.push(step);
        Some(name)
    }

    /// Redoes the most recently undone step.
    pub fn redo(&mut self, world: &mut W) -> Option<&'static str> {
        let mut step = self.undone.pop()?;
        step.redo(world);
        let name = step.name();
        self.done.push_back(step);
        Some(name)
    }

    /// Steps that can still be undone.
    pub fn depth(&self) -> usize {
        self.done.len()
    }

    /// Steps that can be redone.
    pub fn redo_depth(&self) -> usize {
        self.undone.len()
    }

    /// What the history holds, in bytes, by its steps' own reckoning.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Steps dropped for want of room.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Forgets everything. What a "new file" does.
    pub fn clear(&mut self) {
        self.done.clear();
        self.forget_undone();
        self.bytes = 0;
    }

    fn forget_undone(&mut self) {
        for step in self.undone.drain(..) {
            self.bytes = self.bytes.saturating_sub(step.bytes());
        }
    }

    /// Drops the oldest steps until the history fits.
    ///
    /// The oldest rather than the largest: history is only useful as an unbroken
    /// sequence backwards from now, and a hole in the middle of it is worse than a
    /// shorter one. The most recent step is never dropped, so a ceiling smaller than
    /// one step leaves one rather than none — an empty history is not a smaller one,
    /// it is a different thing.
    fn trim(&mut self) {
        while self.bytes > self.limit && self.done.len() > 1 {
            let Some(step) = self.done.pop_front() else {
                return;
            };
            self.bytes = self.bytes.saturating_sub(step.bytes());
            self.dropped += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, PartialEq, Debug)]
    struct World {
        value: i32,
    }

    struct Add(i32);

    impl Step<World> for Add {
        fn name(&self) -> &'static str {
            "add"
        }
        fn undo(&mut self, world: &mut World) {
            world.value -= self.0;
        }
        fn redo(&mut self, world: &mut World) {
            world.value += self.0;
        }
        fn bytes(&self) -> usize {
            64
        }
    }

    #[test]
    fn undo_and_redo_return_the_same_state() {
        let mut world = World::default();
        let mut stack = UndoStack::new();
        for step in [1, 2, 3] {
            world.value += step;
            stack.push(Box::new(Add(step)));
        }
        assert_eq!(world.value, 6);
        while stack.undo(&mut world).is_some() {}
        assert_eq!(world, World { value: 0 });
        while stack.redo(&mut world).is_some() {}
        assert_eq!(world, World { value: 6 });
    }

    /// The rule every editor has and nobody writes down: doing something new after an
    /// undo drops what was undone.
    #[test]
    fn a_new_step_drops_the_redo_branch() {
        let mut world = World::default();
        let mut stack = UndoStack::new();
        world.value += 1;
        stack.push(Box::new(Add(1)));
        stack.undo(&mut world);
        assert_eq!(stack.redo_depth(), 1);
        assert_eq!(world.value, 0, "the undo took it back");
        world.value += 5;
        stack.push(Box::new(Add(5)));
        assert_eq!(stack.redo_depth(), 0);
        assert_eq!(stack.bytes(), 64);
        assert!(stack.redo(&mut world).is_none(), "there is nothing to redo");
    }

    #[test]
    fn a_ceiling_drops_the_oldest_step() {
        let mut world = World::default();
        let mut stack = UndoStack::new().with_limit(128);
        for _ in 0..4 {
            world.value += 1;
            stack.push(Box::new(Add(1)));
        }
        assert_eq!(world.value, 4);
        assert_eq!(stack.depth(), 2, "128 bytes holds two 64-byte steps");
        assert_eq!(stack.dropped(), 2);
        // The two that are left still work, and history stops where it was trimmed.
        while stack.undo(&mut world).is_some() {}
        assert_eq!(world.value, 2, "what was dropped cannot be undone");
    }

    /// Trimming has to keep the *order* of what survives, not merely the count.
    ///
    /// The steps are distinguishable here, unlike in the test above: a history that
    /// drops from the wrong end, or reorders what it keeps, undoes the wrong amount and
    /// nothing but the final value would show it.
    #[test]
    fn what_a_ceiling_keeps_is_the_most_recent_in_order() {
        let mut world = World::default();
        // Four steps of 64 bytes, room for three.
        let mut stack = UndoStack::new().with_limit(192);
        for step in 1..=4 {
            world.value += step;
            stack.push(Box::new(Add(step)));
        }
        assert_eq!(world.value, 10);
        assert_eq!(stack.depth(), 3);
        assert_eq!(stack.dropped(), 1, "the oldest went, and only the oldest");

        while stack.undo(&mut world).is_some() {}
        assert_eq!(world.value, 1, "2, 3 and 4 came back off, in that order");
    }
}
