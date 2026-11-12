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

use masonry::core::keyboard::{Code, Key, KeyState, KeyboardEvent, Modifiers};
use masonry::kurbo::Point;
use masonry::ui_events::pointer::PointerButton;

/// What kind of pointer produced an event.
///
/// Here because the drag threshold is not one number: a mouse and a finger differ by an
/// order of magnitude in how far they travel before the user meant to travel (§39.2).
/// Masonry carries this on every pointer event, so a driver has it for free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Device {
    /// A mouse, or anything the platform reports as one.
    #[default]
    Mouse,
    /// A pen or stylus.
    Pen,
    /// A finger.
    Touch,
    /// Anything else, measured as a mouse.
    Other,
}

/// What a driver knows about an event that the event itself does not say.
///
/// Two fields, and both are needed to tell a click from a drag without a clock:
///
/// * `time_ns` comes from `PointerState::time` — the platform's own timestamp, so a double click is decided from the
///   events and a test can produce one by hand.
/// * `screen` is where the pointer was **in screen pixels**. The position inside an [`OpEvent`] is in whatever space
///   the driver thinks in — canvas units in the example, which span a factor of 400 across the zoom range — so a
///   threshold measured on it would mean something different at every zoom (§25.2 learned this once already).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    /// The platform's own timestamp, in nanoseconds.
    pub time_ns: u64,
    /// What produced the event, which is what chooses the drag threshold.
    pub device: Device,
    /// Where the pointer was, in screen pixels.
    pub screen: Point,
}

impl Sample {
    /// A sample with no timestamp, for a driver or a test that has none.
    ///
    /// Zero time means every click is a first click: two of them cannot be closer than
    /// the double-click window because they are not closer at all.
    pub fn at(screen: Point) -> Self {
        Self {
            time_ns: 0,
            device: Device::Mouse,
            screen,
        }
    }
}

/// A pointer or key event, in the form the keymap matches and an operator receives.
#[derive(Clone, Debug, PartialEq)]
pub enum OpEvent {
    /// A pointer button went down.
    Press {
        /// The button that went down.
        button: PointerButton,
        /// Where the pointer was, in the driver's own space.
        pos: Point,
        /// The modifiers held at the time.
        mods: Modifiers,
    },
    /// A pointer button came up.
    Release {
        /// The button that came up.
        button: PointerButton,
        /// Where the pointer was, in the driver's own space.
        pos: Point,
        /// The modifiers held at the time.
        mods: Modifiers,
    },
    /// The pointer moved. Never matched by a binding — a keymap that could start an
    /// operator on a bare move would start one on every frame of a drag.
    Move {
        /// Where the pointer is now, in the driver's own space.
        pos: Point,
        /// The modifiers held at the time.
        mods: Modifiers,
    },
    /// The wheel or a trackpad scrolled.
    ///
    /// `dy` is in **logical pixels**, positive towards the user (the wheel rolled back, a
    /// page scrolled down), whatever unit the platform reported: a line or a page is
    /// converted by the driver, which is the one that knows how big a line and a page are
    /// (`blazy_canvas::wheel_pixels`). An operator that zooms decides how many pixels are
    /// worth how much zoom; the event only says how far.
    Scroll {
        /// Where the pointer was, in the driver's own space.
        pos: Point,
        /// How far, in logical pixels, positive towards the user.
        dy: f64,
        /// The modifiers held at the time.
        mods: Modifiers,
    },
    /// A key went down or came up.
    Key {
        /// The key itself.
        key: Key,
        /// The modifiers held at the time.
        mods: Modifiers,
        /// Whether this is the key going down.
        down: bool,
    },
    /// A press that turned out to be a click: the button went down and came up again
    /// without travelling past the drag threshold.
    ///
    /// Synthesised by [`OpRuntime`](crate::runtime::OpRuntime), never by a driver, and
    /// `pos` is **the press's** position rather than the release's — an operator started
    /// by a gesture wants where the gesture began.
    Click {
        /// The button that was clicked.
        button: PointerButton,
        /// Where the **press** was, in the driver's own space.
        pos: Point,
        /// The same point in screen pixels, for an operator that works in that space.
        ///
        /// Both are here because a gesture has one beginning and two audiences: an
        /// operator that moves a node thinks in the driver's space, one that moves the
        /// view thinks in pixels, and neither should have to ask the other's question.
        screen: Point,
        /// The modifiers held at the time.
        mods: Modifiers,
        /// 1 for a single click, 2 for the second click of a double, and so on.
        count: u8,
    },
    /// A press that turned out to be a drag: the pointer travelled past the threshold
    /// while the button was down. `pos` is the press's position, which is the anchor a
    /// transform operator needs.
    Drag {
        /// The button holding the drag.
        button: PointerButton,
        /// Where the **press** was, in the driver's own space.
        pos: Point,
        /// The press's position in screen pixels. See [`Click::screen`](Self::Click).
        screen: Point,
        /// The modifiers held at the time.
        mods: Modifiers,
    },
}

impl OpEvent {
    /// Where the pointer was, for the events that have a position.
    pub fn pos(&self) -> Option<Point> {
        match self {
            Self::Press { pos, .. }
            | Self::Release { pos, .. }
            | Self::Move { pos, .. }
            | Self::Scroll { pos, .. }
            | Self::Click { pos, .. }
            | Self::Drag { pos, .. } => Some(*pos),
            Self::Key { .. } => None,
        }
    }

    /// The same event at another position.
    ///
    /// For a driver that has to convert: the positions in an [`OpEvent`] are in one
    /// space — whichever one the operators think in — and what arrives from the toolkit
    /// is in another. Converting once, here, is what keeps an operator from having to
    /// know which is which; the space that is *not* this one travels in
    /// [`Sample::screen`]. An event with no position is returned unchanged.
    #[must_use]
    pub fn with_pos(self, at: Point) -> Self {
        match self {
            Self::Press { button, mods, .. } => Self::Press { button, pos: at, mods },
            Self::Release { button, mods, .. } => Self::Release { button, pos: at, mods },
            Self::Move { mods, .. } => Self::Move { pos: at, mods },
            Self::Scroll { dy, mods, .. } => Self::Scroll { pos: at, dy, mods },
            Self::Click {
                button,
                screen,
                mods,
                count,
                ..
            } => Self::Click {
                button,
                pos: at,
                screen,
                mods,
                count,
            },
            Self::Drag {
                button, screen, mods, ..
            } => Self::Drag {
                button,
                pos: at,
                screen,
                mods,
            },
            Self::Key { .. } => self,
        }
    }

    /// A keyboard event, in the form a keymap matches.
    ///
    /// What every driver should build its key events with, because a key as the platform
    /// reports it is the *text* it would type, and a binding names a key:
    ///
    /// * **Case is dropped.** `Shift+D` arrives as `"D"`, and so does `D` with Caps Lock on; both are the `d` key.
    /// * **The layout is seen past.** On a layout whose letters are not Latin a key types a character no binding names
    ///   — `Shift+D` is `"В"` on a Russian one, `Ctrl+C` is `"с"` — so the key is named by where it is, as the US
    ///   layout would name that place. A layout that does type Latin letters keeps them, so on AZERTY the key labelled
    ///   `A` is still `a`.
    ///
    /// Shifted punctuation keeps what it types (`Shift+=` is `"+"`): every layout agrees
    /// it is ASCII, and only its position would say otherwise.
    pub fn from_keyboard(event: &KeyboardEvent) -> Self {
        Self::Key {
            key: binding_key(&event.key, event.code),
            mods: event.modifiers,
            down: event.state == KeyState::Down,
        }
    }

    /// The modifiers held when the event happened.
    pub fn mods(&self) -> Modifiers {
        match self {
            Self::Press { mods, .. }
            | Self::Release { mods, .. }
            | Self::Move { mods, .. }
            | Self::Scroll { mods, .. }
            | Self::Key { mods, .. }
            | Self::Click { mods, .. }
            | Self::Drag { mods, .. } => *mods,
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
    /// A press and release of `button` that never became a drag.
    ///
    /// The trigger a keymap wants for "click on nothing deselects", and the reason this
    /// enum grew: with only [`Press`](Self::Press) that sentence had to be a property on
    /// somebody else's binding (§39.1).
    Click(PointerButton),
    /// A press of `button` that travelled past the drag threshold.
    Drag(PointerButton),
    /// The wheel, or a trackpad's scroll, in either direction.
    ///
    /// One trigger for both directions, because what the direction means is the
    /// operator's — a zoom reads `dy` and goes in or out — and two bindings for one
    /// gesture would be two places to rebind it.
    Scroll,
    /// The second click of a double click, within the keymap's window.
    ///
    /// A separate trigger rather than a count property, because a binding on it must be
    /// able to sit *before* the single-click one and win.
    DoubleClick(PointerButton),
}

/// One event pattern: what happened, and which modifiers were held while it did.
///
/// Modifiers match **exactly**. Shift-click is a different binding from click, which
/// is what lets `extend` be a property of the binding rather than a branch inside the
/// operator — the difference between a keymap a user can edit and one they cannot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    /// What has to happen.
    pub trigger: Trigger,
    /// Which modifiers have to be held while it does. Matched exactly.
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

    /// A click of `button`: pressed and released without a drag.
    pub fn click(button: PointerButton) -> Self {
        Self::new(Trigger::Click(button))
    }

    /// A drag of `button`.
    pub fn drag(button: PointerButton) -> Self {
        Self::new(Trigger::Drag(button))
    }

    /// A double click of `button`.
    pub fn double_click(button: PointerButton) -> Self {
        Self::new(Trigger::DoubleClick(button))
    }

    /// A character key going down, e.g. `Pattern::key("g")`.
    pub fn key(name: &str) -> Self {
        Self::new(Trigger::Key(Key::Character(name.into())))
    }

    /// The wheel, in either direction.
    pub fn scroll() -> Self {
        Self::new(Trigger::Scroll)
    }

    /// A named key going down, e.g. `Pattern::named(NamedKey::Escape)`.
    pub fn named(key: masonry::core::keyboard::NamedKey) -> Self {
        Self::new(Trigger::Key(Key::Named(key)))
    }

    /// Whether this pattern is waiting for a gesture on `button` rather than for a raw
    /// press or release.
    ///
    /// What the runtime asks to decide whether a press is worth holding: if no binding
    /// wants a click or a drag on this button, there is nothing to wait for and the
    /// press is over the moment it happened.
    pub fn wants_gesture(&self, button: PointerButton) -> bool {
        matches!(
            self.trigger,
            Trigger::Click(want) | Trigger::Drag(want) | Trigger::DoubleClick(want) if want == button
        )
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
            (Trigger::Key(want), OpEvent::Key { key, down: true, .. }) => same_key(want, key),
            (Trigger::KeyUp(want), OpEvent::Key { key, down: false, .. }) => same_key(want, key),
            // A double click is also a click, and the keymap's order decides which
            // binding gets it: a `DoubleClick` binding placed first wins, and one that
            // is not there leaves the second click behaving like any other.
            (Trigger::Click(want), OpEvent::Click { button, .. }) => want == button,
            (Trigger::DoubleClick(want), OpEvent::Click { button, count, .. }) => want == button && *count >= 2,
            (Trigger::Drag(want), OpEvent::Drag { button, .. }) => want == button,
            (Trigger::Scroll, OpEvent::Scroll { .. }) => true,
            _ => false,
        }
    }
}

/// Whether a binding's key is the event's key.
///
/// A letter matches in either case: a keymap file says `Shift+D` as naturally as
/// `Shift+d`, and [`OpEvent::from_keyboard`] already gives the event in lower case.
fn same_key(want: &Key, key: &Key) -> bool {
    match (want, key) {
        (Key::Character(want), Key::Character(key)) => want.eq_ignore_ascii_case(key),
        _ => want == key,
    }
}

/// The key a binding names for what the platform reported: see [`OpEvent::from_keyboard`].
fn binding_key(key: &Key, code: Code) -> Key {
    let Key::Character(text) = key else {
        return key.clone();
    };
    if text.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Key::Character(text.to_ascii_lowercase());
    }
    // A character no binding can name — another script, or a control character some
    // platforms report for `Ctrl+letter` — so the key is named by its place.
    match us_character(code) {
        Some(character) => Key::Character(character.into()),
        None => key.clone(),
    }
}

/// What the key at `code` types on the US layout, unshifted, for the keys that type
/// something: the names the default keymap is written in.
fn us_character(code: Code) -> Option<&'static str> {
    Some(match code {
        Code::KeyA => "a",
        Code::KeyB => "b",
        Code::KeyC => "c",
        Code::KeyD => "d",
        Code::KeyE => "e",
        Code::KeyF => "f",
        Code::KeyG => "g",
        Code::KeyH => "h",
        Code::KeyI => "i",
        Code::KeyJ => "j",
        Code::KeyK => "k",
        Code::KeyL => "l",
        Code::KeyM => "m",
        Code::KeyN => "n",
        Code::KeyO => "o",
        Code::KeyP => "p",
        Code::KeyQ => "q",
        Code::KeyR => "r",
        Code::KeyS => "s",
        Code::KeyT => "t",
        Code::KeyU => "u",
        Code::KeyV => "v",
        Code::KeyW => "w",
        Code::KeyX => "x",
        Code::KeyY => "y",
        Code::KeyZ => "z",
        Code::Digit0 => "0",
        Code::Digit1 => "1",
        Code::Digit2 => "2",
        Code::Digit3 => "3",
        Code::Digit4 => "4",
        Code::Digit5 => "5",
        Code::Digit6 => "6",
        Code::Digit7 => "7",
        Code::Digit8 => "8",
        Code::Digit9 => "9",
        Code::Minus => "-",
        Code::Equal => "=",
        Code::BracketLeft => "[",
        Code::BracketRight => "]",
        Code::Backslash => "\\",
        Code::Semicolon => ";",
        Code::Quote => "'",
        Code::Comma => ",",
        Code::Period => ".",
        Code::Slash => "/",
        Code::Backquote => "`",
        Code::Space => " ",
        _ => return None,
    })
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

    /// A key as the platform reports it: what it types, and where it is.
    fn typed(text: &str, code: Code, mods: Modifiers) -> OpEvent {
        OpEvent::from_keyboard(&KeyboardEvent {
            state: KeyState::Down,
            key: Key::Character(text.into()),
            code,
            modifiers: mods,
            ..KeyboardEvent::default()
        })
    }

    /// `Shift+D` types `D`, and a binding written `Shift+d` is the same key — as is one
    /// written `Shift+D` in a keymap file. Caps Lock changes the text, not the key.
    #[test]
    fn a_letter_is_a_key_whatever_its_case() {
        let shift_d = Pattern::key("d").with_mods(Modifiers::SHIFT);
        assert!(shift_d.matches(&typed("D", Code::KeyD, Modifiers::SHIFT)));
        assert!(
            Pattern::key("D")
                .with_mods(Modifiers::SHIFT)
                .matches(&typed("D", Code::KeyD, Modifiers::SHIFT))
        );
        assert!(Pattern::key("g").matches(&typed("G", Code::KeyG, Modifiers::CAPS_LOCK)));
        assert!(
            !shift_d.matches(&typed("D", Code::KeyD, Modifiers::empty())),
            "modifiers still count"
        );
    }

    /// On a layout without Latin letters the key is named by its place: `Shift+В` is
    /// `Shift+D` and `Ctrl+С` is `Ctrl+C` on a Russian layout. A layout with Latin letters
    /// keeps them: the AZERTY key labelled `A`, where QWERTY has `Q`, is `a`.
    #[test]
    fn a_key_on_another_layout_is_named_by_its_place_unless_it_types_latin() {
        let shift_d = Pattern::key("d").with_mods(Modifiers::SHIFT);
        assert!(shift_d.matches(&typed("В", Code::KeyD, Modifiers::SHIFT)));
        let ctrl_c = Pattern::key("c").with_mods(Modifiers::CONTROL);
        assert!(ctrl_c.matches(&typed("с", Code::KeyC, Modifiers::CONTROL)));
        assert!(
            ctrl_c.matches(&typed("\u{3}", Code::KeyC, Modifiers::CONTROL)),
            "a control character too"
        );
        assert!(Pattern::key("-").matches(&typed("-", Code::Minus, Modifiers::empty())));

        assert!(Pattern::key("a").matches(&typed("a", Code::KeyQ, Modifiers::empty())));
        assert!(!Pattern::key("q").matches(&typed("a", Code::KeyQ, Modifiers::empty())));
        // A character with no place to name it stays what it was.
        assert_eq!(typed("ж", Code::Unidentified, Modifiers::empty()), OpEvent::Key {
            key: Key::Character("ж".into()),
            mods: Modifiers::empty(),
            down: true
        });
    }
}
