//! The frame stays on the GPU: scene into a texture, texture into the swapchain.
//!
//! `rnd/architecture.md` §27.1 has the recipe and where it came from. The shape of it:
//!
//! ```text
//! Composition ──▶ VelloRenderer ──▶ Rgba8Unorm texture ──▶ TextureBlitter ──▶ swapchain
//! ```
//!
//! The intermediate texture is not an accident of the design: `imaging_vello` renders
//! into `Rgba8Unorm` and nothing else, while a window surface is usually
//! `Bgra8Unorm`. The blit that bridges them runs on the GPU, so the frame still never
//! enters main memory — which is the whole point, and the counter the criteria are
//! decided on.
//!
//! Everything here is behind the `vello` feature: it needs a graphics device, and a
//! machine without one has to fall back to the blit path rather than fail to start.

use imaging_wgpu::{TextureRenderer, TextureViewTarget};
use masonry::app::VisualLayerPlan;
use masonry::dpi::PhysicalSize;
use masonry::kurbo::Size;
use masonry::peniko::Color;
use wgpu::CurrentSurfaceTexture;

use crate::backend::{Backend, BackendError};
use crate::compose::{Composition, Hole};
use crate::present::{PresentCounters, PresentError};

/// What the intermediate frame texture has to allow.
///
/// `STORAGE_BINDING` is the one that is not guessable: vello renders through a compute
/// shader and writes the texture as storage, so without it the first frame fails
/// validation rather than looking wrong. `TEXTURE_BINDING` is what the blitter reads
/// it with, `RENDER_ATTACHMENT` is what a raster-based backend would need, and
/// `COPY_SRC` is what makes the texture readable for a test or a screenshot.
const TARGET_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::STORAGE_BINDING
    .union(wgpu::TextureUsages::TEXTURE_BINDING)
    .union(wgpu::TextureUsages::RENDER_ATTACHMENT)
    .union(wgpu::TextureUsages::COPY_SRC);

/// The format `imaging_vello` renders into. Checked against the renderer at startup.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Composes plans and draws them into a GPU texture.
///
/// The half of the swapchain path that needs no window, which is what makes it
/// measurable on a machine that has a graphics device but no display (§27.5).
pub struct GpuFrames {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: imaging_vello::VelloRenderer,
    target: wgpu::Texture,
    view: wgpu::TextureView,
    size: PhysicalSize<u32>,
    background: Option<Color>,
    holes: Vec<Hole>,
    counters: PresentCounters,
}

impl GpuFrames {
    /// Creates its own headless device. For offscreen rendering and for benchmarks.
    pub fn offscreen(size: PhysicalSize<u32>) -> Result<Self, BackendError> {
        let (device, queue) = headless_device().map_err(|reason| BackendError::Unavailable {
            backend: Backend::Vello,
            reason,
        })?;
        Self::new(device, queue, size)
    }

    /// Uses a device somebody else made — a window's, or an engine's in guest mode.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, size: PhysicalSize<u32>) -> Result<Self, BackendError> {
        let renderer = imaging_vello::VelloRenderer::new(device.clone(), queue.clone()).map_err(|error| {
            BackendError::Unavailable {
                backend: Backend::Vello,
                reason: format!("vello renderer: {error:?}"),
            }
        })?;

        // The renderer says what it can draw into; believing it rather than assuming
        // is what keeps a future backend from failing at the first frame.
        if !renderer.supported_texture_formats().contains(&TARGET_FORMAT) {
            return Err(BackendError::Unavailable {
                backend: Backend::Vello,
                reason: format!("renderer cannot draw into {TARGET_FORMAT:?}"),
            });
        }

        let (target, view) = create_target(&device, size);
        Ok(Self {
            device,
            queue,
            renderer,
            target,
            view,
            size,
            background: None,
            holes: Vec::new(),
            counters: PresentCounters::default(),
        })
    }

    pub fn with_background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The texture the last frame was drawn into.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.target
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn holes(&self) -> &[Hole] {
        &self.holes
    }

    pub fn counters(&self) -> PresentCounters {
        self.counters
    }

    pub fn size(&self) -> PhysicalSize<u32> {
        self.size
    }

    /// Reallocates the intermediate texture for a new window size.
    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size == self.size || size.width == 0 || size.height == 0 {
            return;
        }
        let (target, view) = create_target(&self.device, size);
        self.target = target;
        self.view = view;
        self.size = size;
    }

    /// Composes a plan and draws it into the texture.
    ///
    /// No pixel buffer is created and nothing is read back: the counters this bumps
    /// are `frames` and `holes` only, which is what the criteria check.
    pub fn draw(&mut self, plan: &VisualLayerPlan, logical: Size, device_scale: f64) -> Result<(), PresentError> {
        let (width, height) = Composition::physical_size(logical, device_scale);
        self.draw_sized(plan, PhysicalSize::new(width, height), device_scale)
    }

    /// The same, at an exact frame size. See [`Host::render_sized`](crate::Host::render_sized).
    pub fn draw_sized(
        &mut self,
        plan: &VisualLayerPlan,
        frame: PhysicalSize<u32>,
        device_scale: f64,
    ) -> Result<(), PresentError> {
        self.resize(frame);

        let composition = Composition::new(plan, device_scale);
        let composition = match self.background {
            Some(color) => composition.on_background(color, self.size.width, self.size.height),
            None => composition,
        };

        self.holes.clear();
        self.holes.extend_from_slice(&composition.holes);
        self.counters.frames += 1;
        self.counters.holes += composition.holes.len() as u64;

        let mut scene = composition.scene;
        self.renderer
            .render_source_into_texture(
                &mut scene,
                TextureViewTarget::new(&self.view, self.size.width, self.size.height),
            )
            .map_err(|error| PresentError::Platform(format!("vello into texture: {error}")))
    }

    /// Waits for the GPU to finish the frame.
    ///
    /// Only a benchmark needs this: a submitted frame is otherwise timed as the cost
    /// of submitting it, which is not what anybody wants to know.
    pub fn wait(&self) {
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }
}

/// A device with no window behind it.
///
/// The same request `masonry_imaging` makes for its headless renderer; its own
/// version is private, and the alternative to twenty lines here is a dependency on an
/// async executor for one call.
pub(crate) fn headless_device() -> Result<(wgpu::Device, wgpu::Queue), String> {
    block_on(async {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .map_err(|error| format!("no compatible adapter: {error}"))?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("blazy headless device"),
                ..Default::default()
            })
            .await
            .map_err(|error| format!("device request failed: {error}"))
    })
}

/// Runs a future to completion on this thread.
///
/// `wgpu`'s setup calls are async and everything else here is not. Parking the thread
/// until the waker unparks it is what upstream does for the same reason, and it keeps
/// an executor out of the dependency list for three calls made once at startup.
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct ThreadWaker(std::thread::Thread);

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn create_target(device: &wgpu::Device, size: PhysicalSize<u32>) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("blazy frame"),
        size: wgpu::Extent3d {
            width: size.width.max(1),
            height: size.height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: TARGET_USAGE,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

// --- MARK: SWAPCHAIN

/// Draws into a window's swapchain, keeping the frame on the GPU throughout.
///
/// The window half of [`GpuFrames`]: the same draw, plus a surface to blit into and
/// present. Split that way on purpose — a benchmark on a machine with a graphics
/// device but no display can still measure the frame path (§27.5).
#[cfg(feature = "window")]
pub struct SwapchainPresenter {
    gpu: GpuFrames,
    window: std::sync::Arc<winit::window::Window>,
    /// Kept because a lost surface is recreated from it rather than mourned.
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    blitter: wgpu::util::TextureBlitter,
    /// Set when the surface said the image it handed out no longer matches it.
    ///
    /// Acted on at the start of the next frame rather than immediately: the image is
    /// still ours until it is presented, and reconfiguring while holding it means
    /// asking for a second one, which is a validation error.
    reconfigure: bool,
}

#[cfg(feature = "window")]
impl SwapchainPresenter {
    /// Opens a device for this window and configures its surface.
    pub fn new(
        window: std::sync::Arc<winit::window::Window>,
        size: PhysicalSize<u32>,
        background: Color,
    ) -> Result<Self, BackendError> {
        let unavailable = |reason: String| BackendError::Unavailable {
            backend: Backend::Vello,
            reason,
        };

        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| unavailable(format!("no surface for this window: {error}")))?;

        let (adapter, device, queue) = block_on(async {
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: Some(&surface),
                    ..Default::default()
                })
                .await
                .map_err(|error| format!("no adapter for this surface: {error}"))?;
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("blazy window device"),
                    ..Default::default()
                })
                .await
                .map_err(|error| format!("device request failed: {error}"))?;
            Ok::<_, String>((adapter, device, queue))
        })
        .map_err(unavailable)?;

        let capabilities = surface.get_capabilities(&adapter);
        tracing::info!(
            formats = ?capabilities.formats,
            usages = ?capabilities.usages,
            alpha_modes = ?capabilities.alpha_modes,
            adapter = %adapter.get_info().name,
            "surface capabilities"
        );
        // Only these two: they are what a blit can land in without a colour-space
        // surprise, and they are what upstream accepts for the same reason (§27.1).
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| {
                matches!(
                    format,
                    wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
                )
            })
            .ok_or_else(|| unavailable("the surface offers no format we can blit into".to_string()))?;

        let (alpha_mode, blitter) = choose_blit(&device, &adapter, &capabilities, format);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: Vec::new(),
        };
        surface.configure(&device, &config);

        let gpu = GpuFrames::new(device, queue, size)?.with_background(background);
        Ok(Self {
            gpu,
            window,
            instance,
            surface,
            config,
            blitter,
            reconfigure: false,
        })
    }

    fn configure(&mut self) {
        self.surface.configure(self.gpu.device(), &self.config);
    }

    /// Points the surface and the frame texture at a new size.
    fn set_size(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.configure();
        // A reconfigure asked for by the previous frame has just happened anyway.
        self.reconfigure = false;
        self.gpu.resize(size);
    }

    /// The swapchain texture, or `None` for a frame that cannot be shown.
    ///
    /// Every outcome `wgpu` distinguishes means something different, and treating them
    /// as one is how a window ends up either panicking or permanently blank:
    ///
    /// * `Suboptimal` **hands over a usable image** and asks for a reconfigure. It has to be presented and the
    ///   reconfigure deferred — reconfiguring here would re-acquire the surface while this image is still held, which
    ///   is a validation error, and by default an uncaptured validation error panics. A stale surface during a drag is
    ///   routine on X11 and Xwayland with NVIDIA drivers, so this is the normal path rather than the exceptional one.
    /// * `Outdated` hands over nothing: reconfigure and ask again.
    /// * `Occluded` and `Timeout` are ordinary — a minimised window is occluded every frame, and logging that as an
    ///   error would fill the log with the fact that nobody is looking.
    /// * `Lost` means the surface must be built again, which is why the instance is kept. Without this the window stays
    ///   blank for the rest of the session after a compositor restart.
    fn acquire(&mut self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(texture) => Some(texture),
            CurrentSurfaceTexture::Suboptimal(texture) => {
                self.reconfigure = true;
                Some(texture)
            },
            CurrentSurfaceTexture::Outdated => {
                self.configure();
                self.acquire_again()
            },
            CurrentSurfaceTexture::Occluded | CurrentSurfaceTexture::Timeout => None,
            CurrentSurfaceTexture::Lost => {
                tracing::warn!("swapchain lost; rebuilding the surface");
                match self.instance.create_surface(self.window.clone()) {
                    Ok(surface) => {
                        self.surface = surface;
                        self.configure();
                        self.acquire_again()
                    },
                    Err(error) => {
                        tracing::error!("the surface could not be rebuilt: {error}");
                        None
                    },
                }
            },
            other => {
                tracing::error!("no swapchain texture: {other:?}");
                None
            },
        }
    }

    /// One retry after reconfiguring. A second failure is this frame's answer.
    fn acquire_again(&mut self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(texture) => Some(texture),
            CurrentSurfaceTexture::Suboptimal(texture) => {
                self.reconfigure = true;
                Some(texture)
            },
            other => {
                tracing::debug!("no swapchain texture after reconfiguring: {other:?}");
                None
            },
        }
    }
}

#[cfg(feature = "window")]
impl crate::present::Presenter for SwapchainPresenter {
    fn name(&self) -> &'static str {
        "swapchain"
    }

    fn present(
        &mut self,
        plan: &VisualLayerPlan,
        frame: PhysicalSize<u32>,
        device_scale: f64,
    ) -> Result<(), PresentError> {
        // The surface follows the frame rather than the window event that announced
        // it: one source of truth for the size means the blit can never be asked to
        // stretch a frame into a swapchain image of another size.
        if self.config.width != frame.width.max(1) || self.config.height != frame.height.max(1) {
            self.set_size(frame);
        } else if std::mem::take(&mut self.reconfigure) {
            // Asked for by the previous frame, once its image was presented.
            self.configure();
        }
        self.gpu.draw_sized(plan, frame, device_scale)?;

        let Some(surface_texture) = self.acquire() else {
            // The frame is drawn and simply not shown; the next redraw will show one.
            return Ok(());
        };

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("blazy surface blit"),
            });
        // GPU to GPU: this is the step the whole task exists for, and the reason the
        // frame never appears in main memory.
        self.blitter
            .copy(self.gpu.device(), &mut encoder, self.gpu.view(), &view);
        self.gpu.queue().submit([encoder.finish()]);

        self.window.pre_present_notify();
        surface_texture.present();
        Ok(())
    }

    fn holes(&self) -> &[Hole] {
        self.gpu.holes()
    }

    fn counters(&self) -> PresentCounters {
        self.gpu.counters()
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        self.set_size(size);
    }
}

/// Picks an alpha mode and a blitter that agree about premultiplication.
///
/// Straight from `masonry_winit`'s surface setup, including the AMD-on-Windows case:
/// these are three findings from other people's machines, and rediscovering them
/// costs more than crediting them (§27.1).
#[cfg(feature = "window")]
fn choose_blit(
    device: &wgpu::Device,
    adapter: &wgpu::Adapter,
    capabilities: &wgpu::SurfaceCapabilities,
    format: wgpu::TextureFormat,
) -> (wgpu::CompositeAlphaMode, wgpu::util::TextureBlitter) {
    use wgpu::util::{TextureBlitter, TextureBlitterBuilder};
    use wgpu::{BlendComponent, BlendFactor, BlendState, CompositeAlphaMode};

    const PREMUL: BlendState = BlendState {
        alpha: BlendComponent::REPLACE,
        color: BlendComponent {
            src_factor: BlendFactor::SrcAlpha,
            dst_factor: BlendFactor::Zero,
            operation: wgpu::BlendOperation::Add,
        },
    };

    if capabilities.alpha_modes.contains(&CompositeAlphaMode::PostMultiplied) {
        (CompositeAlphaMode::PostMultiplied, TextureBlitter::new(device, format))
    } else if capabilities.alpha_modes.contains(&CompositeAlphaMode::PreMultiplied) {
        (
            CompositeAlphaMode::PreMultiplied,
            TextureBlitterBuilder::new(device, format).blend_state(PREMUL).build(),
        )
    } else if cfg!(windows) && adapter.get_info().name.contains("AMD") {
        (
            CompositeAlphaMode::Auto,
            TextureBlitterBuilder::new(device, format).blend_state(PREMUL).build(),
        )
    } else {
        (CompositeAlphaMode::Auto, TextureBlitter::new(device, format))
    }
}
