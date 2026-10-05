//! What the screen does on a key, as data.

use blazy_ops::event::{OpEvent, Pattern};
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
