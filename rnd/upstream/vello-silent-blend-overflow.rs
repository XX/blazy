//! Second failure mode: the blend scratch buffer, and what actually charges it.
//!
//! `blend_spill` is `1 << 20` u32. A tile whose nesting goes deeper than
//! BLEND_STACK_SPLIT (4) spills `(depth - 4) * 16 * 16` words, allocated in
//! `coarse.wgsl` with `atomicAdd(&bump.blend, scratch_size)`; on overflow the stage
//! sets `bump.failed`, `fine.wgsl` returns without drawing, and `render_to_texture`
//! still reports success. So the frame is simply missing, exactly as in the tile case.
//!
//! What is *not* obvious is which layers charge a tile. `coarse.wgsl` decides with
//!
//! ```text
//! is_blend     = blend != BLEND_CLIP
//! include_tile = n_segs != 0 || (backdrop_clear == is_clip) || is_blend
//! ```
//!
//! so a blend layer (`push_layer`) is included in every tile of its box, while a clip
//! (`push_clip_layer`) is included only where its path has segments — the tiles its
//! outline crosses. This program walks both, and three mixtures of them, at two frame
//! sizes.
//!
//! Measured on Intel UHD (CML GT2), Mesa 25.2.8, Vulkan, vello 0.10:
//!
//! | nesting | 1100x750 | 2200x1500 |
//! |---|---|---|
//! | groups | draws at 5, empty at 6 | draws at 4, empty at 5 |
//! | screen-sized rect clips | draws at 30, empty at 40 | draws at 21, empty at 22 |
//! | rect clips inset by 32 | draws at 40 | draws at 22, empty at 23 |
//! | clips whose outline zigzags across the frame | draws at 5, empty at 6 | draws at 4, empty at 5 |
//! | a clip around every group | draws at 5, empty at 6 | draws at 4, empty at 5 |
//!
//! The rectangular-clip boundaries put the charge at about **half the ring** of tiles
//! around the clip's box — `columns + rows` — rather than at its whole perimeter.
//!
//! The layers are visual no-ops (Normal mix, alpha 1.0, clips the size of the frame),
//! so any difference from the same scene drawn without them is the overflow and
//! nothing else.

use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use vello::kurbo::{Affine, BezPath, Circle, Point, Rect};
use vello::peniko::{Color, Fill, Mix};
use vello::wgpu;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};

/// Windows to walk: an ordinary one, and the same at a HiDPI scale factor.
const SIZES: [(u32, u32); 2] = [(1100, 750), (2200, 1500)];

fn main() {
    let (device, queue) = block_on(async {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("an adapter");
        println!("adapter: {}", adapter.get_info().name);
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("vello blend repro"),
                ..Default::default()
            })
            .await
            .expect("a device")
    });
    let mut renderer = Renderer::new(&device, RendererOptions::default()).expect("a renderer");

    for (width, height) in SIZES {
        let tiles = u64::from(width.div_ceil(16) * height.div_ceil(16));
        println!(
            "\n{width}x{height}: {tiles} tiles; blend budget {} words, spill per tile per level 256",
            1_u64 << 20
        );

        let reference = render(&device, &queue, &mut renderer, width, height, 0, Nesting::Blend);
        for kind in [
            Nesting::Blend,
            Nesting::Clip,
            Nesting::InsetClipsDeep,
            Nesting::ClipDenseOutline,
            Nesting::Mixed,
            Nesting::ClipAroundEachBlend,
            Nesting::ClipsUnderFourBlends,
        ] {
        println!("  --- {kind:?}");
        for depth in [4_usize, 5, 6, 12, 20, 21, 22, 23, 30, 40] {
            let image = render(&device, &queue, &mut renderer, width, height, depth, kind);
            let differing = image
                .iter()
                .zip(reference.iter())
                .filter(|(a, b)| a != b)
                .count() as f64
                / image.len() as f64;
            let empty = image.iter().all(|byte| *byte == 0);
            let needed = tiles * 256 * depth.saturating_sub(4) as u64;
            println!(
                "  depth {depth:>2}: spill needs {needed:>9} words  differs from the plain scene in {:>6.2}% of bytes{}",
                differing * 100.0,
                if empty { "   (frame is empty)" } else { "" },
            );
        }
        }
    }

    // --- The third buffer: per-tile command lists. Many small paths piled into one
    // corner keep the tile count low while every tile they touch collects a command
    // per path — which is the case coarse.wgsl handles by writing everyone's commands
    // to the same place ("technical UB", says the comment) rather than by giving up.
    println!("\nptcl: overdraw in a small box, so tiles stay cheap and commands do not");
    let (width, height) = SIZES[0];
    let reference = overdraw(&device, &queue, &mut renderer, width, height, 500);
    for paths in [500_usize, 4000, 8000, 12000, 16000] {
        let image = overdraw(&device, &queue, &mut renderer, width, height, paths);
        let empty = image.iter().all(|byte| *byte == 0);
        let differing = image
            .iter()
            .zip(reference.iter())
            .filter(|(a, b)| a != b)
            .count() as f64
            / image.len() as f64;
        println!(
            "  {paths:>6} paths in a 200x200 box: tiles {:>9}, ptcl needs ~{:>10} words of {}  differs {:>6.2}%{}",
            paths as u64 * 169,
            paths as u64 * 169 * 6,
            1_u64 << 23,
            differing * 100.0,
            if empty { "   (frame is empty)" } else { "" },
        );
    }
}

/// `paths` small circles stacked inside one 200x200 box.
fn overdraw(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut Renderer,
    width: u32,
    height: u32,
    paths: usize,
) -> Vec<u8> {
    let mut scene = Scene::new();
    for i in 0..paths {
        let x = 100.0 + (i % 17) as f64 * 6.0;
        let y = 100.0 + (i % 23) as f64 * 5.0;
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(0x40 + (i % 100) as u8, 0x70, 0xc0, 0x30),
            None,
            &Circle::new(Point::new(x, y), 40.0),
        );
    }
    draw(device, queue, renderer, width, height, scene)
}

/// How the `depth` wrapping layers are pushed.
#[derive(Copy, Clone, Debug)]
enum Nesting {
    /// `push_layer` with a visually null blend: what the tile buffer's sibling costs.
    Blend,
    /// `push_clip_layer` to the whole screen: the shape a UI actually nests, and the
    /// one §33.7 measured as harmless.
    Clip,
    /// The same, but each clip is smaller than the last, so the deepest tiles are few.
    ClipShrinking,
    /// Clip, blend, clip, blend: what a real frame looks like (§20.3).
    Mixed,
    /// `depth` blend layers, each one wrapped in its own screen clip: the same blend
    /// depth as `Blend`, with clips interleaved. If clips are free, the boundary does
    /// not move.
    ClipAroundEachBlend,
    /// Four blend layers, then `depth` clips inside them: clips at the deepest levels.
    ClipsUnderFourBlends,
    /// `depth` clips whose *outline* crosses every tile of the frame — a zigzag, not a
    /// rectangle. coarse.wgsl includes a clip in a tile only when the clip path has
    /// segments there (`include_tile = n_segs != 0 || ...`), so a screen-sized rect
    /// clip charges nothing in its interior and this one should charge everywhere.
    ClipDenseOutline,
    /// Like `ClipAroundEachBlend`, but the clips are inset from the frame, so their
    /// outlines are interior tiles rather than tiles on the frame's own edge.
    InsetClipAroundEachBlend,
    /// `depth` nested inset rectangular clips and nothing else: at a large enough
    /// depth, the ring of tiles along their outlines is a budget's worth on its own.
    InsetClipsDeep,
}

/// A zigzag spanning the frame: a horizontal-ish edge in every tile row, so the path
/// has segments in essentially every tile.
fn zigzag(width: f64, height: f64) -> BezPath {
    let mut path = BezPath::new();
    path.move_to(Point::new(0.0, 0.0));
    let mut y = 0.0;
    let mut left = false;
    while y < height {
        let (x0, x1) = if left { (width, 0.0) } else { (0.0, width) };
        path.line_to(Point::new(x0, y));
        path.line_to(Point::new(x1, y + 8.0));
        left = !left;
        y += 16.0;
    }
    path.line_to(Point::new(width, height));
    path.line_to(Point::new(0.0, height));
    path.close_path();
    path
}

/// The same picture, wrapped in `depth` layers that should change nothing.
fn render(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut Renderer,
    width: u32,
    height: u32,
    depth: usize,
    kind: Nesting,
) -> Vec<u8> {
    let mut scene = Scene::new();
    let screen = Rect::new(0.0, 0.0, f64::from(width), f64::from(height));
    for level in 0..depth {
        match kind {
            Nesting::Blend => scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, Affine::IDENTITY, &screen),
            Nesting::Clip => scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &screen),
            Nesting::ClipShrinking => {
                let inset = 4.0 * level as f64;
                scene.push_clip_layer(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    &screen.inset(-inset),
                );
            }
            Nesting::Mixed => {
                if level % 2 == 0 {
                    scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &screen);
                } else {
                    scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, Affine::IDENTITY, &screen);
                }
            }
            Nesting::ClipAroundEachBlend => {
                scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &screen);
                scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, Affine::IDENTITY, &screen);
            }
            Nesting::InsetClipAroundEachBlend => {
                scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &screen.inset(-32.0));
                scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, Affine::IDENTITY, &screen);
            }
            Nesting::InsetClipsDeep => {
                scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &screen.inset(-32.0));
            }
            Nesting::ClipDenseOutline => {
                let saw = zigzag(f64::from(width), f64::from(height));
                scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &saw);
            }
            Nesting::ClipsUnderFourBlends => {
                if level == 0 {
                    for _ in 0..4 {
                        scene.push_layer(Fill::NonZero, Mix::Normal, 1.0, Affine::IDENTITY, &screen);
                    }
                }
                scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &screen);
            }
        }
    }
    for i in 0..40 {
        let x = 30.0 + (i % 8) as f64 * 130.0;
        let y = 40.0 + (i / 8) as f64 * 140.0;
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::from_rgb8(0x40 + (i * 5) as u8, 0x70, 0xc0),
            None,
            &Circle::new(Point::new(x, y), 55.0),
        );
    }
    let pops = match kind {
        Nesting::ClipAroundEachBlend | Nesting::InsetClipAroundEachBlend => depth * 2,
        Nesting::ClipsUnderFourBlends if depth > 0 => depth + 4,
        _ => depth,
    };
    for _ in 0..pops {
        scene.pop_layer();
    }

    draw(device, queue, renderer, width, height, scene)
}

/// Renders one scene into a fresh texture and hands back the pixels.
fn draw(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut Renderer,
    width: u32,
    height: u32,
    scene: Scene,
) -> Vec<u8> {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    renderer
        .render_to_texture(
            device,
            queue,
            &scene,
            &view,
            &RenderParams {
                base_color: Color::from_rgb8(0x14, 0x14, 0x18),
                width,
                height,
                antialiasing_method: AaConfig::Area,
            },
        )
        .expect("vello reports success");
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    read(device, queue, &texture, width, height)
}

fn read(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture, width: u32, height: u32) -> Vec<u8> {
    let row_bytes = width * 4;
    let padded = row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.poll(wgpu::PollType::wait_indefinitely());

    let mapped = buffer.slice(..).get_mapped_range();
    let mut pixels = Vec::with_capacity((row_bytes * height) as usize);
    for y in 0..height {
        let start = (y * padded) as usize;
        pixels.extend_from_slice(&mapped[start..start + row_bytes as usize]);
    }
    drop(mapped);
    buffer.unmap();
    pixels
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}
