//! The assembly: node editors over one graph, in areas, in as many windows as the user
//! opens.
//!
//! Every crate below this one is a mechanism — a split tree, a canvas, an operator
//! runtime, a window loop — and an application has to put them together before any of
//! them shows anything. That putting-together was written once, in the `area-screen`
//! example, and it was where two defects lived that no mechanism could see
//! (`issues/application assembly.md`): a view that left the tree stayed owed every later
//! change, so every window was woken after every event for good; and a link made in one
//! window never reached another. Both were the application's bookkeeping, and both are
//! the library's now — [`Views`](blazy_node_editor::Views) in `blazy-node-editor`, and
//! the driver here.
//!
//! What an application still writes is what only it knows: its graph
//! ([`NodeGraph`]), what a node looks like ([`NodeSource`](blazy_canvas::NodeSource)),
//! and what fills an area. The rest is [`EditorApp`]:
//!
//! * every area is its own layer, so an idle area keeps its pixels (§36);
//! * a window collects what another window changed before it draws, and is woken only when something is owed (§44.3);
//! * the screen's operations — split, join, swap, maximize, detach, another window, the workspace file — run on keys
//!   that are data ([`ScreenKeys`]);
//! * a detached area takes its view, its selection and its history with it, and leaves nothing modal behind (§44).
//!
//! ```no_run
//! # use blazy_app::EditorApp;
//! # use blazy_areas::SplitTree;
//! # use blazy_shell::window::WindowConfig;
//! # fn editor_for<G: blazy_node_editor::NodeGraph>(
//! #     _: &blazy_node_editor::SessionHandle<G>,
//! # ) -> masonry::core::NewWidget<dyn masonry::core::Widget> { unimplemented!() }
//! # fn run<G: blazy_node_editor::NodeGraph>(graph: blazy_node_editor::SharedGraph<G>) {
//! EditorApp::new(&graph, |_area, session| editor_for(session))
//!     .run(WindowConfig::default(), SplitTree::balanced(2))
//!     .unwrap();
//! # }
//! ```

#![warn(missing_docs, unreachable_pub)]

mod keys;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::rc::Rc;

use blazy_areas::{AreaId, AreaScreen, SplitTree, Workspace};
use blazy_node_editor::{EditorSession, NodeGraph, SessionHandle, SharedGraph, sync_root};
use blazy_ops::keymap::{Keymap, KeymapError};
use blazy_shell::Backend;
use blazy_shell::window::{Error, ShellCtx, ShellDriver, WindowConfig, WindowKey, run};
use masonry::app::RenderRoot;
use masonry::core::{Handled, NewWidget, TextEvent, Widget, WidgetId, WidgetMut};
use masonry::dpi::LogicalSize;
use masonry::kurbo::Axis;
use masonry::peniko::Color;

pub use crate::keys::{SCREEN_CONTEXT, ScreenAction, ScreenKeys, UnknownScreenAction};

/// The keymap an application starts from: the editor's bindings and the screen's.
///
/// One keymap, so one file overrides both — a user who rebinds `node.delete` and the
/// split in the same breath writes one file, and [`Keymap::write`] of this is that file's
/// starting point.
pub fn default_keymap() -> Keymap {
    blazy_node_editor::ops::default_keymap().with(SCREEN_CONTEXT, ScreenKeys::default().bindings())
}

/// Why a keymap could not be put in force.
#[derive(Debug)]
#[non_exhaustive]
pub enum KeymapLoadError {
    /// The file could not be read.
    Io(std::io::Error),
    /// It could be read, and it is not a keymap, or a line of it is wrong.
    Keymap(KeymapError),
    /// Its screen section names an action there is none of.
    Screen(UnknownScreenAction),
}

impl std::fmt::Display for KeymapLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Keymap(error) => write!(f, "{error}"),
            Self::Screen(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for KeymapLoadError {}

/// A screen of node editors: areas that carry the session their editor shows.
///
/// The session is the area's payload rather than its widget's field, because a widget
/// does not survive being rebuilt in another window and a view, a selection and a
/// history must (§44.2).
pub type EditorScreen<G> = AreaScreen<SessionHandle<G>>;

/// What fills one area, given the session it shows.
type BuildArea<G> = dyn Fn(AreaId, &SessionHandle<G>) -> NewWidget<dyn Widget>;

/// Makes a session for an area that appears.
type MakeSession<G> = dyn Fn(&SharedGraph<G>) -> SessionHandle<G>;

/// What the assembly has done, summed over its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppCounters {
    /// Windows the application has opened, the first one included.
    pub windows_opened: u64,
    /// Changes a window collected because another window made them.
    pub pulled: u64,
    /// Windows woken because one of their views was owed something.
    ///
    /// Zero after an event that changed nothing, and that is the claim §36 rests on: a
    /// window that is woken for no reason is a window that never idles.
    pub wakes: u64,
    /// Times a window's keys were pointed at another editor.
    ///
    /// One per crossing from one area into another, not one per event: the target is
    /// read on every event, and set only when it changed.
    pub key_targets: u64,
}

/// Node editors over one graph, in areas, in windows: the [`ShellDriver`] an application
/// hands to the window loop.
///
/// One per process, like any driver (§44.4). The graph is shared by every window; each
/// window is another screen over it, not another application (§30).
pub struct EditorApp<G: NodeGraph> {
    graph: SharedGraph<G>,
    build: Rc<BuildArea<G>>,
    session: Rc<MakeSession<G>>,
    keys: ScreenKeys,
    workspace: Option<PathBuf>,
    layers: bool,
    /// The tree a new window opens with.
    tree: SplitTree,
    /// What every window after the first is opened with.
    title: String,
    size: LogicalSize<f64>,
    backend: Option<Backend>,
    base_color: Color,
    /// Windows still to open at startup.
    remaining: usize,
    /// Every window this application has.
    windows: Vec<WindowKey>,
    /// The editor each window last sent its keys to.
    focus: Vec<(WindowKey, WidgetId)>,
    counters: AppCounters,
}

impl<G: NodeGraph> EditorApp<G> {
    /// An application over `graph`, whose areas are filled by `build`.
    ///
    /// `build` is called for every area that appears — at startup, on a split, when a
    /// workspace is read, in a window that was just opened — with the session the area
    /// shows. The usual body is a [`NodeEditor`](blazy_node_editor::NodeEditor) over a
    /// canvas, made with
    /// [`NodeEditor::with_session`](blazy_node_editor::NodeEditor::with_session),
    /// optionally inside an `AreaContent` with a header above it.
    pub fn new(
        graph: &SharedGraph<G>,
        build: impl Fn(AreaId, &SessionHandle<G>) -> NewWidget<dyn Widget> + 'static,
    ) -> Self {
        let defaults = WindowConfig::default();
        Self {
            graph: graph.clone(),
            build: Rc::new(build),
            session: Rc::new(|graph| EditorSession::new(graph).share()),
            keys: ScreenKeys::default(),
            workspace: None,
            layers: true,
            tree: SplitTree::balanced(1),
            title: defaults.title,
            size: defaults.size,
            backend: defaults.backend,
            base_color: defaults.base_color,
            remaining: 0,
            windows: Vec::new(),
            focus: Vec::new(),
            counters: AppCounters::default(),
        }
    }

    /// The same application, making each area's session with `make`.
    ///
    /// For a keymap of the application's own, or operators of its own beside the
    /// editor's: [`EditorSession::with_runtime`] takes the runtime whole.
    #[must_use]
    pub fn with_sessions(mut self, make: impl Fn(&SharedGraph<G>) -> SessionHandle<G> + 'static) -> Self {
        self.session = Rc::new(make);
        self
    }

    /// The same application, with other bindings for the screen's operations.
    #[must_use]
    pub fn with_keys(mut self, keys: ScreenKeys) -> Self {
        self.keys = keys;
        self
    }

    /// The same application, with `keymap` in force for the editors and the screen.
    ///
    /// Each area's session gets a runtime with this keymap, and the screen's bindings
    /// come from its [`SCREEN_CONTEXT`] section. Replaces [`with_sessions`](Self::with_sessions).
    ///
    /// # Errors
    ///
    /// When the screen section names an action there is none of.
    pub fn with_keymap(mut self, keymap: Keymap) -> Result<Self, UnknownScreenAction> {
        self.keys = ScreenKeys::from_keymap(&keymap)?;
        self.session = Rc::new(move |graph| {
            EditorSession::with_runtime(graph, blazy_node_editor::ops::runtime_with(keymap.clone())).share()
        });
        Ok(self)
    }

    /// The same application, with a user's overrides file laid over [`default_keymap`].
    ///
    /// All or nothing: a file with one bad line leaves the defaults in force and says
    /// which line.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, is not a keymap, or names a binding or an action
    /// there is none of.
    pub fn with_keymap_overrides(self, path: impl AsRef<std::path::Path>) -> Result<Self, KeymapLoadError> {
        let text = std::fs::read_to_string(path).map_err(KeymapLoadError::Io)?;
        let keymap = default_keymap().patched(&text).map_err(KeymapLoadError::Keymap)?;
        self.with_keymap(keymap).map_err(KeymapLoadError::Screen)
    }

    /// The same application, writing and reading its workspace at `path`.
    ///
    /// Without one, [`ScreenAction::SaveWorkspace`] and [`ScreenAction::LoadWorkspace`]
    /// do nothing.
    #[must_use]
    pub fn with_workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace = Some(path.into());
        self
    }

    /// Whether idle areas keep their pixels (§36). On by default.
    ///
    /// The areas still have to ask to be layers — `AreaContent::with_isolated_layer` —
    /// and this is the host's half: naming them, so the shell asks them to repaint and
    /// the presenter may keep their pixels.
    #[must_use]
    pub fn with_layer_cache(mut self, enabled: bool) -> Self {
        self.layers = enabled;
        self
    }

    /// Opens `count` windows at startup rather than one.
    #[must_use]
    pub fn with_windows(mut self, count: usize) -> Self {
        self.remaining = count.saturating_sub(1);
        self
    }

    /// A screen over the graph, tiled by `tree`.
    pub fn screen(&self, tree: SplitTree) -> EditorScreen<G> {
        self.screen_carrying(tree, None)
    }

    /// The same, with the first area taking over a session that already exists.
    ///
    /// What detach builds: the window is new, the tree is new, the widget is new — and
    /// the session is the one the area had (§44).
    pub fn screen_carrying(&self, tree: SplitTree, carried: Option<SessionHandle<G>>) -> EditorScreen<G> {
        let (graph, build, session) = (self.graph.clone(), self.build.clone(), self.session.clone());
        let mut carried = carried;
        AreaScreen::with_payloads(tree, move |area| {
            // The carried session goes to the first area asked for and to no other: an
            // area that appears later is a new view and needs a session of its own, or
            // two areas would share one selection.
            let session = carried.take().unwrap_or_else(|| session(&graph));
            (build(area, &session), session)
        })
    }

    /// What the application has done.
    pub fn counters(&self) -> AppCounters {
        self.counters
    }

    /// Opens the first window, tiled by `tree`, and runs until the last one closes.
    ///
    /// # Errors
    ///
    /// As [`blazy_shell::window::run`].
    pub fn run(mut self, config: WindowConfig, tree: SplitTree) -> Result<(), Error> {
        self.tree = tree.clone();
        self.title = config.title.clone();
        self.size = config.size;
        self.backend = config.backend;
        self.base_color = config.base_color;
        let screen = self.screen(tree);
        run(
            config,
            NewWidget::new(screen).erased(),
            masonry::theme::default_property_set(),
            self,
        )
    }

    /// The windows that have to draw to catch up with a change, after an event.
    ///
    /// Every window while any view is owed something, and none otherwise. Coarse on
    /// purpose: a window does not know which views are in another window's tree, and a
    /// window woken with nothing to collect draws a frame its layers mostly copy (§36).
    /// What must never happen is a wake with nothing owed, and that is what the
    /// registry forgetting departed views guarantees.
    pub fn windows_to_wake(&self) -> Vec<WindowKey> {
        if !self.graph.borrow().views().has_pending() {
            return Vec::new();
        }
        self.windows.clone()
    }

    /// Collects what the views in `root` are owed. What [`ShellDriver::frame`] does.
    pub fn pull(&mut self, root: &mut RenderRoot) -> usize {
        // Cloned out of the graph so that applying a change — which may build a node,
        // which reads the graph — never meets a borrow held here.
        let views = self.graph.borrow().views().clone();
        if !views.has_pending() {
            return 0;
        }
        let applied = sync_root(root, &views);
        self.counters.pulled += applied as u64;
        applied
    }

    /// Points `window`'s keys at the editor of the area under the pointer, or of the
    /// first area when the pointer is over none.
    ///
    /// Masonry hands a key to the focused widget or to the window's focus fallback and to
    /// nobody else (§38.3); a window of several editors that names none has editors that
    /// never hear `G`, `X` or `Ctrl+Z` — which is what the window this crate replaced did.
    /// The area under the pointer is Blender's answer and the one an area can give: the
    /// screen publishes it, and the area's session knows its editor.
    ///
    /// Read through a `WidgetRef`, not a `WidgetMut`: this runs after every event, and an
    /// edit would cost the window a battery of rewrite passes each time (§38.2).
    pub fn route_keys(&mut self, window: WindowKey, root: &mut RenderRoot) {
        let target = {
            let screen = root.get_layer_root(0);
            let Some(screen) = screen.downcast::<EditorScreen<G>>() else {
                return;
            };
            let area = screen.hovered_area().or_else(|| screen.tree().areas().next());
            area.and_then(|area| screen.payload(area))
                .and_then(|session| session.borrow().editor)
        };
        let Some(target) = target else {
            return;
        };
        match self.focus.iter_mut().find(|(key, _)| *key == window) {
            Some((_, current)) if *current == target => return,
            Some((_, current)) => *current = target,
            None => self.focus.push((window, target)),
        }
        if root.set_focus_fallback(Some(target)) {
            self.counters.key_targets += 1;
        }
    }

    /// Does what `action` asks, in the window `root` belongs to.
    ///
    /// Public for a test or a menu, which have an action rather than a key.
    pub fn perform(&mut self, cx: &mut ShellCtx, root: &mut RenderRoot, action: ScreenAction) -> Handled {
        match action {
            ScreenAction::SplitHorizontal => on_hovered_area::<G>(root, |screen, area| {
                AreaScreen::split(screen, area, Axis::Horizontal, 0.5);
            }),
            ScreenAction::SplitVertical => on_hovered_area::<G>(root, |screen, area| {
                AreaScreen::split(screen, area, Axis::Vertical, 0.5);
            }),
            ScreenAction::Join => on_hovered_area::<G>(root, |screen, area| {
                // The survivor is the one under the pointer, as in Blender. An area
                // whose sibling is a split has no partner: that is what a binary tree can
                // express, and §41.1 counts what it costs.
                match screen.widget.tree().joinable(area) {
                    Some(sibling) => {
                        AreaScreen::join(screen, area, sibling);
                    },
                    None => tracing::info!(area, "no sibling to join with; see §41.1"),
                }
            }),
            ScreenAction::Swap => {
                on_hovered_area::<G>(root, |screen, area| match screen.widget.tree().joinable(area) {
                    Some(sibling) => {
                        AreaScreen::swap(screen, area, sibling);
                    },
                    None => tracing::info!(area, "no sibling to swap with"),
                })
            },
            ScreenAction::ToggleMaximize => {
                with_screen::<G, _>(root, |screen| match screen.widget.tree().maximized() {
                    Some(_) => {
                        AreaScreen::restore(screen);
                    },
                    None => {
                        if let Some(area) = screen.widget.hovered_area() {
                            AreaScreen::maximize(screen, area);
                        }
                    },
                })
            },
            ScreenAction::NewWindow => self.open_window(cx, None),
            ScreenAction::Detach => {
                let taken = with_screen::<G, _>(root, |screen| {
                    let area = screen.widget.hovered_area()?;
                    detach_area(screen, area)
                });
                match taken {
                    Some(session) => self.open_window(cx, Some(session)),
                    // The last area of a window does not detach, exactly as an area with
                    // no sibling does not join.
                    None => tracing::info!("nothing to detach here"),
                }
            },
            ScreenAction::SaveWorkspace => self.save(root),
            ScreenAction::LoadWorkspace => self.load(root),
        }
        Handled::Yes
    }

    /// Asks for another window over the graph, with the first area taking over
    /// `carried` if there is one.
    ///
    /// The screen is built here and handed over as a fresh tree, because a widget tree
    /// cannot move between windows: `RenderRoot` owns its arena and Masonry has no
    /// reparenting (§44.1). What the windows share is the graph.
    fn open_window(&mut self, cx: &mut ShellCtx, carried: Option<SessionHandle<G>>) {
        let detached = carried.is_some();
        let tree = if detached {
            SplitTree::balanced(1)
        } else {
            self.tree.clone()
        };
        let screen = self.screen_carrying(tree, carried);
        let config = WindowConfig {
            title: if detached {
                format!("{} - detached area", self.title)
            } else {
                self.title.clone()
            },
            size: self.size,
            backend: self.backend,
            base_color: self.base_color,
            ..WindowConfig::default()
        };
        let window = cx.open_window(config, NewWidget::new(screen).erased());
        tracing::info!(?window, detached, "window asked for");
    }

    fn save(&self, root: &mut RenderRoot) {
        let Some(path) = &self.workspace else {
            tracing::info!("no workspace file to write to");
            return;
        };
        let text = with_screen::<G, _>(root, |screen| {
            // The tree is the geometry; what fills each area is the application's to
            // say, and here every area is the same kind of editor (§41.5).
            let mut workspace = Workspace::new(screen.widget.tree().clone());
            for area in screen.widget.tree().areas() {
                workspace.set_content(area, "node-editor");
            }
            workspace.write()
        });
        match std::fs::write(path, text) {
            Ok(()) => tracing::info!(path = %path.display(), "workspace written"),
            Err(error) => tracing::warn!(path = %path.display(), "workspace not written: {error}"),
        }
    }

    fn load(&self, root: &mut RenderRoot) {
        let Some(path) = &self.workspace else {
            tracing::info!("no workspace file to read from");
            return;
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(path = %path.display(), "workspace not read: {error}");
                return;
            },
        };
        match Workspace::parse(&text) {
            Ok(workspace) => {
                with_screen::<G, _>(root, |screen| AreaScreen::set_tree(screen, workspace.tree().clone()));
                tracing::info!(path = %path.display(), "workspace read");
            },
            Err(error) => tracing::warn!(path = %path.display(), "workspace not read: {error}"),
        }
    }
}

impl<G: NodeGraph> ShellDriver for EditorApp<G> {
    /// Records the window, and opens the next one `with_windows` asked for.
    ///
    /// Each new window comes back here, so the count walks down to zero through the same
    /// path a key takes, rather than through a loop of its own.
    fn started(&mut self, cx: &mut ShellCtx, window: WindowKey, root: &mut RenderRoot) {
        self.windows.push(window);
        self.counters.windows_opened += 1;
        self.route_keys(window, root);
        if self.remaining > 0 {
            self.remaining -= 1;
            self.open_window(cx, None);
        }
    }

    /// Catches this window up with what another one changed.
    ///
    /// The pull half of §30: the push reaches the areas of one window and is dropped
    /// across the boundary of two, so the views record what they are owed and the window
    /// collects it when it draws (§44.3).
    fn frame(&mut self, _window: WindowKey, root: &mut RenderRoot) {
        self.pull(root);
    }

    /// Points the keys at the area under the pointer, and wakes the windows whose views are
    /// owed something.
    ///
    /// After the event rather than in a seat: a seat is offered the event before the
    /// tree acts on it, so a change made with the mouse would be noticed one gesture
    /// late (§44.3).
    fn settled(&mut self, cx: &mut ShellCtx, window: WindowKey, root: &mut RenderRoot) {
        // The keys follow the pointer from area to area, decided after the event like
        // everything else here: the event is what moved the pointer.
        self.route_keys(window, root);
        for window in self.windows_to_wake() {
            self.counters.wakes += 1;
            cx.request_redraw(window);
        }
    }

    fn layers(&mut self, _window: WindowKey, root: &mut RenderRoot) -> Vec<WidgetId> {
        if !self.layers {
            return Vec::new();
        }
        // Read-only: this is asked every frame, and an edit costs a rewrite battery.
        root.get_layer_root(0)
            .downcast::<EditorScreen<G>>()
            .map(|screen| screen.area_ids())
            .unwrap_or_default()
    }

    /// The screen's operations, from the seat in front of the tree.
    ///
    /// Here rather than in a widget because they are the *screen's* operations and the
    /// screen is the root: there is nothing above it to bubble to.
    fn text_event(
        &mut self,
        cx: &mut ShellCtx,
        _window: WindowKey,
        root: &mut RenderRoot,
        event: &TextEvent,
    ) -> Handled {
        match self.keys.action_for(event) {
            Some(action) => self.perform(cx, root, action),
            None => Handled::No,
        }
    }
}

/// Takes an area out of a screen, ready to be built in another window.
///
/// `AreaScreen::detach` removes the area and hands back what it carried; this adds the
/// one thing the screen cannot know to do — **cancelling whatever was modal in the
/// session**. A gesture belongs to the window it was made in: pointer capture is that
/// window's `RenderRoot`'s, and the area is about to stop being there (§44).
///
/// `None` for the last area of a screen, which does not detach.
pub fn detach_area<G: NodeGraph>(
    screen: &mut WidgetMut<'_, EditorScreen<G>>,
    area: AreaId,
) -> Option<SessionHandle<G>> {
    let session = EditorScreen::<G>::detach(screen, area)?;
    {
        let session = &mut *session.borrow_mut();
        session.runtime.cancel_all(&mut session.world);
    }
    Some(session)
}

/// Runs `act` on the window's screen.
fn with_screen<G: NodeGraph, R>(
    root: &mut RenderRoot,
    act: impl FnOnce(&mut WidgetMut<'_, EditorScreen<G>>) -> R,
) -> R {
    root.edit_base_layer(|mut widget| act(&mut widget.downcast::<EditorScreen<G>>()))
}

/// Runs `act` on the screen and the area under the pointer, if there is one.
fn on_hovered_area<G: NodeGraph>(root: &mut RenderRoot, act: impl FnOnce(&mut WidgetMut<'_, EditorScreen<G>>, AreaId)) {
    with_screen::<G, _>(root, |screen| {
        if let Some(area) = screen.widget.hovered_area() {
            act(screen, area);
        }
    });
}
