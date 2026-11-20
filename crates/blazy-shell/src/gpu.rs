//! The frame stays on the GPU: scene into a texture, texture into the swapchain.
//!
//!
//! ```text
//! Composition ──▶ VelloRenderer ──▶ Rgba8Unorm texture ──▶ TextureBlitter ──▶ swapchain
//! ```
//!
//! The intermediate texture exists because `imaging_vello` renders only into `Rgba8Unorm`
//! and a surface is usually `Bgra8Unorm`; the blit runs on the GPU, so the frame never
//! enters main memory. Behind the `vello` feature.

use imaging_wgpu::{TextureRenderer, TextureViewTarget};
use masonry::app::VisualLayerPlan;
use masonry::dpi::PhysicalSize;
use masonry::kurbo::{Affine, Size};
use masonry::peniko::Color;
#[cfg(feature = "window")]
use wgpu::CurrentSurfaceTexture;

use crate::backend::{Backend, BackendError};
use crate::compose::{Composition, Hole, LayerChoice};
use crate::layers::{LayerCache, LayerCounters, PixelRect, scene_bounds};
use crate::present::{PresentCounters, PresentError};

/// What the intermediate frame texture has to allow: storage (vello renders with a compute
/// shader), sampling (the blitter), render attachment, and copies both ways (readback,
/// kept layers).
const TARGET_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::STORAGE_BINDING
    .union(wgpu::TextureUsages::TEXTURE_BINDING)
    .union(wgpu::TextureUsages::RENDER_ATTACHMENT)
    .union(wgpu::TextureUsages::COPY_SRC)
    .union(wgpu::TextureUsages::COPY_DST);

/// The format `imaging_vello` renders into. Checked against the renderer at startup.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Composes plans and draws them into a GPU texture: the swapchain path without a window.
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

        // The renderer says what it can draw into.
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
            cache: {
                let mut cache = LayerCache::new();
                cache.set_budget(default_cache_budget(size));
                cache
            },
        })
    }

    /// Registers the layers whose pixels may be kept between frames (none by default). A
    /// layer must own its rectangle: true of areas tiling a window, false of anything
    /// overlapping.
    pub fn cache_layers(&mut self, ids: Vec<masonry::core::WidgetId>) {
        self.cache.set_wanted(ids);
    }

    /// What the layer cache has been doing.
    pub fn layer_counters(&self) -> LayerCounters {
        self.cache.counters()
    }

    /// Sets how many bytes of texture the layer cache may hold before it evicts; two frames'
    /// worth by default, reset on resize.
    pub fn cache_budget(&mut self, bytes: u64) {
        self.cache.set_budget(bytes);
    }

    /// The ceiling this frame size gets unless a caller says otherwise.
    pub fn default_cache_budget(&self) -> u64 {
        default_cache_budget(self.size)
    }

    /// Fills the frame with `color` before the plan is drawn over it.
    pub fn with_background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    /// The device the frames are drawn on, for a caller that draws into the holes.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The queue the frames are submitted on.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The texture the last frame was drawn into.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.target
    }

    /// A view of [`texture`](Self::texture).
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The rectangles the last frame left for the host to fill.
    pub fn holes(&self) -> &[Hole] {
        &self.holes
    }

    /// What these frames have cost so far.
    pub fn counters(&self) -> PresentCounters {
        self.counters
    }

    /// The frame size, in physical pixels.
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
        self.cache.set_budget(default_cache_budget(size));
    }

    /// Composes a plan and draws it into the texture, reading nothing back.
    ///
    /// Returns [`PresentError::SceneTooLarge`] or [`PresentError::SceneTooDeep`], drawing
    /// nothing, for a scene the rasteriser would silently drop ([`crate::tiles`]).
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

    /// Draws a plan without the budget check, for the benchmark that verifies the check. A
    /// frame drawn here can silently be the previous one.
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

        // Before anything is submitted: vello drops an oversized scene silently.
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

    /// Walks the plan, leaving out the layers whose pixels the cache still has (the scene is
    /// compared first: locating a layer walks all of it).
    fn compose(&mut self, plan: &VisualLayerPlan, device_scale: f64) -> (Composition, Kept) {
        // The base colour as the walk's first command; kept pixels are copied over it later.
        let background = self.background.map(|color| (color, self.size.width, self.size.height));
        let mut kept = Kept::default();
        if !self.cache.is_active() {
            return (
                Composition::build(plan, device_scale, background, |_, _, _, _| LayerChoice::Draw),
                kept,
            );
        }

        let size = self.size;
        let cache = &mut self.cache;
        let mut offered = 0;
        let composition = Composition::build(plan, device_scale, background, |index, layer, scene, transform| {
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
            // A layer that draws nothing has no rectangle to keep.
            cache.note_walk();
            if let Some(rect) = scene_bounds(scene, transform, size) {
                kept.store.push((index, layer.widget_id, transform, rect));
            }
            LayerChoice::Draw
        });
        cache.note_offered(offered);
        (composition, kept)
    }

    /// Puts the kept pixels back and copies the ones just drawn: after the rasteriser, which
    /// clears the target; pixel for pixel, nothing resampled.
    fn exchange(&mut self, plan: &VisualLayerPlan, kept: Kept) {
        if kept.reuse.is_empty() && kept.store.is_empty() {
            return;
        }

        // Protected before anything is stored: an eviction would empty a kept area.

        self.cache.protect(kept.reuse.iter().map(|(id, _)| *id));

        // Checked: cached layers must not overlap.
        let rects: Vec<_> = kept
            .reuse
            .iter()
            .map(|(_, rect)| *rect)
            .chain(kept.store.iter().map(|(_, _, _, rect)| *rect))
            .collect();
        self.cache.note_overlaps(&rects);

        // Textures first: allocating needs the cache mutably.
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
            match self.cache.texture_of(*id) {
                Some(texture) => copy_rect(&mut encoder, texture, CORNER, &self.target, rect.origin(), *rect),
                // Unreachable while `protect` holds; counted, since the area would be empty.
                None => self.cache.note_dropped(),
            }
        }
        for (_, id, _, rect) in &kept.store {
            if let Some(texture) = self.cache.texture_of(*id) {
                copy_rect(&mut encoder, &self.target, rect.origin(), texture, CORNER, *rect);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.cache.release();
    }

    /// Copies rows of the frame out of the texture, as tightly packed RGBA8 (rows outside
    /// are clamped away): enough to tell whether a frame was drawn. Counts as a readback.
    pub fn read_rows(&mut self, y: u32, rows: u32) -> Vec<u8> {
        let (width, height) = (self.size.width, self.size.height);
        let y = y.min(height);
        let rows = rows.min(height - y);
        if rows == 0 || width == 0 {
            return Vec::new();
        }

        let row_bytes = width * 4;
        // Rows are padded for `copy_texture_to_buffer`; the padding is dropped.
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

    /// Copies the frame out of the texture, as tightly packed RGBA8: for a test, a
    /// screenshot or a benchmark, not the frame path (it counts a readback).
    ///
    /// Blocks until the GPU has finished and the buffer is mapped.
    pub fn read_pixels(&mut self) -> Vec<u8> {
        self.read_rows(0, self.size.height)
    }

    /// Waits for the GPU to finish the frame (for a benchmark's timing).
    pub fn wait(&self) {
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }
}

/// A device with no window behind it, requested as `masonry_imaging` does privately.
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

/// Runs a future to completion on this thread, by parking it (for `wgpu`'s async setup).
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

/// Draws into a window's swapchain, keeping the frame on the GPU: [`GpuFrames`] plus a
/// surface.
#[cfg(feature = "window")]
pub struct SwapchainPresenter {
    gpu: GpuFrames,
    window: std::sync::Arc<winit::window::Window>,
    /// Kept to recreate a lost surface.
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    blitter: wgpu::util::TextureBlitter,
    /// Set when the surface asked to be reconfigured; done at the next frame, after the
    /// image it handed out is presented.
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
        // The two a blit lands in without a colour-space surprise.
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
    /// * `Suboptimal` hands over a usable image: present it, reconfigure next frame (now would be a validation error).
    ///   Routine with NVIDIA on X11.
    /// * `Outdated`: reconfigure and ask again.
    /// * `Occluded`, `Timeout`: ordinary, not logged as errors.
    /// * `Lost`: rebuild the surface (after a compositor restart).
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
        // The surface follows the frame's size, so the blit never stretches.
        if self.config.width != frame.width.max(1) || self.config.height != frame.height.max(1) {
            self.set_size(frame);
        } else if std::mem::take(&mut self.reconfigure) {
            // Asked for by the previous frame, once its image was presented.
            self.configure();
        }
        self.gpu.draw_sized(plan, frame, device_scale)?;

        let Some(surface_texture) = self.acquire() else {
            // Drawn and not shown.
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
        // GPU to GPU.
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

/// Picks an alpha mode and a blitter that agree about premultiplication, as
/// `masonry_winit` does (including the AMD-on-Windows case).
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

/// Frames' worth of texture the layer cache may hold by default: the layers tile one frame,
/// and the second is slack for a resize.
const CACHE_FRAMES_OF_TEXTURE: u64 = 2;

/// The default ceiling for a frame of this size.
fn default_cache_budget(size: PhysicalSize<u32>) -> u64 {
    u64::from(size.width) * u64::from(size.height) * 4 * CACHE_FRAMES_OF_TEXTURE
}

/// What the frame owes the cache once it is drawn.
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
