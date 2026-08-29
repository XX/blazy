//! The event an operator sees, and the pattern a keymap matches it against.
//!
//! Normalised on purpose: the runtime is driven from three places (§38) — the widget
//! tree, a layer's pre-tree hook and the host in front of `RenderRoot` — and all three
//! have a different event type in their hands. One shape here means the keymap has one
//! thing to match and an operator has one thing to read.
//!
//! Positions are in the driver's own coordinate space, whatever that is; the runtime
//! never interprets them. The example converts to canvas coordinates before it builds
//! the event, because that is the space its operators think in.

use masonry::core::keyboard::{Key, Modifiers};
use masonry::kurbo::Point;
use masonry::ui_events::pointer::PointerButton;

/// A pointer or key event, in the form the keymap matches and an operator receives.
#[derive(Clone, Debug, PartialEq)]
pub enum OpEvent {
    /// A pointer button went down.
    Press {
        button: PointerButton,
        pos: Point,
        mods: Modifiers,
    },
    /// A pointer button came up.
    Release {
        button: PointerButton,
        pos: Point,
        mods: Modifiers,
    },
    /// The pointer moved. Never matched by a binding — a keymap that could start an
    /// operator on a bare move would start one on every frame of a drag.
    Move { pos: Point, mods: Modifiers },
    /// A key went down or came up.
    Key { key: Key, mods: Modifiers, down: bool },
}

impl OpEvent {
    /// Where the pointer was, for the events that have a position.
    pub fn pos(&self) -> Option<Point> {
        match self {
            Self::Press { pos, .. } | Self::Release { pos, .. } | Self::Move { pos, .. } => Some(*pos),
            Self::Key { .. } => None,
        }
    }

    /// The modifiers held when the event happened.
    pub fn mods(&self) -> Modifiers {
        match self {
            Self::Press { mods, .. }
            | Self::Release { mods, .. }
            | Self::Move { mods, .. }
            | Self::Key { mods, .. } => *mods,
        }
    }
}

/// What kind of event a binding fires on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// A button going down.
    Press(PointerButton),
    /// A button coming up.
    Release(PointerButton),
    /// A key going down.
    Key(Key),
    /// A key coming up. Rare, and here because a keymap that cannot express it makes
    /// "held to pan" impossible to write as data.
    KeyUp(Key),
}

/// One event pattern: what happened, and which modifiers were held while it did.
///
/// Modifiers match **exactly**. Shift-click is a different binding from click, which
/// is what lets `extend` be a property of the binding rather than a branch inside the
/// operator — the difference between a keymap a user can edit and one they cannot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    pub trigger: Trigger,
    pub mods: Modifiers,
}

impl Pattern {
    /// A pattern with no modifiers held.
    pub fn new(trigger: Trigger) -> Self {
        Self {
            trigger,
            mods: Modifiers::empty(),
        }
    }

    /// The same pattern, with `mods` required.
    #[must_use]
    pub fn with_mods(mut self, mods: Modifiers) -> Self {
        self.mods = mods;
        self
    }

    /// A press of `button`.
    pub fn press(button: PointerButton) -> Self {
        Self::new(Trigger::Press(button))
    }

    /// A release of `button`.
    pub fn release(button: PointerButton) -> Self {
        Self::new(Trigger::Release(button))
    }

    /// A character key going down, e.g. `Pattern::key("g")`.
    pub fn key(name: &str) -> Self {
        Self::new(Trigger::Key(Key::Character(name.into())))
    }

    /// A named key going down, e.g. `Pattern::named(NamedKey::Escape)`.
    pub fn named(key: masonry::core::keyboard::NamedKey) -> Self {
        Self::new(Trigger::Key(Key::Named(key)))
    }

    /// Whether `event` is this pattern.
    pub fn matches(&self, event: &OpEvent) -> bool {
        // Only the modifiers a binding names are compared, and all of them are: an
        // exact test is what keeps Ctrl+Z from also firing on Ctrl+Shift+Z.
        if !mods_equal(self.mods, event.mods()) {
            return false;
        }
        match (&self.trigger, event) {
            (Trigger::Press(want), OpEvent::Press { button, .. }) => want == button,
            (Trigger::Release(want), OpEvent::Release { button, .. }) => want == button,
            (Trigger::Key(want), OpEvent::Key { key, down: true, .. }) => want == key,
            (Trigger::KeyUp(want), OpEvent::Key { key, down: false, .. }) => want == key,
            _ => false,
        }
    }
}

/// Whether two modifier sets are the same, ignoring the lock keys.
///
/// Caps Lock and Num Lock are state, not intent: a keymap that compared them would
/// stop working the moment someone leans on Caps Lock, and no keymap ever means to
/// bind them.
fn mods_equal(a: Modifiers, b: Modifiers) -> bool {
    const INTENT: Modifiers = Modifiers::CONTROL
        .union(Modifiers::SHIFT)
        .union(Modifiers::ALT)
        .union(Modifiers::META);
    a.intersection(INTENT) == b.intersection(INTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(mods: Modifiers) -> OpEvent {
        OpEvent::Press {
            button: PointerButton::Primary,
            pos: Point::ORIGIN,
            mods,
        }
    }

    #[test]
    fn a_press_matches_its_button_and_nothing_else() {
        assert!(Pattern::press(PointerButton::Primary).matches(&press(Modifiers::empty())));
        assert!(!Pattern::press(PointerButton::Secondary).matches(&press(Modifiers::empty())));
        assert!(!Pattern::release(PointerButton::Primary).matches(&press(Modifiers::empty())));
    }

    /// The whole reason `extend` is a property of a binding and not a branch inside
    /// the operator: these are two bindings, not one.
    #[test]
    fn modifiers_are_matched_exactly() {
        let plain = Pattern::press(PointerButton::Primary);
        let shifted = plain.clone().with_mods(Modifiers::SHIFT);
        assert!(plain.matches(&press(Modifiers::empty())));
        assert!(!plain.matches(&press(Modifiers::SHIFT)));
        assert!(shifted.matches(&press(Modifiers::SHIFT)));
        assert!(!shifted.matches(&press(Modifiers::empty())));
    }

    /// Lock keys are state rather than intent, and a keymap that compared them would
    /// stop working for anyone with Caps Lock on.
    #[test]
    fn lock_keys_do_not_break_a_binding() {
        let plain = Pattern::press(PointerButton::Primary);
        assert!(plain.matches(&press(Modifiers::CAPS_LOCK)));
    }

    #[test]
    fn a_key_binding_matches_the_key_going_down() {
        let g = Pattern::key("g");
        assert!(g.matches(&OpEvent::Key {
            key: Key::Character("g".into()),
            mods: Modifiers::empty(),
            down: true,
        }));
        assert!(!g.matches(&OpEvent::Key {
            key: Key::Character("g".into()),
            mods: Modifiers::empty(),
            down: false,
        }));
        assert!(!g.matches(&OpEvent::Key {
            key: Key::Character("b".into()),
            mods: Modifiers::empty(),
            down: true,
        }));
    }
}
