//! Owner mode: our own windows, our own event loop, our own frames on the screen.
//!
//! Each window has its own `RenderRoot`, presenter, device and layer cache. One
//! [`ShellDriver`] serves the process, and every method names its window ([`WindowKey`]);
//! a driver asks for windows, and for another window to draw, through [`ShellCtx`].
//!
//! Not `masonry_winit`, whose runner owns a compile-time rasteriser and keeps its event
//! conversion private; input conversion is the same public `ui_events_winit`.
//!
//! # How a frame reaches the screen
//!
//! Two ways, chosen at startup along with the rasteriser:
//!
//! * [`SwapchainPresenter`](crate::gpu::SwapchainPresenter) — the scene is drawn into a texture and blitted into the
//!   swapchain. The frame never enters main memory. Needs a graphics device, so it comes with the `vello` feature.
//! * [`BlitPresenter`] — the frame is rasterised into a buffer and copied into the window with `softbuffer`. Works
//!   anywhere, including on a machine with no usable GPU, and costs one pass over the frame plus the platform's own
//!   copy.
//!
//! Without a usable GPU the GPU path falls back to the second, with a warning.
//!
//! Not done: accessibility and window-manager gestures (custom title bar, resize handles,
//! system menu); their signals are ignored. Popups are layers ([`apply_layer_signal`]);
//! the clipboard and input method are in [`text`](crate::text).

use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use masonry::app::{RenderRoot, RenderRootOptions, RenderRootSignal, VisualLayerPlan, WindowSizePolicy};
use masonry::core::{
    DefaultProperties, ErasedAction, Handled, NewWidget, PointerEvent, TextEvent, Widget, WidgetId, WindowEvent,
};
use masonry::dpi::{LogicalSize, PhysicalSize};
use masonry::imaging::RgbaImage;
use masonry::peniko::Color;
use ui_events_winit::{WindowEventReducer, WindowEventTranslation};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent as WinitWindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

use crate::backend::Backend;
use crate::compose::Hole;
use crate::host::Host;
use crate::present::{PresentCounters, PresentError, Presenter};
use crate::text::{
    Clipboard, MemoryClipboard, SystemClipboard, TextCounters, apply_text_signal, ime_event, paste_or_key,
};

/// How the window should be created.
pub struct WindowConfig {
    /// The window's title.
    pub title: String,
    /// Its initial inner size, in logical pixels.
    pub size: LogicalSize<f64>,
    /// The smallest the user may make it, if it is bounded at all.
    pub min_size: Option<LogicalSize<f64>>,
    /// Whether the user may resize it.
    pub resizable: bool,
    /// What the window shows where the widget tree drew nothing: the frame must be opaque,
    /// or anti-aliased edges are blended against transparency.
    pub base_color: Color,
    /// Which rasteriser to use, or `None` for the first one that opens.
    pub backend: Option<Backend>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "blazy".to_string(),
            size: LogicalSize::new(1400.0, 900.0),
            min_size: Some(LogicalSize::new(640.0, 480.0)),
            resizable: true,
            base_color: Color::from_rgb8(0x14, 0x14, 0x18),
            backend: None,
        }
    }
}

impl WindowConfig {
    /// The same configuration, with the window's title.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// The same configuration, with the initial inner size in logical pixels.
    pub fn with_size(mut self, width: f64, height: f64) -> Self {
        self.size = LogicalSize::new(width, height);
        self
    }

    /// The same configuration, with the rasteriser named — or `None` for the first
    /// one that opens.
    pub fn with_backend(mut self, backend: Option<Backend>) -> Self {
        self.backend = backend;
        self
    }

    /// The same configuration, with the colour behind the widget tree.
    pub fn with_base_color(mut self, color: Color) -> Self {
        self.base_color = color;
        self
    }
}

/// The name of a window, handed out by the shell before the window exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowKey(u64);

/// What a driver asked the shell to do; an embedder running its own loop drains these.
#[non_exhaustive]
pub enum ShellRequest {
    /// Open a window under the name the driver was given.
    Open(Box<NewWindow>),
    /// Close a window. Closing the last one ends the loop.
    Close(WindowKey),
    /// Draw a window that may be idle.
    Redraw(WindowKey),
}

/// A window that has been asked for and not yet created.
pub struct NewWindow {
    /// The name it will answer to, already valid.
    pub window: WindowKey,
    /// How it should be created.
    pub config: WindowConfig,
    /// Its root widget, a fresh one (a tree cannot move between windows).
    pub root: NewWidget<dyn Widget>,
}

/// The process's clipboard, shared: what [`ShellCtx::shared_clipboard`] hands out.
pub type SharedClipboard = Rc<RefCell<Box<dyn Clipboard>>>;

/// What a driver may ask the shell for; queued, since creating a window needs the loop.
pub struct ShellCtx {
    requests: Vec<ShellRequest>,
    next: u64,
    /// The process's clipboard, one for every window.
    clipboard: SharedClipboard,
    text: TextCounters,
}

impl Default for ShellCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellCtx {
    /// A context with nothing asked for yet, for an embedder running its own loop. Its
    /// clipboard is a [`MemoryClipboard`] until [`with_clipboard`](Self::with_clipboard).
    pub fn new() -> Self {
        Self {
            requests: Vec::new(),
            next: 0,
            clipboard: Rc::new(RefCell::new(Box::new(MemoryClipboard::default()))),
            text: TextCounters::default(),
        }
    }

    /// The same context, copying to and pasting from `clipboard`.
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Box<dyn Clipboard>) -> Self {
        self.clipboard = Rc::new(RefCell::new(clipboard));
        self
    }

    /// The clipboard text widgets copy to and paste from; do not hold it across an event.
    pub fn clipboard(&mut self) -> RefMut<'_, Box<dyn Clipboard>> {
        self.clipboard.borrow_mut()
    }

    /// The clipboard's cell, for a driver to hand to what copies outside the text widgets.
    pub fn shared_clipboard(&self) -> SharedClipboard {
        self.clipboard.clone()
    }

    /// What the host has done with text so far.
    pub fn text_counters(&self) -> TextCounters {
        self.text
    }

    pub(crate) fn text_counters_mut(&mut self) -> &mut TextCounters {
        &mut self.text
    }

    /// Asks for a window and names it; the window appears when the loop gets control back.
    pub fn open_window(&mut self, config: WindowConfig, root: NewWidget<dyn Widget>) -> WindowKey {
        let window = WindowKey(self.next);
        self.next += 1;
        self.requests
            .push(ShellRequest::Open(Box::new(NewWindow { window, config, root })));
        window
    }

    /// Names a window the shell did not open (an embedder's, a test's).
    pub fn name_window(&mut self) -> WindowKey {
        let window = WindowKey(self.next);
        self.next += 1;
        window
    }

    /// Asks for a window to be closed. Closing the last one ends the loop.
    pub fn close_window(&mut self, window: WindowKey) {
        self.requests.push(ShellRequest::Close(window));
    }

    /// Asks for a window to draw a frame: for a change another window has to follow.
    pub fn request_redraw(&mut self, window: WindowKey) {
        self.requests.push(ShellRequest::Redraw(window));
    }

    /// Takes what has been asked for, leaving the context empty.
    ///
    /// The shell's loop calls this itself; an embedder running its own loop has to.
    pub fn drain(&mut self) -> Vec<ShellRequest> {
        std::mem::take(&mut self.requests)
    }
}

/// What an application wants to hear about from the shell.
///
/// **One driver per process, and every method names the window it is about.** That is
/// how `masonry_winit::AppDriver` is shaped, but the reason here is detach: moving an
/// area from one window to another is an operation with two ends, and a driver that saw
/// one window could not express it. An application that wants per-window state keys it
/// by [`WindowKey`], which is the same thing the shell does.
pub trait ShellDriver {
    /// A widget emitted an action. The default drops it.
    fn on_action(&mut self, cx: &mut ShellCtx, window: WindowKey, action: ErasedAction, from: WidgetId) {
        let _ = (cx, window, action, from);
    }

    /// Called once per window, with its tree built and before its first event: where to set
    /// what lives on the root, such as `RenderRoot::set_focus_fallback`
    /// (`root.get_layer_root(0).id()` is the application's root widget).
    fn started(&mut self, cx: &mut ShellCtx, window: WindowKey, root: &mut RenderRoot) {
        let _ = (cx, window, root);
    }

    /// The event has been delivered and the tree has acted on it: where an application wakes
    /// the other windows that have to follow ([`ShellCtx::request_redraw`]). The seats run
    /// before the tree, too early for that.
    fn settled(&mut self, cx: &mut ShellCtx, window: WindowKey, root: &mut RenderRoot) {
        let _ = (cx, window, root);
    }

    /// The window is about to draw a frame: where it catches up with changes made in another
    /// window (a `mutate_later` does not cross windows).
    fn frame(&mut self, window: WindowKey, root: &mut RenderRoot) {
        let _ = (window, root);
    }

    /// The subtrees that are layers of their own, asked once per frame (for a screen of areas,
    /// `AreaScreen::area_ids()`): each is asked to repaint, and the presenter may keep its
    /// pixels. None by default.
    fn layers(&mut self, window: WindowKey, root: &mut RenderRoot) -> Vec<WidgetId> {
        let _ = (window, root);
        Vec::new()
    }

    /// The seat in front of the widget tree: every pointer event, before `RenderRoot`.
    /// [`Handled::Yes`] keeps it from the tree entirely.
    ///
    /// It does not know what is under the pointer, and asking (`RenderRoot::edit_widget`)
    /// costs a rewrite battery; a layer root's own hook is cheaper where it will do.
    fn pointer_event(
        &mut self,
        cx: &mut ShellCtx,
        window: WindowKey,
        root: &mut RenderRoot,
        event: &PointerEvent,
    ) -> Handled {
        let _ = (cx, window, root, event);
        Handled::No
    }

    /// The same seat, for keys.
    fn text_event(
        &mut self,
        cx: &mut ShellCtx,
        window: WindowKey,
        root: &mut RenderRoot,
        event: &TextEvent,
    ) -> Handled {
        let _ = (cx, window, root, event);
        Handled::No
    }
}

/// Does what a layer signal asks of `root` (a popup is a layer the host adds, from
/// [`RenderRootSignal::NewLayer`]), or hands the signal back if it is not one.
pub fn apply_layer_signal(root: &mut RenderRoot, signal: RenderRootSignal) -> Option<RenderRootSignal> {
    match signal {
        RenderRootSignal::NewLayer(_kind, widget, position) => root.add_layer(widget, position),
        RenderRootSignal::RemoveLayer(id) => root.remove_layer(id),
        RenderRootSignal::RepositionLayer(id, position) => root.reposition_layer(id, position),
        other => return Some(other),
    }
    None
}

/// Offers one pointer event to the host seat, then to the tree unless it was taken. Free,
/// so an embedder or a test gets what the shell's loop gets.
pub fn deliver_pointer(
    driver: &mut dyn ShellDriver,
    cx: &mut ShellCtx,
    window: WindowKey,
    root: &mut RenderRoot,
    event: PointerEvent,
) -> Handled {
    if driver.pointer_event(cx, window, root, &event).is_handled() {
        return Handled::Yes;
    }
    root.handle_pointer_event(event)
}

/// As [`deliver_pointer`], for keys and text. While a text field has focus, the paste key
/// becomes the clipboard's text here.
pub fn deliver_text(
    driver: &mut dyn ShellDriver,
    cx: &mut ShellCtx,
    window: WindowKey,
    root: &mut RenderRoot,
    event: TextEvent,
) -> Handled {
    let Some(event) = paste_or_key(cx, root, event) else {
        return Handled::Yes;
    };
    if driver.text_event(cx, window, root, &event).is_handled() {
        return Handled::Yes;
    }
    root.handle_text_event(event)
}

impl ShellDriver for () {}

impl<F: FnMut(ErasedAction, WidgetId)> ShellDriver for F {
    fn on_action(&mut self, _cx: &mut ShellCtx, _window: WindowKey, action: ErasedAction, from: WidgetId) {
        self(action, from);
    }
}

/// Why the shell could not run.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No rasteriser could be opened.
    Backend(crate::BackendError),
    /// A frame could not be composed, rasterised or shown (the platform's error as text).
    Presented(PresentError),
    /// The event loop refused to start or to run.
    EventLoop(winit::error::EventLoopError),
    /// The platform refused to create the window.
    Os(winit::error::OsError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(error) => write!(f, "{error}"),
            Self::Presented(error) => write!(f, "{error}"),
            Self::EventLoop(error) => write!(f, "{error}"),
            Self::Os(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(error) => Some(error),
            Self::Presented(error) => Some(error),
            Self::EventLoop(error) => Some(error),
            Self::Os(error) => Some(error),
        }
    }
}

impl From<crate::BackendError> for Error {
    fn from(error: crate::BackendError) -> Self {
        Self::Backend(error)
    }
}

impl From<PresentError> for Error {
    fn from(error: PresentError) -> Self {
        Self::Presented(error)
    }
}

/// Runs an application in its own window until it exits.
///
/// # Errors
///
/// If the event loop cannot start, or the window or its presenter cannot be created. A
/// frame that fails later is logged.
pub fn run(
    config: WindowConfig,
    root: NewWidget<dyn Widget>,
    default_properties: DefaultProperties,
    driver: impl ShellDriver + 'static,
) -> Result<(), Error> {
    // Tracing, if the application has none, as `masonry_winit` does.
    let _ = masonry::app::try_init_tracing();

    let event_loop = EventLoop::new().map_err(Error::EventLoop)?;
    // The first window is asked for like any other.
    let mut cx = ShellCtx::new().with_clipboard(system_clipboard());
    cx.open_window(config, root);
    let mut app = ShellApp {
        windows: HashMap::new(),
        routes: HashMap::new(),
        default_properties: Arc::new(default_properties),
        driver: Box::new(driver),
        cx,
        failure: None,
    };
    event_loop.run_app(&mut app).map_err(Error::EventLoop)?;
    app.failure.map_or(Ok(()), Err)
}

// --- MARK: BLIT

/// Rasterises the frame into a buffer and copies it into the window: works with any
/// [`Backend`] and no graphics device, at the cost of a pass through main memory.
pub struct BlitPresenter {
    host: Host,
    backend: Backend,
    surface: softbuffer::Surface<Arc<Window>, Arc<Window>>,
    holes: Vec<Hole>,
    counters: PresentCounters,
}

impl BlitPresenter {
    /// Opens `backend` and a `softbuffer` surface on `window`.
    pub fn new(backend: Backend, window: Arc<Window>, background: Color) -> Result<Self, Error> {
        let platform = |error: softbuffer::SoftBufferError| Error::Presented(PresentError::Platform(error.to_string()));
        let host = Host::new(backend).map_err(Error::Backend)?.with_background(background);
        let context = softbuffer::Context::new(window.clone()).map_err(platform)?;
        let surface = softbuffer::Surface::new(&context, window).map_err(platform)?;
        Ok(Self {
            host,
            backend,
            surface,
            holes: Vec::new(),
            counters: PresentCounters::default(),
        })
    }
}

impl Presenter for BlitPresenter {
    fn name(&self) -> &'static str {
        "blit"
    }

    fn present(
        &mut self,
        plan: &VisualLayerPlan,
        frame: PhysicalSize<u32>,
        device_scale: f64,
    ) -> Result<(), PresentError> {
        self.host.set_device_scale(device_scale);
        let frame = self.host.render_sized(plan, frame).map_err(PresentError::Host)?;

        self.holes.clear();
        self.holes.extend_from_slice(&frame.holes);
        blit(&mut self.surface, &frame.image)?;

        // Counted once the frame reached the window.
        self.counters.frames += 1;
        self.counters.holes += frame.holes.len() as u64;
        self.counters.cpu_bytes = self.host.counters().image_bytes;
        self.counters.readbacks += u64::from(self.backend.needs_device());
        Ok(())
    }

    fn holes(&self) -> &[Hole] {
        &self.holes
    }

    fn counters(&self) -> PresentCounters {
        self.counters
    }

    fn resize(&mut self, _size: PhysicalSize<u32>) {
        // `softbuffer` is resized when the frame is presented.
    }
}

/// One window the shell holds, with its own presenter, device, rasteriser and layer cache.
struct ShellWindow {
    window: Arc<Window>,
    presenter: Box<dyn Presenter>,
    root: RenderRoot,
    /// Where this root drops its signals, shared with the sink closure it owns.
    signals: Rc<RefCell<Vec<RenderRootSignal>>>,
    reducer: WindowEventReducer,
    /// When the last animation frame ran, for the next one's interval; `None` after a frame
    /// that did not animate, so the first frame after idling gets zero, as Masonry promises.
    last_anim: Option<Instant>,
    /// Whether the last frame was refused by the rasteriser, to warn once per change.
    refusing: bool,
}

struct ShellApp {
    /// The windows, by the name the driver knows them under.
    windows: HashMap<WindowKey, ShellWindow>,
    /// Which of them a winit event belongs to.
    routes: HashMap<WindowId, WindowKey>,
    /// The theme, shared by every window: one map per application.
    default_properties: Arc<DefaultProperties>,
    driver: Box<dyn ShellDriver>,
    /// What the driver has asked for and the loop has not done yet.
    cx: ShellCtx,
    /// Why a window could not be opened, handed back by [`run`] once the loop stops (the
    /// handlers cannot return errors).
    failure: Option<Error>,
}

impl ShellApp {
    /// Does what the driver asked for, between events.
    fn apply_requests(&mut self, event_loop: &ActiveEventLoop) {
        for request in self.cx.drain() {
            match request {
                ShellRequest::Open(new) => {
                    if let Err(error) = self.open(event_loop, new.window, new.config, new.root) {
                        self.failure = Some(error);
                        event_loop.exit();
                        return;
                    }
                },
                ShellRequest::Close(window) => self.close(event_loop, window),
                ShellRequest::Redraw(window) => {
                    if let Some(shell) = self.windows.get(&window) {
                        shell.window.request_redraw();
                    }
                },
            }
        }
    }

    /// Creates a window, its surface and its render root.
    fn open(
        &mut self,
        event_loop: &ActiveEventLoop,
        key: WindowKey,
        config: WindowConfig,
        root: NewWidget<dyn Widget>,
    ) -> Result<(), Error> {
        let mut attributes = Window::default_attributes()
            .with_title(config.title.clone())
            .with_resizable(config.resizable)
            .with_inner_size(config.size);
        if let Some(min) = config.min_size {
            attributes = attributes.with_min_inner_size(min);
        }
        let window = Arc::new(event_loop.create_window(attributes).map_err(Error::Os)?);
        let scale_factor = window.scale_factor();
        let presenter = open_presenter(&config, &window)?;
        tracing::info!(presenter = presenter.name(), window = key.0, "blazy shell");

        let signals: Rc<RefCell<Vec<RenderRootSignal>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = signals.clone();
        let mut render_root = RenderRoot::new(root, move |signal| sink.borrow_mut().push(signal), RenderRootOptions {
            default_properties: self.default_properties.clone(),
            use_system_fonts: true,
            size_policy: WindowSizePolicy::User,
            size: window.inner_size(),
            scale_factor,
            test_font: None,
        });

        self.driver.started(&mut self.cx, key, &mut render_root);

        window.request_redraw();
        self.routes.insert(window.id(), key);
        self.windows.insert(key, ShellWindow {
            window,
            presenter,
            root: render_root,
            signals,
            reducer: WindowEventReducer::default(),
            last_anim: None,
            refusing: false,
        });
        Ok(())
    }

    /// Drops a window. The loop ends when the last one goes.
    fn close(&mut self, event_loop: &ActiveEventLoop, key: WindowKey) {
        if let Some(window) = self.windows.remove(&key) {
            self.routes.remove(&window.window.id());
        }
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }

    /// Composes, rasterises and presents one frame of one window, from the tree's plan;
    /// what the tree left to the host arrives as a hole.
    fn redraw(&mut self, key: WindowKey) -> Result<(), Error> {
        let Some(shell) = self.windows.get_mut(&key) else {
            return Ok(());
        };
        let root = &mut shell.root;

        // First: changes from another window have to land before the layout.
        self.driver.frame(key, root);

        if root.needs_anim() {
            let now = Instant::now();
            let interval = shell.last_anim.map_or(Duration::ZERO, |last| now.duration_since(last));
            shell.last_anim = Some(now);
            root.handle_window_event(WindowEvent::AnimFrame(interval));
        } else {
            shell.last_anim = None;
        }

        // Layers, before the plan is built: they must repaint, and the presenter must know.
        let layers = self.driver.layers(key, root);
        if !layers.is_empty() {
            for id in &layers {
                root.edit_widget(*id, |mut widget| widget.ctx.request_paint_only());
            }
            shell.presenter.cache_layers(layers);
        }

        let (plan, _tree_update) = root.redraw();

        // A scene the rasteriser cannot take skips the frame, with a warning.
        match shell.presenter.present(&plan, root.size(), shell.window.scale_factor()) {
            Ok(()) => {
                if shell.refusing {
                    shell.refusing = false;
                    tracing::info!(window = key.0, "the scene fits the rasteriser again");
                }
            },
            Err(error @ (PresentError::SceneTooLarge { .. } | PresentError::SceneTooDeep { .. })) => {
                if !shell.refusing {
                    shell.refusing = true;
                    tracing::warn!(
                        window = key.0,
                        "frame not drawn: {error}; the window keeps the last frame it had"
                    );
                }
                return Ok(());
            },
            Err(error) => return Err(Error::Presented(error)),
        }

        // Holes are reported; the host owns what goes in them.
        report_holes(shell.presenter.holes());

        shell.window.set_cursor(root.cursor_icon());
        Ok(())
    }

    /// Acts on everything every window's render root asked for during the last call.
    fn drain_signals(&mut self, event_loop: &ActiveEventLoop) {
        let keys: Vec<WindowKey> = self.windows.keys().copied().collect();
        for key in keys {
            let Some(shell) = self.windows.get_mut(&key) else {
                continue;
            };
            let signals: Vec<_> = shell.signals.borrow_mut().drain(..).collect();
            for signal in signals {
                match signal {
                    RenderRootSignal::Action(action, from) => {
                        self.driver.on_action(&mut self.cx, key, action, from);
                    },
                    RenderRootSignal::RequestRedraw | RenderRootSignal::RequestAnimFrame => {
                        if let Some(shell) = self.windows.get(&key) {
                            shell.window.request_redraw();
                        }
                    },
                    RenderRootSignal::SetCursor(cursor) => {
                        if let Some(shell) = self.windows.get(&key) {
                            shell.window.set_cursor(cursor);
                        }
                    },
                    RenderRootSignal::SetTitle(title) => {
                        if let Some(shell) = self.windows.get(&key) {
                            shell.window.set_title(&title);
                        }
                    },
                    RenderRootSignal::SetSize(size) => {
                        if let Some(shell) = self.windows.get(&key) {
                            let _ = shell.window.request_inner_size(size);
                        }
                    },
                    signal @ (RenderRootSignal::StartIme
                    | RenderRootSignal::EndIme
                    | RenderRootSignal::ImeMoved(..)
                    | RenderRootSignal::ClipboardStore(_)) => {
                        if let Some(shell) = self.windows.get(&key) {
                            let _ = apply_text_signal(&mut self.cx, shell.window.as_ref(), signal);
                        }
                    },
                    // The application exiting, not one window closing.
                    RenderRootSignal::Exit => event_loop.exit(),
                    signal @ (RenderRootSignal::NewLayer(..)
                    | RenderRootSignal::RemoveLayer(_)
                    | RenderRootSignal::RepositionLayer(..)) => {
                        if let Some(shell) = self.windows.get_mut(&key) {
                            let _ = apply_layer_signal(&mut shell.root, signal);
                            shell.window.request_redraw();
                        }
                    },
                    // Not handled yet: accessibility and window-manager gestures.
                    _ => {},
                }
            }
        }
        self.apply_requests(event_loop);
    }
}

/// Chooses how a window's frames reach the screen: the GPU path if asked for and
/// available, the blit path otherwise, with a warning.
fn open_presenter(config: &WindowConfig, window: &Arc<Window>) -> Result<Box<dyn Presenter>, Error> {
    let base = config.base_color;

    #[cfg(feature = "vello")]
    if config.backend.is_none_or(|backend| backend == Backend::Vello) {
        match crate::gpu::SwapchainPresenter::new(window.clone(), window.inner_size(), base) {
            Ok(presenter) => return Ok(Box::new(presenter)),
            Err(error) => tracing::warn!("falling back to the blit path: {error}"),
        }
    }

    // The fallback must not need a device.
    let backend = config
        .backend
        .filter(|backend| !backend.needs_device())
        .or_else(|| {
            crate::backend::COMPILED
                .iter()
                .copied()
                .find(|backend| !backend.needs_device())
        })
        .unwrap_or(Backend::VelloCpu);
    Ok(Box::new(BlitPresenter::new(backend, window.clone(), base)?))
}

/// The platform's clipboard, or an in-process one (with a warning) when there is none.
fn system_clipboard() -> Box<dyn Clipboard> {
    match SystemClipboard::open() {
        Ok(clipboard) => Box::new(clipboard),
        Err(error) => {
            tracing::warn!(%error, "no system clipboard; copy and paste stay inside the application");
            Box::new(MemoryClipboard::default())
        },
    }
}

/// Reports holes to whoever is watching. Filling them is the owner's business.
fn report_holes(holes: &[Hole]) {
    for hole in holes {
        tracing::trace!(?hole, "external content left to the host");
    }
}

/// Puts a rendered frame on the screen: one pass to drop alpha and reorder the channels.
fn blit(surface: &mut softbuffer::Surface<Arc<Window>, Arc<Window>>, image: &RgbaImage) -> Result<(), PresentError> {
    let platform = |error: softbuffer::SoftBufferError| PresentError::Platform(error.to_string());
    let (Some(width), Some(height)) = (NonZeroU32::new(image.width), NonZeroU32::new(image.height)) else {
        return Ok(());
    };
    surface.resize(width, height).map_err(platform)?;

    let mut buffer = surface.buffer_mut().map_err(platform)?;
    for (out, pixel) in buffer.iter_mut().zip(image.data.as_chunks::<4>().0) {
        *out = (u32::from(pixel[0]) << 16) | (u32::from(pixel[1]) << 8) | u32::from(pixel[2]);
    }
    buffer.present().map_err(platform)
}

impl ApplicationHandler for ShellApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // The first window is a request queued by `run`.
        self.apply_requests(event_loop);
        self.drain_signals(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WinitWindowEvent) {
        let Some(key) = self.routes.get(&id).copied() else {
            return;
        };
        let Some(shell) = self.windows.get_mut(&key) else {
            return;
        };
        let scale_factor = shell.window.scale_factor();

        // Input through the reducer `masonry_winit` uses, one per window.
        if let Some(translation) = shell.reducer.reduce(scale_factor, &event) {
            match translation {
                WindowEventTranslation::Keyboard(key_event) => {
                    deliver_text(
                        self.driver.as_mut(),
                        &mut self.cx,
                        key,
                        &mut shell.root,
                        TextEvent::Keyboard(key_event),
                    );
                },
                WindowEventTranslation::Pointer(pointer) => {
                    deliver_pointer(self.driver.as_mut(), &mut self.cx, key, &mut shell.root, pointer);
                },
            }
        }

        match event {
            // The window, not the process.
            WinitWindowEvent::CloseRequested => self.close(event_loop, key),
            WinitWindowEvent::Resized(size) => {
                shell.root.handle_window_event(WindowEvent::Resize(size));
                shell.presenter.resize(size);
            },
            WinitWindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The device scale goes into composition only: a frame, not a relayout.
                shell.root.handle_window_event(WindowEvent::Rescale(scale_factor));
            },
            // Composed text: not a key, so the reducer does not translate it.
            WinitWindowEvent::Ime(ime) => {
                deliver_text(self.driver.as_mut(), &mut self.cx, key, &mut shell.root, ime_event(ime));
            },
            WinitWindowEvent::Focused(focused) => {
                deliver_text(
                    self.driver.as_mut(),
                    &mut self.cx,
                    key,
                    &mut shell.root,
                    TextEvent::WindowFocusChange(focused),
                );
            },
            WinitWindowEvent::RedrawRequested => {
                if let Err(error) = self.redraw(key) {
                    tracing::error!(window = key.0, "{error}");
                }
            },
            _ => {},
        }

        if let Some(shell) = self.windows.get_mut(&key) {
            self.driver.settled(&mut self.cx, key, &mut shell.root);
        }

        self.drain_signals(event_loop);
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Only an animation (or an external hole) asks for the next frame, per window.
        for shell in self.windows.values() {
            if shell.root.needs_anim() {
                shell.window.request_redraw();
            }
        }
    }
}
