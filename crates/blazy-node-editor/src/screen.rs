//! The screen's operations as operators: split, join, detach, save, open, the sidebar.
//!
//! The screen is the driver's, so an operator leaves a request in the world
//! ([`EditorWorld::take_screen_requests`]) for the driver to do after the event. The
//! operators poll on a snapshot the driver keeps ([`ScreenState`]); without one, they are
//! refused.

use std::rc::Rc;

use blazy_ops::runtime::OpCtx;
use blazy_ops::{OpResult, Operator};

use crate::{EditorWorld, NodeGraph};

/// An operation on the screen or on an area: the area whose operator asked, or the one
/// under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScreenAction {
    /// Splits the area into two, side by side.
    SplitHorizontal,
    /// Splits the area into two, one above the other.
    SplitVertical,
    /// Joins the area with its sibling, which must be an area; this one survives.
    Join,
    /// Swaps the area with its sibling.
    Swap,
    /// Shows the area alone, or brings the screen back.
    ToggleMaximize,
    /// Opens another window over the same graph.
    NewWindow,
    /// Moves the area into a window of its own.
    Detach,
    /// Writes the workspace file.
    SaveWorkspace,
    /// Reads the workspace file back.
    LoadWorkspace,
    /// Shows or hides the area's sidebar — Blender's `N`.
    ToggleSidebar,
}

/// The keymap context the screen's bindings live in.
pub const SCREEN_CONTEXT: &str = "screen";

impl ScreenAction {
    /// Every action, in the order the defaults bind them.
    pub const ALL: [Self; 10] = [
        Self::SplitHorizontal,
        Self::SplitVertical,
        Self::Join,
        Self::Swap,
        Self::ToggleMaximize,
        Self::NewWindow,
        Self::Detach,
        Self::SaveWorkspace,
        Self::LoadWorkspace,
        Self::ToggleSidebar,
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
            Self::ToggleSidebar => "screen.sidebar",
        }
    }

    /// The action a keymap file names, if it names one.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.name() == name)
    }
}

/// What the screen's operators poll on: a snapshot of the screen around this world's area,
/// kept by the driver. The default refuses every operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(clippy::exhaustive_structs, reason = "built by a driver, field by field")]
pub struct ScreenState {
    /// Whether a driver keeps this snapshot at all.
    pub driven: bool,
    /// Whether the area has a sibling to join or swap with.
    pub joinable: bool,
    /// How many areas the area's window shows.
    pub areas: usize,
    /// Whether a workspace file is set, to save to and read from.
    pub workspace: bool,
    /// Whether the area has a sidebar to show.
    pub sidebar: bool,
}

impl ScreenState {
    /// Whether `action` can be done now.
    pub fn allows(&self, action: ScreenAction) -> bool {
        // No guard for a focused text field: a key reaches the editor only if the field leaves
        // it, and with the screen's keys in the editor's keymap the field keeps them.
        if !self.driven {
            return false;
        }
        match action {
            ScreenAction::Join | ScreenAction::Swap => self.joinable,
            ScreenAction::Detach => self.areas > 1,
            ScreenAction::SaveWorkspace | ScreenAction::LoadWorkspace => self.workspace,
            ScreenAction::ToggleSidebar => self.sidebar,
            _ => true,
        }
    }
}

/// The system clipboard, as a driver hands it to a world: `node.copy` and `node.paste`
/// write and read it themselves, from a key, a menu or a script alike.
#[derive(Clone)]
pub struct SystemClipboard {
    read: Rc<dyn Fn() -> Option<String>>,
    write: Rc<dyn Fn(String)>,
}

impl SystemClipboard {
    /// A clipboard read with `read` and written with `write`.
    pub fn new(read: impl Fn() -> Option<String> + 'static, write: impl Fn(String) + 'static) -> Self {
        Self {
            read: Rc::new(read),
            write: Rc::new(write),
        }
    }

    /// What it holds, if text.
    pub fn read(&self) -> Option<String> {
        (self.read)()
    }

    /// Puts `text` on it.
    pub fn write(&self, text: String) {
        (self.write)(text);
    }
}

impl std::fmt::Debug for SystemClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SystemClipboard")
    }
}

/// One operation on the screen, as an operator: refused when the snapshot says it cannot be
/// done, and otherwise a request for the driver.
pub struct ScreenOp {
    action: ScreenAction,
}

impl ScreenOp {
    /// The operator for `action`, named as the action is ([`ScreenAction::name`]).
    pub fn new(action: ScreenAction) -> Self {
        Self { action }
    }
}

impl<G: NodeGraph> Operator<EditorWorld<G>> for ScreenOp {
    fn name(&self) -> &'static str {
        self.action.name()
    }

    fn poll(&self, cx: &OpCtx<'_, EditorWorld<G>>) -> bool {
        cx.world().screen().allows(self.action)
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        cx.world_mut().request_screen(self.action);
        OpResult::Finished
    }
}
