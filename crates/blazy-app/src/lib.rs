//! The assembly: node editors over one graph, in areas, in as many windows as the user
//! opens.
//!
//! An application writes its graph ([`NodeGraph`]), what a node looks like
//! ([`NodeSource`](blazy_canvas::NodeSource)) and what fills an area; [`EditorApp`] does
//! the rest:
//!
//! * every area is its own layer, so an idle area keeps its pixels;
//! * a window collects what another window changed before it draws, and is woken only when something is owed;
//! * the screen's operations — split, join, swap, maximize, detach, another window, the workspace file — are operators
//!   in every area's keymap, bound by data ([`ScreenKeys`]) and done by the driver;
//! * a detached area takes its view, its selection and its history with it, and leaves nothing modal behind.
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

use blazy_areas::{AreaContent, AreaId, AreaScreen, RegionKind, SplitTree, Workspace};
use blazy_node_editor::{
    EdgePan, EditorSession, NodeEditor, NodeGraph, ScreenState, SessionHandle, SharedGraph, SystemClipboard, sync_root,
};
use blazy_ops::keymap::{Keymap, KeymapError};
use blazy_shell::Backend;
use blazy_shell::window::{Error, SharedClipboard, ShellCtx, ShellDriver, WindowConfig, WindowKey, run};
use masonry::app::RenderRoot;
use masonry::core::{
    DefaultProperties, Handled, NewWidget, PointerEvent, TextEvent, Widget, WidgetId, WidgetMut, WidgetRef,
};
use masonry::dpi::LogicalSize;
use masonry::kurbo::Axis;
use masonry::peniko::Color;

pub use crate::keys::{SCREEN_CONTEXT, ScreenAction, ScreenKeys, UnknownScreenAction};

/// The keymap an application starts from: the editor's bindings and the screen's, so one
/// overrides file covers both.
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
pub type EditorScreen<G> = AreaScreen<SessionHandle<G>>;

/// What fills one area, given the session it shows.
type BuildArea<G> = dyn Fn(AreaId, &SessionHandle<G>) -> NewWidget<dyn Widget>;

/// What an application writes beside the workspace when it is saved.
type SaveGraph<G> = dyn Fn(&SharedGraph<G>);

/// What an application reads into its graph when the workspace is read; `true` if it did.
type LoadGraph<G> = dyn Fn(&mut G) -> bool;

/// What fills one area's sidebar, given the session the area shows.
type BuildSidebar<G> = dyn Fn(AreaId, &SessionHandle<G>) -> NewWidget<dyn Widget>;

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
    /// Windows woken because one of their views was owed something; zero after an event
    /// that changed nothing.
    pub wakes: u64,
    /// Times a window's keys were pointed at another editor (once per area crossed).
    pub key_targets: u64,
    /// Pointer events handed from the host seat to an operator started by a key
    /// ([`NodeEditor::host_pointer_event`]).
    pub host_events: u64,
}

/// Node editors over one graph, in areas, in windows: the [`ShellDriver`] an application
/// hands to the window loop.
pub struct EditorApp<G: NodeGraph> {
    graph: SharedGraph<G>,
    build: Rc<BuildArea<G>>,
    /// What fills each area's sidebar, when the application gave its areas one.
    sidebar: Option<Rc<BuildSidebar<G>>>,
    session: Rc<MakeSession<G>>,
    /// Edge pan for every session the application makes, when it asked for it.
    edge_pan: Option<EdgePan>,
    /// The process's clipboard, once the first window is up — handed to every area's world.
    clipboard: Option<SharedClipboard>,
    workspace: Option<PathBuf>,
    /// What the application writes beside the workspace when it is saved — its graph.
    on_save: Option<Rc<SaveGraph<G>>>,
    /// What replaces the graph when the workspace is read — its file.
    on_load: Option<Rc<LoadGraph<G>>>,
    /// Whether the workspace file has been read at startup yet.
    restored: bool,
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
    /// The properties every widget falls back on: the theme, and the application's
    /// styles — what a selected node looks like among them.
    properties: Option<DefaultProperties>,
    /// Every window this application has.
    windows: Vec<WindowKey>,
    /// The editor each window last sent its keys to.
    focus: Vec<(WindowKey, WidgetId)>,
    counters: AppCounters,
}

impl<G: NodeGraph> EditorApp<G> {
    /// An application over `graph`, whose areas are filled by `build`, called for every area
    /// that appears with the session it shows — usually a [`NodeEditor`] made with
    /// [`NodeEditor::with_session`].
    pub fn new(
        graph: &SharedGraph<G>,
        build: impl Fn(AreaId, &SessionHandle<G>) -> NewWidget<dyn Widget> + 'static,
    ) -> Self {
        let defaults = WindowConfig::default();
        Self {
            graph: graph.clone(),
            build: Rc::new(build),
            sidebar: None,
            // The editor's operators and the screen's, with both sections of the keymap.
            session: Rc::new(|graph| {
                EditorSession::with_runtime(graph, blazy_node_editor::ops::runtime_with(default_keymap())).share()
            }),
            edge_pan: None,
            clipboard: None,
            workspace: None,
            on_save: None,
            on_load: None,
            restored: false,
            layers: true,
            tree: SplitTree::balanced(1),
            title: defaults.title,
            size: defaults.size,
            backend: defaults.backend,
            base_color: defaults.base_color,
            remaining: 0,
            properties: None,
            windows: Vec::new(),
            focus: Vec::new(),
            counters: AppCounters::default(),
        }
    }

    /// The same application, making each area's session with `make` (a runtime of its own:
    /// [`EditorSession::with_runtime`]). Its keymap should include the screen's section.
    #[must_use]
    pub fn with_sessions(mut self, make: impl Fn(&SharedGraph<G>) -> SessionHandle<G> + 'static) -> Self {
        self.session = Rc::new(make);
        self
    }

    /// The same application, scrolling the view at the edge of an area during a gesture
    /// whose binding asks for it ([`EdgePan`], [`EDGE_PAN`](blazy_node_editor::EDGE_PAN)).
    #[must_use]
    pub fn with_edge_pan(mut self, edge_pan: EdgePan) -> Self {
        self.edge_pan = Some(edge_pan);
        self
    }

    /// The same application, with other bindings for the screen's operations.
    #[must_use]
    pub fn with_keys(mut self, keys: ScreenKeys) -> Self {
        let keymap = blazy_node_editor::ops::default_keymap().with(SCREEN_CONTEXT, keys.bindings());
        self.session = Rc::new(move |graph| {
            EditorSession::with_runtime(graph, blazy_node_editor::ops::runtime_with(keymap.clone())).share()
        });
        self
    }

    /// The same application, with `keymap` in force for the editors and the screen (its
    /// [`SCREEN_CONTEXT`] section). Replaces [`with_sessions`](Self::with_sessions).
    ///
    /// # Errors
    ///
    /// When the screen section names an action there is none of.
    pub fn with_keymap(mut self, keymap: Keymap) -> Result<Self, UnknownScreenAction> {
        // Checked: a screen binding naming no action is refused.
        let _ = ScreenKeys::from_keymap(&keymap)?;
        self.session = Rc::new(move |graph| {
            EditorSession::with_runtime(graph, blazy_node_editor::ops::runtime_with(keymap.clone())).share()
        });
        Ok(self)
    }

    /// The same application, with a user's overrides file laid over [`default_keymap`], all
    /// or nothing.
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

    /// The same application, with `properties` (the theme and the application's styles, such
    /// as a selected node's) in place of the theme's defaults.
    #[must_use]
    pub fn with_properties(mut self, properties: DefaultProperties) -> Self {
        self.properties = Some(properties);
        self
    }

    /// The same application, every area of which has a sidebar on its right, filled by
    /// `build`: each area becomes an [`AreaContent`], and the sidebar's state is the
    /// session's, shown and hidden by [`ScreenAction::ToggleSidebar`] and kept in the
    /// workspace file.
    #[must_use]
    pub fn with_sidebar(
        mut self,
        build: impl Fn(AreaId, &SessionHandle<G>) -> NewWidget<dyn Widget> + 'static,
    ) -> Self {
        self.sidebar = Some(Rc::new(build));
        self
    }

    /// The same application, writing and reading its workspace at `path`; read when the
    /// first window opens if the file is there.
    #[must_use]
    pub fn with_workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace = Some(path.into());
        self
    }

    /// Runs `save` whenever the workspace is saved ([`ScreenAction::SaveWorkspace`]), after
    /// the workspace file is written.
    ///
    /// Where an application writes its graph. Not called without a workspace path.
    #[must_use]
    pub fn on_save(mut self, save: impl Fn(&SharedGraph<G>) + 'static) -> Self {
        self.on_save = Some(Rc::new(save));
        self
    }

    /// Runs `load` whenever the workspace is read ([`ScreenAction::LoadWorkspace`], `Ctrl+O`),
    /// before the screen is: the application reads its graph's file and replaces the
    /// graph's contents, and answers whether it did.
    ///
    /// **Keep the registry:** move the old graph's [`Views`](blazy_node_editor::Views) into
    /// the new one before swapping them, or every view is registered nowhere.
    ///
    /// Gestures are cancelled before the hook; after it every view starts over, the selection
    /// and the copy go, and the history is cleared. Not called at startup.
    #[must_use]
    pub fn on_load(mut self, load: impl Fn(&mut G) -> bool + 'static) -> Self {
        self.on_load = Some(Rc::new(load));
        self
    }

    /// Whether idle areas keep their pixels (areas also ask, with
    /// `AreaContent::with_isolated_layer`). On by default.
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

    /// The same, with the first area taking over a session that already exists (detach).
    pub fn screen_carrying(&self, tree: SplitTree, carried: Option<SessionHandle<G>>) -> EditorScreen<G> {
        let (graph, build, session) = (self.graph.clone(), self.build.clone(), self.session.clone());
        let sidebar = self.sidebar.clone();
        let edge_pan = self.edge_pan;
        let mut carried = carried;
        AreaScreen::with_payloads(tree, move |area| {
            // The carried session goes to the first area only.
            let session = carried.take().unwrap_or_else(|| {
                let session = session(&graph);
                if edge_pan.is_some() {
                    session.borrow_mut().set_edge_pan(edge_pan);
                }
                session
            });
            let main = build(area, &session);
            let widget = match &sidebar {
                Some(panel) => {
                    let content = AreaContent::new(vec![(RegionKind::Main, 0.0, main)])
                        .with_sidebar(panel(area, &session), session.borrow().sidebar().clone());
                    NewWidget::new(content).erased()
                },
                None => main,
            };
            (widget, session)
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
        let properties = self
            .properties
            .take()
            .unwrap_or_else(masonry::theme::default_property_set);
        run(config, NewWidget::new(screen).erased(), properties, self)
    }

    /// The windows that have to draw to catch up with a change: all of them while any view
    /// is owed something, none otherwise.
    pub fn windows_to_wake(&self) -> Vec<WindowKey> {
        if !self.graph.borrow().views().has_pending() {
            return Vec::new();
        }
        self.windows.clone()
    }

    /// Collects what the views in `root` are owed. What [`ShellDriver::frame`] does.
    pub fn pull(&mut self, root: &mut RenderRoot) -> usize {
        // Cloned: applying a change may read the graph.
        let views = self.graph.borrow().views().clone();
        if !views.has_pending() {
            return 0;
        }
        let applied = sync_root(root, &views);
        self.counters.pulled += applied as u64;
        applied
    }

    /// Points `window`'s keys (the focus fallback) at the editor of the area under the
    /// pointer, or of the first area. Read through a `WidgetRef`, which costs no passes.
    pub fn route_keys(&mut self, window: WindowKey, root: &mut RenderRoot) {
        let target = {
            let screen = root.get_layer_root(0);
            let Some(screen) = screen.downcast::<EditorScreen<G>>() else {
                return;
            };
            // Mid-gesture the keys stay with the running operator.
            let area = screen.hovered_area().or_else(|| screen.tree().areas().next());
            area.and_then(|area| screen.payload(area))
                .and_then(|session| session.borrow().editor())
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

    /// Does what `action` asks, in the window `root` belongs to, to the area under the pointer.
    ///
    /// For a test or a script on the driver's side; a key or a menu entry runs the action's
    /// operator, whose request the driver does after the event ([`settled`](ShellDriver::settled)).
    pub fn perform(&mut self, cx: &mut ShellCtx, root: &mut RenderRoot, action: ScreenAction) -> Handled {
        self.perform_in(cx, root, None, action)
    }

    /// Does what `action` asks to `area` — the area whose operator asked — or to the area under
    /// the pointer for `None`.
    fn perform_in(
        &mut self,
        cx: &mut ShellCtx,
        root: &mut RenderRoot,
        area: Option<AreaId>,
        action: ScreenAction,
    ) -> Handled {
        let target = area;
        match action {
            ScreenAction::SplitHorizontal => on_area::<G>(root, target, |screen, area| {
                AreaScreen::split(screen, area, Axis::Horizontal, 0.5);
            }),
            ScreenAction::SplitVertical => on_area::<G>(root, target, |screen, area| {
                AreaScreen::split(screen, area, Axis::Vertical, 0.5);
            }),
            ScreenAction::Join => on_area::<G>(root, target, |screen, area| {
                // This area survives, as in Blender.
                match screen.widget.tree().joinable(area) {
                    Some(sibling) => {
                        AreaScreen::join(screen, area, sibling);
                    },
                    None => tracing::info!(area, "no sibling to join with"),
                }
            }),
            ScreenAction::Swap => {
                on_area::<G>(root, target, |screen, area| match screen.widget.tree().joinable(area) {
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
                        if let Some(area) = target.or_else(|| screen.widget.hovered_area()) {
                            AreaScreen::maximize(screen, area);
                        }
                    },
                })
            },
            ScreenAction::NewWindow => self.open_window(cx, None),
            ScreenAction::Detach => {
                let taken = with_screen::<G, _>(root, |screen| {
                    let area = target.or_else(|| screen.widget.hovered_area())?;
                    detach_area(screen, area)
                });
                match taken {
                    Some(session) => self.open_window(cx, Some(session)),
                    // The last area of a window does not detach.
                    None => tracing::info!("nothing to detach here"),
                }
            },
            ScreenAction::SaveWorkspace => self.save(root),
            ScreenAction::LoadWorkspace => self.load(root),
            ScreenAction::ToggleSidebar => {
                if self.sidebar.is_none() {
                    return Handled::No;
                }
                on_area::<G>(root, target, |screen, area| {
                    if let Some(session) = screen.widget.payload(area) {
                        let state = session.borrow().sidebar().clone();
                        let mut sidebar = state.get();
                        sidebar.shown = !sidebar.shown;
                        state.set(sidebar);
                    }
                    refresh_area(screen, area);
                });
            },
            // An action a later editor crate added and this driver does not know yet.
            _ => {
                tracing::warn!(?action, "a screen action this driver does not do");
                return Handled::No;
            },
        }
        Handled::Yes
    }

    /// Asks for another window over the graph, with a fresh tree, the first area taking
    /// over `carried` if there is one.
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
            // Every area is the same kind of editor.
            let mut workspace = Workspace::new(screen.widget.tree().clone());
            let areas: Vec<AreaId> = screen.widget.tree().areas().collect();
            for area in areas {
                workspace.set_content(area, "node-editor");
                if self.sidebar.is_some()
                    && let Some(session) = screen.widget.payload(area)
                {
                    workspace.set_sidebar(area, session.borrow().sidebar().get());
                }
            }
            workspace.write()
        });
        match std::fs::write(path, text) {
            Ok(()) => tracing::info!(path = %path.display(), "workspace written"),
            Err(error) => tracing::warn!(path = %path.display(), "workspace not written: {error}"),
        }
        if let Some(save) = &self.on_save {
            save(&self.graph);
        }
    }

    /// Replaces the graph through the application's `on_load`, if it has one and it reads.
    fn reload_graph(&mut self, root: &mut RenderRoot) {
        let Some(load) = self.on_load.clone() else {
            return;
        };
        // A gesture holds names of the graph about to go; it ends first, cleaning up.
        // Gestures in other windows end when their editors see the new generation.
        with_screen::<G, _>(root, |screen| {
            let areas: Vec<AreaId> = screen.widget.tree().areas().collect();
            for area in areas {
                if let Some(session) = screen.widget.payload(area) {
                    session.borrow_mut().cancel_all();
                }
            }
        });
        let replaced = load(&mut self.graph.borrow_mut());
        if !replaced {
            return;
        }
        {
            let graph = self.graph.borrow();
            graph.views().graph_replaced(&*graph);
        }
        // This window starts over now; the others when they next draw.
        self.pull(root);
        tracing::info!("graph read");
    }

    /// `Ctrl+O`: the graph, through the application, then the screen.
    fn load(&mut self, root: &mut RenderRoot) {
        self.reload_graph(root);
        self.load_workspace(root);
    }

    /// The screen alone, from the workspace file — what the first window does at startup.
    fn load_workspace(&self, root: &mut RenderRoot) {
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
                with_screen::<G, _>(root, |screen| {
                    AreaScreen::set_tree(screen, workspace.tree().clone());
                    // After the tree: an area the file names may only now exist.
                    let areas: Vec<AreaId> = screen.widget.tree().areas().collect();
                    for area in areas {
                        if let (Some(sidebar), Some(session)) = (workspace.sidebar(area), screen.widget.payload(area)) {
                            session.borrow().sidebar().set(sidebar);
                        }
                        refresh_area(screen, area);
                    }
                });
                tracing::info!(path = %path.display(), "workspace read");
            },
            Err(error) => tracing::warn!(path = %path.display(), "workspace not read: {error}"),
        }
    }
}

impl<G: NodeGraph> ShellDriver for EditorApp<G> {
    /// Records the window, and opens the next one `with_windows` asked for.
    fn started(&mut self, cx: &mut ShellCtx, window: WindowKey, root: &mut RenderRoot) {
        // The first window brings the screen back as the workspace file left it.
        if !self.restored {
            self.restored = true;
            if self.workspace.as_ref().is_some_and(|path| path.exists()) {
                self.load_workspace(root);
            }
        }
        self.windows.push(window);
        self.counters.windows_opened += 1;
        // The one clipboard of the process, which the editors' copy and paste reach.
        if self.clipboard.is_none() {
            self.clipboard = Some(cx.shared_clipboard());
        }
        self.route_keys(window, root);
        self.sync_screen(root);
        if self.remaining > 0 {
            self.remaining -= 1;
            self.open_window(cx, None);
        }
    }

    /// Catches this window up with what another one changed.
    fn frame(&mut self, _window: WindowKey, root: &mut RenderRoot) {
        self.pull(root);
    }

    /// Does what the screen's operators asked, points the keys at the area under the
    /// pointer, and wakes the windows whose views are owed something.
    fn settled(&mut self, cx: &mut ShellCtx, window: WindowKey, root: &mut RenderRoot) {
        self.do_screen_requests(cx, root);
        self.route_keys(window, root);
        self.sync_screen(root);
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

    /// Hands the pointer to an operator started by a key, wherever the pointer is: it has
    /// no capture (a press's), so its window's pointer events go to its editor from here.
    fn pointer_event(
        &mut self,
        _cx: &mut ShellCtx,
        _window: WindowKey,
        root: &mut RenderRoot,
        event: &PointerEvent,
    ) -> Handled {
        if root.pointer_capture_target().is_some() {
            return Handled::No;
        }
        let editor = {
            let screen = root.get_layer_root(0);
            let Some(screen) = screen.downcast::<EditorScreen<G>>() else {
                return Handled::No;
            };
            modal_editor(&screen)
        };
        let Some(editor) = editor else {
            return Handled::No;
        };
        self.counters.host_events += 1;
        let consumed = root.edit_widget(editor, |mut widget| {
            NodeEditor::<G>::host_pointer_event(&mut widget.downcast::<NodeEditor<G>>(), event)
        });
        if consumed { Handled::Yes } else { Handled::No }
    }

    /// Keeps the screen's snapshot current before a key reaches the tree.
    fn text_event(
        &mut self,
        cx: &mut ShellCtx,
        _window: WindowKey,
        root: &mut RenderRoot,
        event: &TextEvent,
    ) -> Handled {
        let _ = (cx, event);
        self.sync_screen(root);
        Handled::No
    }
}

impl<G: NodeGraph> EditorApp<G> {
    /// Gives every area's world of this window the snapshot of the screen its operators poll
    /// on, and the system clipboard.
    fn sync_screen(&mut self, root: &mut RenderRoot) {
        let (workspace, sidebar, clipboard) =
            (self.workspace.is_some(), self.sidebar.is_some(), self.clipboard.clone());
        let screen = root.get_layer_root(0);
        let Some(screen) = screen.downcast::<EditorScreen<G>>() else {
            return;
        };
        let areas: Vec<AreaId> = screen.tree().areas().collect();
        for &area in &areas {
            let Some(session) = screen.payload(area) else {
                continue;
            };
            let state = ScreenState {
                driven: true,
                joinable: screen.tree().joinable(area).is_some(),
                areas: areas.len(),
                workspace,
                sidebar,
            };
            let mut session = session.borrow_mut();
            let world = session.world_mut();
            world.set_screen(state);
            if world.clipboard().is_none()
                && let Some(clipboard) = &clipboard
            {
                let (read, write) = (clipboard.clone(), clipboard.clone());
                world.set_clipboard(Some(SystemClipboard::new(
                    move || read.borrow_mut().read(),
                    move |text| write.borrow_mut().write(text),
                )));
            }
        }
    }

    /// Does what the screen's operators asked in this window's areas during the event.
    fn do_screen_requests(&mut self, cx: &mut ShellCtx, root: &mut RenderRoot) {
        let requests: Vec<(AreaId, ScreenAction)> = {
            let screen = root.get_layer_root(0);
            let Some(screen) = screen.downcast::<EditorScreen<G>>() else {
                return;
            };
            screen
                .tree()
                .areas()
                .filter_map(|area| screen.payload(area).map(|session| (area, session.clone())))
                .flat_map(|(area, session)| {
                    let taken = session.borrow_mut().world_mut().take_screen_requests();
                    taken.into_iter().map(move |action| (area, action))
                })
                .collect()
        };
        for (area, action) in requests {
            let _ = self.perform_in(cx, root, Some(area), action);
        }
    }
}

/// The editor of a screen whose operator is running, if any — the one that owns the
/// pointer and the keys until it ends.
fn modal_editor<G: NodeGraph>(screen: &WidgetRef<'_, EditorScreen<G>>) -> Option<WidgetId> {
    screen
        .tree()
        .areas()
        .filter_map(|area| screen.payload(area))
        .find_map(|session| {
            let session = session.borrow();
            if session.runtime().modal_depth() > 0 {
                session.editor()
            } else {
                None
            }
        })
}

/// Takes an area out of a screen for another window, cancelling any gesture in its session
/// (a gesture belongs to its window). `None` for the last area.
pub fn detach_area<G: NodeGraph>(
    screen: &mut WidgetMut<'_, EditorScreen<G>>,
    area: AreaId,
) -> Option<SessionHandle<G>> {
    let session = EditorScreen::<G>::detach(screen, area)?;
    {
        let session = &mut *session.borrow_mut();
        session.cancel_all();
    }
    Some(session)
}

/// Lays an area's regions out again after its sidebar state changed from outside.
fn refresh_area<G: NodeGraph>(screen: &mut WidgetMut<'_, EditorScreen<G>>, area: AreaId) {
    let Some(id) = screen.widget.area_widget(area) else {
        return;
    };
    screen.ctx.mutate_later(id, |mut widget| {
        if let Some(mut content) = widget.try_downcast::<AreaContent>() {
            AreaContent::refresh(&mut content);
        }
    });
}

/// Runs `act` on the window's screen.
fn with_screen<G: NodeGraph, R>(
    root: &mut RenderRoot,
    act: impl FnOnce(&mut WidgetMut<'_, EditorScreen<G>>) -> R,
) -> R {
    root.edit_base_layer(|mut widget| act(&mut widget.downcast::<EditorScreen<G>>()))
}

/// Runs `act` on the screen and `area`, or the area under the pointer for `None`.
fn on_area<G: NodeGraph>(
    root: &mut RenderRoot,
    area: Option<AreaId>,
    act: impl FnOnce(&mut WidgetMut<'_, EditorScreen<G>>, AreaId),
) {
    with_screen::<G, _>(root, |screen| {
        if let Some(area) = area.or_else(|| screen.widget.hovered_area()) {
            act(screen, area);
        }
    });
}
