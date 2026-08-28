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
use masonry::kurbo::{Affine, Size};
use masonry::peniko::Color;
use wgpu::CurrentSurfaceTexture;

use crate::backend::{Backend, BackendError};
use crate::compose::{Composition, Hole, LayerChoice};
use crate::layers::{LayerCache, LayerCounters, PixelRect, scene_bounds};
use crate::present::{PresentCounters, PresentError};

/// What the intermediate frame texture has to allow.
///
/// `STORAGE_BINDING` is the one that is not guessable: vello renders through a compute
/// shader and writes the texture as storage, so without it the first frame fails
/// validation rather than looking wrong. `TEXTURE_BINDING` is what the blitter reads
/// it with, `RENDER_ATTACHMENT` is what a raster-based backend would need, `COPY_SRC`
/// is what makes the texture readable for a test or a screenshot, and `COPY_DST` is
/// what lets a kept layer be copied back into the frame (§36.2).
const TARGET_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::STORAGE_BINDING
    .union(wgpu::TextureUsages::TEXTURE_BINDING)
    .union(wgpu::TextureUsages::RENDER_ATTACHMENT)
    .union(wgpu::TextureUsages::COPY_SRC)
    .union(wgpu::TextureUsages::COPY_DST);

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
    cache: LayerCache,
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
            cache: LayerCache::new(),
        })
    }

    /// Registers the layers whose pixels may be kept between frames (§36).
    ///
    /// Empty — the default — draws every frame from scratch. A registered layer must
    /// **own its rectangle**: nothing else may draw into it, which is true of areas
    /// tiling a window and false of anything overlapping. The host cannot check that,
    /// so it is asked for; [`crate::layers`] says why.
    ///
    /// The ids are the widgets that declared the layers, which for a screen of areas is
    /// `AreaScreen::area_ids()`.
    pub fn cache_layers(&mut self, ids: Vec<masonry::core::WidgetId>) {
        self.cache.set_wanted(ids);
    }

    /// What the layer cache has been doing.
    pub fn layer_counters(&self) -> LayerCounters {
        self.cache.counters()
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
    ///
    /// Returns [`PresentError::SceneTooLarge`] or [`PresentError::SceneTooDeep`] for a
    /// scene the rasteriser cannot take, and draws nothing — [`crate::tiles`] says why
    /// a frame that *is* sent in that case comes back looking like the frame before it.
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
        self.draw_checked(plan, frame, device_scale, true)
    }

    /// Draws a plan without asking whether the rasteriser can take it.
    ///
    /// The door the benchmark needs and an application should not use: the check in
    /// [`Self::draw_sized`] is arithmetic about somebody else's buffer sizes (§33.3),
    /// and the only way to show it is neither blind nor paranoid is to draw the
    /// frames it refuses and look at them. A frame drawn through here can silently be
    /// the previous frame.
    pub fn draw_unchecked(
        &mut self,
        plan: &VisualLayerPlan,
        frame: PhysicalSize<u32>,
        device_scale: f64,
    ) -> Result<(), PresentError> {
        self.draw_checked(plan, frame, device_scale, false)
    }

    fn draw_checked(
        &mut self,
        plan: &VisualLayerPlan,
        frame: PhysicalSize<u32>,
        device_scale: f64,
        checked: bool,
    ) -> Result<(), PresentError> {
        self.resize(frame);

        let (composition, kept) = self.compose(plan, device_scale);
        let composition = match self.background {
            Some(color) => composition.on_background(color, self.size.width, self.size.height),
            None => composition,
        };

        // Before anything is submitted: a scene over one of the rasteriser's fixed
        // buffers is not drawn, and vello does not say so (§33, §34). Cheap next to
        // the frame it guards — two counts over the command stream, and a pass over
        // the composed scene's bounding boxes only when one of them is inconclusive.
        if checked && let Some(overflow) = crate::tiles::over_budget(&composition.scene, self.size) {
            self.counters.frames_refused += 1;
            return Err(match overflow {
                crate::tiles::Overflow::Tiles { tiles, budget } => PresentError::SceneTooLarge { tiles, budget },
                crate::tiles::Overflow::Blend { words, budget } => PresentError::SceneTooDeep { words, budget },
            });
        }

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
            .map_err(|error| PresentError::Platform(format!("vello into texture: {error}")))?;

        self.exchange(plan, kept);
        Ok(())
    }

    /// Walks the plan, leaving out the layers whose pixels the cache still has.
    ///
    /// The decision per layer is §36.3: compare the scene first, because working out
    /// where a layer sits walks the whole of it and a layer that did not change sits
    /// where it sat. With no cache registered every layer is drawn and this is the
    /// ordinary composition.
    fn compose(&mut self, plan: &VisualLayerPlan, device_scale: f64) -> (Composition, Kept) {
        let mut kept = Kept::default();
        if !self.cache.is_active() {
            return (Composition::new(plan, device_scale), kept);
        }

        let size = self.size;
        let cache = &mut self.cache;
        let mut offered = 0;
        let composition = Composition::build(plan, device_scale, |index, layer, scene, transform| {
            if !cache.wants(layer.widget_id) {
                return LayerChoice::Draw;
            }
            offered += 1;
            if let Some(rect) = cache.reusable(layer.widget_id, scene, transform) {
                cache.note_reused();
                kept.reuse.push((layer.widget_id, rect));
                return LayerChoice::Keep;
            }
            cache.note_drawn();
            // A layer that draws nothing has no rectangle to keep; it goes into the
            // frame like any other and that is all it needs.
            cache.note_walk();
            if let Some(rect) = scene_bounds(scene, transform, size) {
                kept.store.push((index, layer.widget_id, transform, rect));
            }
            LayerChoice::Draw
        });
        cache.note_offered(offered);
        (composition, kept)
    }

    /// Puts the kept pixels back, and takes a copy of the ones that were just drawn.
    ///
    /// After the rasteriser and not before: vello clears the target, so a rectangle
    /// copied in first would be painted over. Nothing is resampled — source and target
    /// have the same format and the copy is pixel for pixel, so §23's sharpness claim
    /// is untouched by construction rather than by measurement.
    fn exchange(&mut self, plan: &VisualLayerPlan, kept: Kept) {
        if kept.reuse.is_empty() && kept.store.is_empty() {
            return;
        }

        // Textures first, because allocating one needs the cache mutably and copying
        // out of it needs it while the encoder is alive.
        for (index, id, transform, rect) in &kept.store {
            let masonry::app::VisualLayerKind::Scene(layer_scene) = &plan.layers[*index].kind else {
                continue;
            };
            self.cache
                .store(&self.device, *id, layer_scene, *transform, *rect, TARGET_FORMAT);
        }

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("blazy layer cache"),
        });
        for (id, rect) in &kept.reuse {
            if let Some(texture) = self.cache.texture_of(*id) {
                copy_rect(&mut encoder, texture, CORNER, &self.target, rect.origin(), *rect);
            }
        }
        for (_, id, _, rect) in &kept.store {
            if let Some(texture) = self.cache.texture_of(*id) {
                copy_rect(&mut encoder, &self.target, rect.origin(), texture, CORNER, *rect);
            }
        }
        self.queue.submit([encoder.finish()]);
    }

    /// Copies part of the frame out of the texture, as tightly packed RGBA8.
    ///
    /// A strip rather than the whole frame, for a caller that only needs to know
    /// *whether* this frame was drawn: at 2200x1500 the frame is 13 MB and a strip of
    /// it is half a megabyte, which is the difference between a check that can run
    /// after every frame and one that cannot. Rows outside the texture are clamped
    /// away; asking for none gives none.
    ///
    /// Counts as a readback, for the same reason [`Self::read_pixels`] does.
    pub fn read_rows(&mut self, y: u32, rows: u32) -> Vec<u8> {
        let (width, height) = (self.size.width, self.size.height);
        let y = y.min(height);
        let rows = rows.min(height - y);
        if rows == 0 || width == 0 {
            return Vec::new();
        }

        let row_bytes = width * 4;
        // `copy_texture_to_buffer` wants each row aligned; the padding is dropped on
        // the way out, so the caller gets an image and not a layout to reason about.
        let padded = row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blazy frame readback"),
            size: u64::from(padded) * u64::from(rows),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(rows),
                },
            },
            wgpu::Extent3d {
                width,
                height: rows,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);

        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());

        let mapped = buffer.slice(..).get_mapped_range();
        let mut pixels = Vec::with_capacity((row_bytes * rows) as usize);
        for row in 0..rows {
            let start = (row * padded) as usize;
            pixels.extend_from_slice(&mapped[start..start + row_bytes as usize]);
        }
        drop(mapped);
        buffer.unmap();

        self.counters.readbacks += 1;
        pixels
    }

    /// Copies the frame out of the texture, as tightly packed RGBA8.
    ///
    /// **Not part of the frame path**, and the counter says so: this bumps
    /// `readbacks`, which is the number `gpu_frames_are_not_read_back` gates at zero
    /// (§27.4). It is here for a test, a screenshot, or a benchmark that has to know
    /// whether the frame it just timed contains anything — a frame that failed is
    /// *fast*, and no clock can tell that from a frame that was drawn (§32.4).
    ///
    /// Blocks until the GPU has finished and the buffer is mapped.
    pub fn read_pixels(&mut self) -> Vec<u8> {
        self.read_rows(0, self.size.height)
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

    fn cache_layers(&mut self, ids: Vec<masonry::core::WidgetId>) {
        self.gpu.cache_layers(ids);
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

/// What the frame owes the cache once it is drawn (§36.2).
#[derive(Default)]
struct Kept {
    /// Layers whose pixels are copied in: id and where they go.
    reuse: Vec<(masonry::core::WidgetId, PixelRect)>,
    /// Layers that were drawn and are worth keeping: which layer, and where it landed.
    store: Vec<(usize, masonry::core::WidgetId, Affine, PixelRect)>,
}

/// The corner of a cache texture: it holds one rectangle and nothing else.
const CORNER: (u32, u32) = (0, 0);

/// One rectangle from one texture into another, pixel for pixel.
fn copy_rect(
    encoder: &mut wgpu::CommandEncoder,
    from: &wgpu::Texture,
    from_at: (u32, u32),
    to: &wgpu::Texture,
    to_at: (u32, u32),
    size: PixelRect,
) {
    encoder.copy_texture_to_texture(
        wgpu::TexelCopyTextureInfo {
            texture: from,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: from_at.0,
                y: from_at.1,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyTextureInfo {
            texture: to,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: to_at.0,
                y: to_at.1,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
}
