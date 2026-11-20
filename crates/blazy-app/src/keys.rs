//! What the screen does on a key, as data.

pub use blazy_node_editor::{SCREEN_CONTEXT, ScreenAction};
use blazy_ops::event::Pattern;
use blazy_ops::keymap::{Binding, Keymap};
use masonry::core::keyboard::Modifiers;

/// A screen binding names an action this build does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownScreenAction(pub String);

impl std::fmt::Display for UnknownScreenAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no screen action is called {:?}", self.0)
    }
}

impl std::error::Error for UnknownScreenAction {}

/// Which keys do what to the screen: the [`SCREEN_CONTEXT`] section of a keymap, as data.
///
/// The actions are operators now: each area's runtime runs them from this section
/// like any other binding, after the editor's own sections, and the driver does what they
/// ask. This type is what an application rebinds them with. The defaults hold a modifier the
/// editor's own keymap does not use — and a test holds them to that — except `N`, which the
/// editor's keymap leaves free; the window this crate replaced bound plain `X` to a split
/// and made `node.delete` unreachable.
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
                // Blender's: `N` over an editor. A plain key is safe here — this seat
                // hears no key while a text field has focus.
                (Pattern::key("n"), ScreenAction::ToggleSidebar),
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
    /// skipped: a binding that does nothing is the miss that looks like success.
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
}
