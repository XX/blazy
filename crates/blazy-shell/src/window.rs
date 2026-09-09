//! Owner mode: our own window, our own event loop, our own frame on the screen.
//!
//! `rnd/architecture.md` §14 gives the host two modes; this is the first. The second —
//! guest mode, where an engine hands us its device and its texture — is nearly free
//! from upstream and waits for something real to embed into (§26.5).
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
//! question this crate exists to answer.

use std::cell::RefCell;
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

/// What an application wants to hear about from the shell.
///
/// One method, and a blanket no-op, because the only thing a Masonry application
/// strictly has to handle is a widget's action. Everything else the window knows is
/// available through the widget tree.
pub trait ShellDriver {
    /// A widget emitted an action. The default drops it.
    fn on_action(&mut self, action: ErasedAction, from: WidgetId) {
        let _ = (action, from);
    }

    /// Called once, with the tree built and before the first event.
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
    fn started(&mut self, root: &mut RenderRoot) {
        let _ = root;
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
    fn layers(&mut self, root: &mut RenderRoot) -> Vec<WidgetId> {
        let _ = root;
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
    fn pointer_event(&mut self, root: &mut RenderRoot, event: &PointerEvent) -> Handled {
        let _ = (root, event);
        Handled::No
    }

    /// The same seat, for keys.
    ///
    /// Keys never reach a widget that has not been made the focus fallback, so this is
    /// also the seat that can hear a key when the tree would have dropped it (§38.3).
    fn text_event(&mut self, root: &mut RenderRoot, event: &TextEvent) -> Handled {
        let _ = (root, event);
        Handled::No
    }
}

/// Offers one pointer event to the host seat, and then to the tree unless it was taken.
///
/// The whole of the host seat, and it is a free function so that an embedder running its
/// own loop — or a test with no window at all — gets exactly what the shell's loop gets.
pub fn deliver_pointer(driver: &mut dyn ShellDriver, root: &mut RenderRoot, event: PointerEvent) -> Handled {
    if driver.pointer_event(root, &event).is_handled() {
        return Handled::Yes;
    }
    root.handle_pointer_event(event)
}

/// As [`deliver_pointer`], for keys.
pub fn deliver_text(driver: &mut dyn ShellDriver, root: &mut RenderRoot, event: TextEvent) -> Handled {
    if driver.text_event(root, &event).is_handled() {
        return Handled::Yes;
    }
    root.handle_text_event(event)
}

impl ShellDriver for () {}

impl<F: FnMut(ErasedAction, WidgetId)> ShellDriver for F {
    fn on_action(&mut self, action: ErasedAction, from: WidgetId) {
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
    let mut app = ShellApp {
        config,
        presenter: None,
        default_properties: Some(default_properties),
        root: Some(root),
        driver: Box::new(driver),
        window: None,
        render_root: None,
        signals: Rc::new(RefCell::new(Vec::new())),
        reducer: WindowEventReducer::default(),
        last_anim: Instant::now(),
        refusing: false,
    };
    event_loop.run_app(&mut app).map_err(Error::EventLoop)
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

struct ShellApp {
    config: WindowConfig,
    /// How the frame reaches the screen. Chosen once the window exists.
    presenter: Option<Box<dyn Presenter>>,
    default_properties: Option<DefaultProperties>,
    root: Option<NewWidget<dyn Widget>>,
    driver: Box<dyn ShellDriver>,
    window: Option<Arc<Window>>,
    render_root: Option<RenderRoot>,
    /// Where the render root drops its signals.
    ///
    /// Shared with the sink closure the root owns, because that closure cannot borrow
    /// the structure that owns the root.
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

impl ShellApp {
    /// Creates the window, the surface and the render root.
    fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<(), Error> {
        let mut attributes = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_resizable(self.config.resizable)
            .with_inner_size(self.config.size);
        if let Some(min) = self.config.min_size {
            attributes = attributes.with_min_inner_size(min);
        }
        let window = Arc::new(event_loop.create_window(attributes).map_err(Error::Os)?);
        let scale_factor = window.scale_factor();
        let presenter = self.open_presenter(&window)?;
        tracing::info!(presenter = presenter.name(), "blazy shell");

        let signals = self.signals.clone();
        let mut render_root = RenderRoot::new(
            self.root.take().expect("the root widget is taken once"),
            move |signal| signals.borrow_mut().push(signal),
            RenderRootOptions {
                default_properties: Arc::new(self.default_properties.take().expect("taken once")),
                use_system_fonts: true,
                size_policy: WindowSizePolicy::User,
                size: window.inner_size(),
                scale_factor,
                test_font: None,
            },
        );

        self.driver.started(&mut render_root);

        window.request_redraw();
        self.window = Some(window);
        self.presenter = Some(presenter);
        self.render_root = Some(render_root);
        Ok(())
    }

    /// Chooses how frames will reach the screen.
    ///
    /// The GPU path when it was asked for and the machine can give it; the blit path
    /// otherwise. A machine with no usable device gets a warning and a working
    /// window, because "start without a GPU" is a requirement and not a courtesy.
    fn open_presenter(&mut self, window: &Arc<Window>) -> Result<Box<dyn Presenter>, Error> {
        let base = self.config.base_color;

        #[cfg(feature = "vello")]
        if self.config.backend.is_none_or(|backend| backend == Backend::Vello) {
            match crate::gpu::SwapchainPresenter::new(window.clone(), window.inner_size(), base) {
                Ok(presenter) => return Ok(Box::new(presenter)),
                Err(error) => tracing::warn!("falling back to the blit path: {error}"),
            }
        }

        // Whatever was asked for, the fallback has to be a rasteriser that does not
        // need a device: arriving here after the GPU path failed usually means there
        // is no usable device, and answering "no GPU" by asking for one again is how
        // a promise of "starts without a GPU" turns into a window that never opens.
        let backend = self
            .config
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

    /// Composes, rasterises and presents one frame.
    ///
    /// The whole of §4.2 variant 2 in one function: ask the tree for a plan, walk the
    /// plan rather than flattening it, and put the result on the screen. Anything the
    /// tree left to the host arrives as a hole and is handed to `fill_holes`.
    fn redraw(&mut self) -> Result<(), Error> {
        let (Some(root), Some(window), Some(presenter)) =
            (self.render_root.as_mut(), self.window.as_ref(), self.presenter.as_mut())
        else {
            return Ok(());
        };

        if root.needs_anim() {
            let now = Instant::now();
            let interval = now.duration_since(self.last_anim);
            self.last_anim = now;
            root.handle_window_event(WindowEvent::AnimFrame(interval));
        }

        // Layers, before the plan is built: a widget that wants to be one has to be
        // painting when the paint pass reaches it (§26.1), and the presenter has to
        // know which pixels it may keep (§36). Both from one answer, so an application
        // cannot ask for half of the arrangement.
        let layers = self.driver.layers(root);
        if !layers.is_empty() {
            for id in &layers {
                root.edit_widget(*id, |mut widget| widget.ctx.request_paint_only());
            }
            presenter.cache_layers(layers);
        }

        let (plan, _tree_update) = root.redraw();

        // The window's size in physical pixels is what the frame has to cover, and it
        // is known exactly; the scale factor goes along separately because it belongs
        // to the drawing rather than to the size (§9).
        // A scene the rasteriser cannot take is not a reason to close the window: the
        // frame is skipped, the window keeps what it had, and — unlike the silent
        // version this replaces (§33) — somebody is told.
        match presenter.present(&plan, root.size(), window.scale_factor()) {
            Ok(()) => {
                if self.refusing {
                    self.refusing = false;
                    tracing::info!("the scene fits the rasteriser again");
                }
            },
            Err(error @ (PresentError::SceneTooLarge { .. } | PresentError::SceneTooDeep { .. })) => {
                if !self.refusing {
                    self.refusing = true;
                    tracing::warn!("frame not drawn: {error}; the window keeps the last frame it had");
                }
                return Ok(());
            },
            Err(error) => return Err(Error::Presented(error)),
        }

        // Holes are reported rather than drawn: the host owns what goes in them, and
        // for an application without external content there are none (§4.3).
        report_holes(presenter.holes());

        window.set_cursor(root.cursor_icon());
        Ok(())
    }

    /// Acts on everything the render root asked for during the last call into it.
    fn drain_signals(&mut self, event_loop: &ActiveEventLoop) {
        let signals: Vec<_> = self.signals.borrow_mut().drain(..).collect();
        for signal in signals {
            match signal {
                RenderRootSignal::Action(action, from) => self.driver.on_action(action, from),
                RenderRootSignal::RequestRedraw | RenderRootSignal::RequestAnimFrame => {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                },
                RenderRootSignal::SetCursor(cursor) => {
                    if let Some(window) = &self.window {
                        window.set_cursor(cursor);
                    }
                },
                RenderRootSignal::SetTitle(title) => {
                    if let Some(window) = &self.window {
                        window.set_title(&title);
                    }
                },
                RenderRootSignal::SetSize(size) => {
                    if let Some(window) = &self.window {
                        let _ = window.request_inner_size(size);
                    }
                },
                RenderRootSignal::Exit => event_loop.exit(),
                // Deliberately unhandled, and listed in the module docs: IME, the
                // clipboard, accessibility and window-manager gestures are platform
                // integrations of their own.
                _ => {},
            }
        }
    }
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
        if self.window.is_none() {
            self.start(event_loop).expect("the window could not be created");
            self.drain_signals(event_loop);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WinitWindowEvent) {
        let Some(scale_factor) = self.window.as_ref().map(|window| window.scale_factor()) else {
            return;
        };
        let Some(root) = self.render_root.as_mut() else {
            return;
        };

        // Input first, through the same reducer `masonry_winit` uses (§26.3).
        if let Some(translation) = self.reducer.reduce(scale_factor, &event) {
            match translation {
                WindowEventTranslation::Keyboard(key) => {
                    deliver_text(self.driver.as_mut(), root, TextEvent::Keyboard(key));
                },
                WindowEventTranslation::Pointer(pointer) => {
                    deliver_pointer(self.driver.as_mut(), root, pointer);
                },
            }
        }

        match event {
            WinitWindowEvent::CloseRequested => event_loop.exit(),
            WinitWindowEvent::Resized(size) => {
                root.handle_window_event(WindowEvent::Resize(size));
                if let Some(presenter) = self.presenter.as_mut() {
                    presenter.resize(size);
                }
            },
            WinitWindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The third multiplier of §9, and the only place it is applied: the
                // tree keeps laying out in logical coordinates and the composition
                // scales the drawing, so a display change costs a frame and not a
                // relayout.
                root.handle_window_event(WindowEvent::Rescale(scale_factor));
            },
            WinitWindowEvent::Focused(focused) => {
                deliver_text(self.driver.as_mut(), root, TextEvent::WindowFocusChange(focused));
            },
            WinitWindowEvent::RedrawRequested => {
                if let Err(error) = self.redraw() {
                    tracing::error!("{error}");
                }
            },
            _ => {},
        }

        self.drain_signals(event_loop);
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // An animation frame is a reason to draw the next one; anything else waits
        // for an event. This is what keeps an idle window idle — apart from an
        // external hole, which by §26.1 has to keep painting to stay a hole.
        if let (Some(root), Some(window)) = (self.render_root.as_ref(), self.window.as_ref())
            && root.needs_anim()
        {
            window.request_redraw();
        }
    }
}
