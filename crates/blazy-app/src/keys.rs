//! What the screen does on a key, as data.

use blazy_ops::event::{OpEvent, Pattern};
use blazy_ops::keymap::{Binding, Keymap};
use masonry::core::TextEvent;
use masonry::core::keyboard::{KeyState, Modifiers};

/// An operation on the screen or on the area under the pointer (§41, §44).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScreenAction {
    /// Splits the area under the pointer into two, side by side.
    SplitHorizontal,
    /// Splits the area under the pointer into two, one above the other.
    SplitVertical,
    /// Joins the area under the pointer with its sibling; the one under the pointer
    /// survives. Nothing happens when the sibling is a split rather than an area (§41.1).
    Join,
    /// Swaps the area under the pointer with its sibling (§41.4).
    Swap,
    /// Shows the area under the pointer alone, or brings the screen back (§41.3).
    ToggleMaximize,
    /// Opens another window over the same graph (§44).
    NewWindow,
    /// Moves the area under the pointer into a window of its own, with its view, its
    /// selection and its history (§44).
    Detach,
    /// Writes the workspace file.
    SaveWorkspace,
    /// Reads the workspace file back.
    LoadWorkspace,
}

/// The keymap context the screen's bindings live in.
pub const SCREEN_CONTEXT: &str = "screen";

impl ScreenAction {
    /// Every action, in the order the defaults bind them.
    pub const ALL: [Self; 9] = [
        Self::SplitHorizontal,
        Self::SplitVertical,
        Self::Join,
        Self::Swap,
        Self::ToggleMaximize,
        Self::NewWindow,
        Self::Detach,
        Self::SaveWorkspace,
        Self::LoadWorkspace,
    ];

    /// The name a keymap file binds this action by.
    pub fn name(self) -> &'static str {
        match self {
            Self::SplitHorizontal => "screen.split_horizontal",
            Self::SplitVertical => "screen.split_vertical",
            Self::Join => "screen.join",
            Self::Swap => "screen.swap",
            Self::ToggleMaximize => "screen.maximize",
            Self::NewWindow => "screen.new_window",
            Self::Detach => "screen.detach",
            Self::SaveWorkspace => "screen.save_workspace",
            Self::LoadWorkspace => "screen.load_workspace",
        }
    }

    /// The action a keymap file names, if it names one.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.name() == name)
    }
}

/// A screen binding names an action this build does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownScreenAction(pub String);

impl std::fmt::Display for UnknownScreenAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no screen action is called {:?}", self.0)
    }
}

impl std::error::Error for UnknownScreenAction {}

/// Which keys do what to the screen.
///
/// Data rather than a `match`, so that an application can rebind them — the reason
/// §11 gives for a keymap at all. They are not operators of `blazy-ops` yet: an operation
/// on the screen needs the screen's widget, and an operator never touches a widget
/// (§38.3); making them operators is a question of its own.
///
/// **They are offered an event before the tree is**, from the seat in front of it
/// (§38.2), so a key bound here is a key no editor will ever hear. The defaults all hold a
/// modifier the editor's own keymap does not use, and a test holds them to that; the
/// window this crate replaced bound plain `X` to a split and made `node.delete`
/// unreachable.
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenKeys {
    /// The bindings, tried in order; the first match wins.
    pub bindings: Vec<(Pattern, ScreenAction)>,
}

impl Default for ScreenKeys {
    fn default() -> Self {
        let alt = |key: &str| Pattern::key(key).with_mods(Modifiers::ALT);
        let ctrl = |key: &str| Pattern::key(key).with_mods(Modifiers::CONTROL);
        Self {
            bindings: vec![
                (alt("x"), ScreenAction::SplitHorizontal),
                (alt("y"), ScreenAction::SplitVertical),
                (alt("j"), ScreenAction::Join),
                (alt("w"), ScreenAction::Swap),
                // Blender's own binding for the same toggle.
                (ctrl(" "), ScreenAction::ToggleMaximize),
                (alt("n"), ScreenAction::NewWindow),
                (alt("d"), ScreenAction::Detach),
                (ctrl("s"), ScreenAction::SaveWorkspace),
                (ctrl("o"), ScreenAction::LoadWorkspace),
            ],
        }
    }
}

impl ScreenKeys {
    /// No bindings at all: the screen does nothing on a key.
    pub fn none() -> Self {
        Self { bindings: Vec::new() }
    }

    /// The same bindings, with `pattern` doing `action` ahead of the rest.
    #[must_use]
    pub fn with(mut self, pattern: Pattern, action: ScreenAction) -> Self {
        self.bindings.insert(0, (pattern, action));
        self
    }

    /// The screen's bindings as a keymap section, for a file.
    pub fn bindings(&self) -> Vec<Binding> {
        self.bindings
            .iter()
            .map(|(pattern, action)| Binding::new(pattern.clone(), action.name()))
            .collect()
    }

    /// The screen's bindings as `keymap`'s [`SCREEN_CONTEXT`] section says them; none if
    /// it has no such section.
    ///
    /// # Errors
    ///
    /// When a binding names an action this build does not have. Refused rather than
    /// skipped: a binding that does nothing is the miss that looks like success (§45).
    pub fn from_keymap(keymap: &Keymap) -> Result<Self, UnknownScreenAction> {
        let Some(section) = keymap.section(SCREEN_CONTEXT) else {
            return Ok(Self::none());
        };
        let bindings = section
            .bindings
            .iter()
            .map(|binding| {
                ScreenAction::from_name(&binding.op)
                    .map(|action| (binding.pattern.clone(), action))
                    .ok_or_else(|| UnknownScreenAction(binding.op.to_string()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { bindings })
    }

    /// What `event` asks the screen to do, if anything.
    pub fn action_for(&self, event: &TextEvent) -> Option<ScreenAction> {
        let TextEvent::Keyboard(key) = event else {
            return None;
        };
        let event = OpEvent::Key {
            key: key.key.clone(),
            mods: key.modifiers,
            down: key.state == KeyState::Down,
        };
        self.bindings
            .iter()
            .find(|(pattern, _)| pattern.matches(&event))
            .map(|(_, action)| *action)
    }
}
