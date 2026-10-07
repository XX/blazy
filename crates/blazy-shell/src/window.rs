//! Owner mode: our own windows, our own event loop, our own frames on the screen.
//!
//! `rnd/architecture.md` §14 gives the host two modes; this is the first. The second —
//! guest mode, where an engine hands us its device and its texture — is nearly free
//! from upstream and waits for something real to embed into (§26.5).
//!
//! # Several windows
//!
//! The loop holds a map of them, each with its own `RenderRoot`, presenter, device and
//! layer cache — everything per window, which is a decision with numbers behind it
//! (§44): a second window costs 123 ms to open, of which 95 are the rasteriser's
//! pipelines, and sharing a device would save the other 28.
//!
//! One [`ShellDriver`] for the process, and every method names the window it is about
//! ([`WindowKey`]). A driver asks for windows through [`ShellCtx`], which also carries
//! the one thing one window can do to another: ask it to draw, because an idle window
//! cannot notice a change made elsewhere (§36, §30).
//!
//! # Why not `masonry_winit`
//!
//! Because the piece we have to replace is inside the piece we would want to keep
//! (§26.3): `MasonryState` owns both the event loop and a rasteriser chosen at
//! compile time, and its event conversion is `pub(crate)`. What we do keep is the
//! part upstream itself keeps outside: `ui_events_winit::WindowEventReducer` is the
//! same public crate `masonry_winit` uses to turn winit events into `ui-events`, so
//! this module does not reimplement input handling — it wires it.
//!
//! `RenderRoot` is public in full, so the loop below is a plain winit application
//! that feeds it events and asks it for a plan.
//!
//! # How a frame reaches the screen
//!
//! Two ways, chosen at startup along with the rasteriser (§27):
//!
//! * [`SwapchainPresenter`](crate::gpu::SwapchainPresenter) — the scene is drawn into a texture and blitted into the
//!   swapchain. The frame never enters main memory. Needs a graphics device, so it comes with the `vello` feature.
//! * [`BlitPresenter`] — the frame is rasterised into a buffer and copied into the window with `softbuffer`. Works
//!   anywhere, including on a machine with no usable GPU, and costs one pass over the frame plus the platform's own
//!   copy.
//!
//! The second is the fallback for the first: asking for the GPU path on a machine
//! that cannot give it is answered with a warning and a working window, not with a
//! failure to start.
//!
//! # What this does not do yet
//!
//! IME, the clipboard, accessibility, and window-manager gestures — dragging by a
//! custom title bar, resize handles, the system menu. Masonry emits signals for all
//! of them and this loop ignores those signals rather than pretending: each is a
//! platform integration of its own, and none is on the critical path for the
//! question this crate exists to answer. Layers are not among them: a popup is a layer,
//! and [`apply_layer_signal`] puts it in the window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

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
    /// What the window shows where the widget tree drew nothing.
    ///
    /// Not decoration: the frame has to be opaque by the time it is presented. A
    /// widget tree is not obliged to cover the window — `AreaScreen` paints only its
    /// splitter bars — so without a base colour every anti-aliased edge is blended
    /// against transparency and then flattened, which throws the blending away and
    /// turns a curve into a staircase. Compositing here rather than in the blit also
    /// keeps presentation a plain channel swap.
    pub base_color: Color,
    /// Which rasteriser to use, or `None` for the first one that opens.
    ///
    /// The point of the whole registry: an application decides this at startup, from
    /// a flag or a settings file, not by being rebuilt (§26.2).
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

/// The name of a window, handed out by the shell.
///
/// Ours rather than winit's, and that is what makes it usable: a driver names a window
/// *before* it exists — [`ShellCtx::open_window`] answers immediately and the window is
/// created once the current event is done with — and the name it got then is the name it
/// keeps. Routing winit's own id to this one is the shell's business.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowKey(u64);

/// What a driver asked the shell to do.
///
/// Public because [`deliver_pointer`] and [`deliver_text`] are: an embedder running its
/// own loop gets the same seat as the shell's loop, and therefore the same obligation to
/// honour what the driver asked for. The shell's own loop drains these itself.
#[non_exhaustive]
pub enum ShellRequest {
    /// Open a window under the name the driver was given.
    ///
    /// Boxed because it is the heavy variant and `Close` is a number: an enum as wide as
    /// its widest variant would make every queued close carry a window's worth of bytes.
    Open(Box<NewWindow>),
    /// Close a window. Closing the last one ends the loop.
    Close(WindowKey),
    /// Draw a window that may be idle.
    ///
    /// The only way one window can reach another: a change made in one window has to
    /// wake the windows that have to follow it, and nothing else will — an idle window
    /// stays idle by design (§36).
    Redraw(WindowKey),
}

/// A window that has been asked for and not yet created.
pub struct NewWindow {
    /// The name it will answer to, already valid.
    pub window: WindowKey,
    /// How it should be created.
    pub config: WindowConfig,
    /// Its root widget. A tree cannot be moved between windows, so this is a fresh one:
    /// `RenderRoot` owns its arena and upstream has no reparenting.
    pub root: NewWidget<dyn Widget>,
}

/// What a driver may ask the shell for.
///
/// Passed to every method that could reasonably want a second window — which is every
/// method that hears from the user. The requests are queued rather than performed on the
/// spot because creating a window needs the event loop, and the driver is called from
/// inside an event it is already holding.
pub struct ShellCtx {
    requests: Vec<ShellRequest>,
    next: u64,
}

impl Default for ShellCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellCtx {
    /// A context with nothing asked for yet. For an embedder running its own loop.
    pub fn new() -> Self {
        Self {
            requests: Vec::new(),
            next: 0,
        }
    }

    /// Asks for a window, and names it.
    ///
    /// The key is valid from here on: it is what the driver's later calls are tagged
    /// with, and what [`close_window`](Self::close_window) takes. The window itself
    /// appears when the shell gets back to the event loop.
    pub fn open_window(&mut self, config: WindowConfig, root: NewWidget<dyn Widget>) -> WindowKey {
        let window = WindowKey(self.next);
        self.next += 1;
        self.requests
            .push(ShellRequest::Open(Box::new(NewWindow { window, config, root })));
        window
    }

    /// Names a window the shell did not open.
    ///
    /// For an embedder running its own loop, and for a test: [`deliver_pointer`] and
    /// [`deliver_text`] take the name of the window an event came from, and a window
    /// somebody else created still has to have one.
    pub fn name_window(&mut self) -> WindowKey {
        let window = WindowKey(self.next);
        self.next += 1;
        window
    }

    /// Asks for a window to be closed. Closing the last one ends the loop.
    pub fn close_window(&mut self, window: WindowKey) {
        self.requests.push(ShellRequest::Close(window));
    }

    /// Asks for a window to draw a frame.
    ///
    /// For a change that one window made and another has to follow: the model is shared
    /// and the views are not, and a view that is not drawing cannot notice anything
    /// (§30, and the pull it takes across a window boundary).
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

    /// Called once per window, with its tree built and before its first event.
    ///
    /// The window's own startup, which an application cannot do for itself: a
    /// `RenderRoot` does not exist until the window does, and some of what an
    /// application needs is settled on the root rather than on a widget. The keymap is
    /// the case that made this necessary — Masonry sends a key to the focused widget
    /// or to `RenderRoot::set_focus_fallback` and nowhere else, so a driver that wants
    /// the keys nobody claimed has to name a widget here (§38.3).
    ///
    /// `root.get_layer_root(0).id()` is the application's own root widget, which saves
    /// threading a `WidgetId` out of a tree that has not been built yet.
    fn started(&mut self, cx: &mut ShellCtx, window: WindowKey, root: &mut RenderRoot) {
        let _ = (cx, window, root);
    }

    /// The event has been delivered, and whatever it changed has changed.
    ///
    /// The place to notice that *another* window has to follow: a change made here waits
    /// in the application's model, and a window that is not drawing will not collect it
    /// (§36 — an idle window is idle on purpose). So this is where an application asks
    /// for the frames that are needed, with [`ShellCtx::request_redraw`].
    ///
    /// **After the event rather than in one of the seats**, because the seats are offered
    /// an event *before* the tree acts on it: a driver that asked "is there anything to
    /// carry" from [`pointer_event`](Self::pointer_event) would be asking about the
    /// previous event, and a change made with the mouse would reach the other window one
    /// gesture late — which is exactly what it did before this existed.
    fn settled(&mut self, cx: &mut ShellCtx, window: WindowKey, root: &mut RenderRoot) {
        let _ = (cx, window, root);
    }

    /// The window is about to draw a frame.
    ///
    /// Where an application catches a window up with whatever happened in another one.
    /// Nothing else can do it: a driver is called with the root of the window the event
    /// arrived at, and a widget in one `RenderRoot` cannot be reached from another — the
    /// `mutate_later` that carries a change between the areas of one window is silently
    /// dropped across the boundary of two. So the change waits in the application's model
    /// and this is where the window collects it.
    ///
    /// Only on frames that are happening anyway. A window that has to be woken is woken
    /// with [`ShellCtx::request_redraw`].
    fn frame(&mut self, window: WindowKey, root: &mut RenderRoot) {
        let _ = (window, root);
    }

    /// The subtrees that are layers of their own, asked for once per frame (§36).
    ///
    /// Returning ids does two things, and both are needed for either to be worth
    /// anything: the shell asks each of those widgets to repaint, without which its
    /// layer disappears on the first frame it is idle (§26.1), and it tells the
    /// presenter it may keep their pixels. For a screen of areas this is
    /// `AreaScreen::area_ids()`.
    ///
    /// Only ever called on frames that are happening anyway, so an application that
    /// declares layers does not stop the window from going idle. The default declares
    /// none, and then nothing above happens at all.
    ///
    /// No [`ShellCtx`] here, unlike the methods that hear from the user: this one is on
    /// the frame path, and opening a window from inside a frame is not a thing an
    /// application should be able to do by accident.
    fn layers(&mut self, window: WindowKey, root: &mut RenderRoot) -> Vec<WidgetId> {
        let _ = (window, root);
        Vec::new()
    }

    /// The seat in front of the widget tree: every pointer event, before `RenderRoot`
    /// is called, with the power to keep it.
    ///
    /// Returning [`Handled::Yes`] means the tree never sees the event at all — not the
    /// widget under the pointer, not its ancestors, not a layer root's own hook. This is
    /// the seat §38.2 priced and §39.5 moved out of a benchmark and into the library.
    ///
    /// What it buys and what it costs, both measured (§38.2): it is the only seat that
    /// can withhold *anything*, including events no widget would have handled, and it is
    /// the only one that does not know what is under the pointer. Asking costs
    /// `RenderRoot::edit_widget` and the rewrite battery that follows it, at 41.2 probes
    /// per gesture — which is why an application with a layer root of its own should
    /// prefer that seat and keep this one for what only it can do.
    ///
    /// A gesture never crosses a window: pointer capture belongs to one `RenderRoot`, so
    /// while a button is down its events go to the window that took it.
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
    ///
    /// Keys never reach a widget that has not been made the focus fallback, so this is
    /// also the seat that can hear a key when the tree would have dropped it (§38.3).
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

/// Does what a layer signal asks of `root`, or hands the signal back if it is not one.
///
/// A popup in Masonry — a menu, a tooltip, a selector's list — is a layer, and a layer is
/// not created by the widget that wants it: the widget emits
/// [`RenderRootSignal::NewLayer`] and the host puts the root into the window's stack. This
/// loop used to drop those signals with IME and the clipboard, so no popup ever appeared in
/// a blazy window — and no test noticed, because the test harness handles them itself
/// (`issues/menus.md`). A free function over the root, so an embedder running its own loop
/// does the same, and a test can check it without a window.
pub fn apply_layer_signal(root: &mut RenderRoot, signal: RenderRootSignal) -> Option<RenderRootSignal> {
    match signal {
        RenderRootSignal::NewLayer(_kind, widget, position) => root.add_layer(widget, position),
        RenderRootSignal::RemoveLayer(id) => root.remove_layer(id),
        RenderRootSignal::RepositionLayer(id, position) => root.reposition_layer(id, position),
        other => return Some(other),
    }
    None
}

/// Offers one pointer event to the host seat, and then to the tree unless it was taken.
///
/// The whole of the host seat, and it is a free function so that an embedder running its
/// own loop — or a test with no window at all — gets exactly what the shell's loop gets.
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

/// As [`deliver_pointer`], for keys.
pub fn deliver_text(
    driver: &mut dyn ShellDriver,
    cx: &mut ShellCtx,
    window: WindowKey,
    root: &mut RenderRoot,
    event: TextEvent,
) -> Handled {
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
    /// A frame could not be composed, rasterised or shown.
    ///
    /// The platform's own error arrives as text rather than as its type: which crate
    /// puts pixels on the screen is an implementation detail of a presenter, and a
    /// library that leaks it into its public error type passes every upstream rename
    /// on to everyone downstream (§15.1).
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
/// If the event loop cannot start, or the window or the path its frames take to the
/// screen cannot be created. A frame that fails later is logged rather than returned:
/// by then the application is running, and one bad frame is not a reason to end it.
pub fn run(
    config: WindowConfig,
    root: NewWidget<dyn Widget>,
    default_properties: DefaultProperties,
    driver: impl ShellDriver + 'static,
) -> Result<(), Error> {
    // The same courtesy `masonry_winit` extends: if the application has not set up
    // tracing, set up ours, so that a fallback to the blit path or a lost swapchain
    // texture says so out loud instead of disappearing.
    let _ = masonry::app::try_init_tracing();

    let event_loop = EventLoop::new().map_err(Error::EventLoop)?;
    // The first window is asked for exactly the way a second one is, so there is one
    // path that opens a window and not two that drift apart.
    let mut cx = ShellCtx::new();
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

/// Rasterises the frame into a buffer and copies it into the window.
///
/// The path that works everywhere: any [`Backend`], no graphics device required. What
/// it costs is that the frame passes through main memory — measured at 0.94 ms for the
/// channel swap alone on an 1100x750 frame — and that a GPU rasteriser has to read its
/// own output back (§26.2). The swapchain path exists to avoid both; this one exists
/// because a machine without a usable device still has to show a window.
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

        // Counted after the frame reached the window, so a frame that failed on the
        // way is not in the denominator of a per-frame criterion.
        self.counters.frames += 1;
        self.counters.holes += frame.holes.len() as u64;
        // Read from the host rather than recomputed: the bytes exist because the host
        // made them, and a second place that works them out is a second place to be
        // wrong (§27.4).
        self.counters.cpu_bytes = self.host.counters().image_bytes;
        // A GPU rasteriser on this path has to copy its result out of video memory
        // once per frame; a CPU one wrote into main memory to begin with.
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
        // `softbuffer` is resized when the frame is presented, from the frame's own
        // size, so there is nothing to do here — and nothing to get out of step.
    }
}

/// One window the shell holds.
///
/// Everything in here is per window and stays per window (decision 2 of the detach
/// task): its own presenter, and with it its own device, rasteriser and layer cache with
/// its own ceiling (§37.2). Two windows cost two of each, once, when they open.
struct ShellWindow {
    window: Arc<Window>,
    presenter: Box<dyn Presenter>,
    root: RenderRoot,
    /// Where this root drops its signals.
    ///
    /// Shared with the sink closure the root owns, because that closure cannot borrow
    /// the structure that owns the root. One queue per window, so a signal cannot be
    /// acted on against the wrong one.
    signals: Rc<RefCell<Vec<RenderRootSignal>>>,
    reducer: WindowEventReducer,
    /// When the last animation frame ran, for the interval the next one gets.
    last_anim: Instant,
    /// Whether the last frame was refused by the rasteriser (§33).
    ///
    /// Kept so the warning is logged when the state changes rather than sixty times a
    /// second: a scene over the tile budget stays over it while nothing moves.
    refusing: bool,
}

struct ShellApp {
    /// The windows, by the name the driver knows them under.
    windows: HashMap<WindowKey, ShellWindow>,
    /// Which of them a winit event belongs to.
    routes: HashMap<WindowId, WindowKey>,
    /// The theme, shared by every window: one map per application (§22.1).
    default_properties: Arc<DefaultProperties>,
    driver: Box<dyn ShellDriver>,
    /// What the driver has asked for and the loop has not done yet.
    cx: ShellCtx,
    /// Why a window could not be opened, handed back by [`run`] once the loop stops.
    ///
    /// Kept rather than raised where it happens: the handler methods have no way to
    /// return an error, and a panic there unwinds through the platform's event loop and
    /// takes the `Result` that `run` promises its caller with it.
    failure: Option<Error>,
}

impl ShellApp {
    /// Does what the driver asked for while it was being called.
    ///
    /// Between events rather than during them: creating a window needs the event loop,
    /// and the driver is called from inside an event that is already holding it.
    fn apply_requests(&mut self, event_loop: &ActiveEventLoop) {
        for request in self.cx.drain() {
            match request {
                ShellRequest::Open(new) => {
                    if let Err(error) = self.open(event_loop, new.window, new.config, new.root) {
                        // The first window failing is the application failing to start;
                        // a later one is the same kind of failure and is reported the
                        // same way, because there is nobody else to tell.
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
            last_anim: Instant::now(),
            refusing: false,
        });
        Ok(())
    }

    /// Drops a window. The loop ends when the last one goes.
    ///
    /// Closing a window is not exiting: that distinction is what multi-window costs and
    /// the whole of what the single-window loop got away without.
    fn close(&mut self, event_loop: &ActiveEventLoop, key: WindowKey) {
        if let Some(window) = self.windows.remove(&key) {
            self.routes.remove(&window.window.id());
        }
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }

    /// Composes, rasterises and presents one frame of one window.
    ///
    /// The whole of §4.2 variant 2 in one function: ask the tree for a plan, walk the
    /// plan rather than flattening it, and put the result on the screen. Anything the
    /// tree left to the host arrives as a hole and is handed to `fill_holes`.
    fn redraw(&mut self, key: WindowKey) -> Result<(), Error> {
        let Some(shell) = self.windows.get_mut(&key) else {
            return Ok(());
        };
        let root = &mut shell.root;

        // Before anything this frame is decided: an application may have a change from
        // another window to apply, and it has to land before the layout that draws it.
        self.driver.frame(key, root);

        if root.needs_anim() {
            let now = Instant::now();
            let interval = now.duration_since(shell.last_anim);
            shell.last_anim = now;
            root.handle_window_event(WindowEvent::AnimFrame(interval));
        }

        // Layers, before the plan is built: a widget that wants to be one has to be
        // painting when the paint pass reaches it (§26.1), and the presenter has to
        // know which pixels it may keep (§36). Both from one answer, so an application
        // cannot ask for half of the arrangement.
        let layers = self.driver.layers(key, root);
        if !layers.is_empty() {
            for id in &layers {
                root.edit_widget(*id, |mut widget| widget.ctx.request_paint_only());
            }
            shell.presenter.cache_layers(layers);
        }

        let (plan, _tree_update) = root.redraw();

        // The window's size in physical pixels is what the frame has to cover, and it
        // is known exactly; the scale factor goes along separately because it belongs
        // to the drawing rather than to the size (§9).
        // A scene the rasteriser cannot take is not a reason to close the window: the
        // frame is skipped, the window keeps what it had, and — unlike the silent
        // version this replaces (§33) — somebody is told.
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

        // Holes are reported rather than drawn: the host owns what goes in them, and
        // for an application without external content there are none (§4.3).
        report_holes(shell.presenter.holes());

        shell.window.set_cursor(root.cursor_icon());
        Ok(())
    }

    /// Acts on everything the render roots asked for during the last call into them.
    ///
    /// Every window's queue, not only the one an event arrived for: a root may be made
    /// to ask for something by a `mutate_later` scheduled from another window, which is
    /// exactly what carrying a model change into the other views does (§30).
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
                    // The application exiting, rather than one window closing: a window
                    // goes through `ShellCtx::close_window` or its own close button.
                    RenderRootSignal::Exit => event_loop.exit(),
                    signal @ (RenderRootSignal::NewLayer(..)
                    | RenderRootSignal::RemoveLayer(_)
                    | RenderRootSignal::RepositionLayer(..)) => {
                        if let Some(shell) = self.windows.get_mut(&key) {
                            let _ = apply_layer_signal(&mut shell.root, signal);
                            shell.window.request_redraw();
                        }
                    },
                    // Deliberately unhandled, and listed in the module docs: IME, the
                    // clipboard, accessibility and window-manager gestures are platform
                    // integrations of their own.
                    _ => {},
                }
            }
        }
        self.apply_requests(event_loop);
    }
}

/// Chooses how a window's frames will reach the screen.
///
/// The GPU path when it was asked for and the machine can give it; the blit path
/// otherwise. A machine with no usable device gets a warning and a working
/// window, because "start without a GPU" is a requirement and not a courtesy.
///
/// A free function rather than a method, because every window answers it for itself:
/// its own device, its own rasteriser, its own layer cache (decision 2).
fn open_presenter(config: &WindowConfig, window: &Arc<Window>) -> Result<Box<dyn Presenter>, Error> {
    let base = config.base_color;

    #[cfg(feature = "vello")]
    if config.backend.is_none_or(|backend| backend == Backend::Vello) {
        match crate::gpu::SwapchainPresenter::new(window.clone(), window.inner_size(), base) {
            Ok(presenter) => return Ok(Box::new(presenter)),
            Err(error) => tracing::warn!("falling back to the blit path: {error}"),
        }
    }

    // Whatever was asked for, the fallback has to be a rasteriser that does not
    // need a device: arriving here after the GPU path failed usually means there
    // is no usable device, and answering "no GPU" by asking for one again is how
    // a promise of "starts without a GPU" turns into a window that never opens.
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

/// Reports holes to whoever is watching. Filling them is the owner's business.
fn report_holes(holes: &[Hole]) {
    for hole in holes {
        tracing::trace!(?hole, "external content left to the host");
    }
}

/// Puts a rendered frame on the screen.
///
/// One pass over the pixels to drop alpha and reorder the channels, which is what a
/// CPU presentation path costs. §26.2 explains why the frame arrives as pixels rather
/// than as a texture, and §26.5 what it would take for it not to.
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
        // The first window is a request like any other, queued by `run`; anything the
        // driver asked for while it was being started goes out in the same drain.
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

        // Input first, through the same reducer `masonry_winit` uses (§26.3). One
        // reducer per window: it accumulates pointer state, and two windows have two
        // pointers as far as it is concerned.
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
            // The window, not the process: the loop ends when the last window goes
            // (`close`), which is the whole difference between one window and several.
            WinitWindowEvent::CloseRequested => self.close(event_loop, key),
            WinitWindowEvent::Resized(size) => {
                shell.root.handle_window_event(WindowEvent::Resize(size));
                shell.presenter.resize(size);
            },
            WinitWindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The third multiplier of §9, and the only place it is applied: the
                // tree keeps laying out in logical coordinates and the composition
                // scales the drawing, so a display change costs a frame and not a
                // relayout.
                shell.root.handle_window_event(WindowEvent::Rescale(scale_factor));
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

        // The event is over and the tree has acted on it: an application that has to move
        // something into another window says so now (see `ShellDriver::settled`).
        if let Some(shell) = self.windows.get_mut(&key) {
            self.driver.settled(&mut self.cx, key, &mut shell.root);
        }

        self.drain_signals(event_loop);
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // An animation frame is a reason to draw the next one; anything else waits
        // for an event. This is what keeps an idle window idle — apart from an
        // external hole, which by §26.1 has to keep painting to stay a hole. Asked of
        // each window separately, so one window animating does not wake the others.
        for shell in self.windows.values() {
            if shell.root.needs_anim() {
                shell.window.request_redraw();
            }
        }
    }
}
