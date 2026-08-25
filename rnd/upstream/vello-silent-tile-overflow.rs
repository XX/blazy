//! Minimal reproducer: vello silently draws nothing when the fixed `tiles` buffer
//! overflows. One dependency — `vello = "0.10"` (or `"0.9"`, which behaves the same);
//! wgpu comes from vello's own re-export, so the versions cannot drift.
//!
//! Each path is one screen-spanning diagonal line, so the tiles vello allocates for
//! it is the whole target: ceil(w/16) * ceil(h/16). The prediction under test is
//! that the frame is dropped as soon as paths * tiles_per_path exceeds 1 << 21
//! (`vello_encoding::BufferSizes::new`, hand-picked and independent of the scene).
//!
//! It bisects the boundary itself and prints it next to the prediction.

use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use vello::kurbo::{Affine, Line, Point, Stroke};
use vello::peniko::Color;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};

const TILE: u32 = 16;
const TILES_BUDGET: u64 = 1 << 21;

fn main() {
    let (device, queue) = block_on(async {
        let instance = vello::wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&vello::wgpu::RequestAdapterOptions::default())
            .await
            .expect("an adapter");
        println!("adapter: {:?}", adapter.get_info());
        adapter
            .request_device(&vello::wgpu::DeviceDescriptor {
                label: Some("vello repro"),
                ..Default::default()
            })
            .await
            .expect("a device")
    });

    let mut renderer = Renderer::new(&device, RendererOptions::default()).expect("a renderer");

    for (width, height) in [(1100_u32, 750_u32), (2200, 1500)] {
        let per_path = u64::from(width.div_ceil(TILE) * height.div_ceil(TILE));
        let threshold = TILES_BUDGET / per_path;
        println!("\n{width}x{height}: {per_path} tiles per screen-spanning path, budget {TILES_BUDGET} => predicted limit {threshold} paths");

        // Bisect the boundary: the last path count that draws and the first that
        // does not.
        let (mut draws, mut empty) = (1_u32, 4096_u32);
        while empty - draws > 1 {
            let middle = draws + (empty - draws) / 2;
            if frame_has_ink(&device, &queue, &mut renderer, width, height, middle) {
                draws = middle;
            } else {
                empty = middle;
            }
        }
        println!(
            "  boundary: {draws} paths draw ({} tiles), {empty} paths draw nothing ({} tiles); predicted {threshold}",
            u64::from(draws) * per_path,
            u64::from(empty) * per_path,
        );

        for reach in [1.0_f64, 3.0] {
            let (mut draws, mut empty) = (1_u32, 4096_u32);
            while empty - draws > 1 {
                let middle = draws + (empty - draws) / 2;
                if frame_has_ink_reaching(&device, &queue, &mut renderer, width, height, middle, reach) {
                    draws = middle;
                } else {
                    empty = middle;
                }
            }
            println!("  reach x{reach}: boundary at {draws} paths");
        }

        for paths in [1_u32] {
            let mut scene = Scene::new();
            for i in 0..paths {
                // A diagonal, so the bounding box is the whole target whatever `i` is.
                let y = f64::from(i % 3);
                let line = Line::new(Point::new(0.0, y), Point::new(f64::from(width), f64::from(height) - y));
                scene.stroke(&Stroke::new(1.0), Affine::IDENTITY, Color::from_rgb8(0x80, 0xa0, 0xf0), None, &line);
            }

            // A fresh texture every time: an undrawn frame is then all zeroes rather
            // than the frame before it, which is exactly what makes this visible.
            let texture = device.create_texture(&vello::wgpu::TextureDescriptor {
                label: None,
                size: vello::wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: vello::wgpu::TextureDimension::D2,
                format: vello::wgpu::TextureFormat::Rgba8Unorm,
                usage: vello::wgpu::TextureUsages::STORAGE_BINDING | vello::wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&vello::wgpu::TextureViewDescriptor::default());

            let result = renderer.render_to_texture(
                &device,
                &queue,
                &scene,
                &view,
                &RenderParams {
                    base_color: Color::from_rgb8(0x14, 0x14, 0x18),
                    width,
                    height,
                    antialiasing_method: AaConfig::Area,
                },
            );
            let _ = device.poll(vello::wgpu::PollType::wait_indefinitely());

            let ink = ink_fraction(&device, &queue, &texture, width, height);
            println!(
                "  {paths:>5} paths  {:>10} tiles needed  render {:>4}  ink {:>6.2}%{}",
                paths as u64 * per_path,
                if result.is_ok() { "ok" } else { "err" },
                ink * 100.0,
                if ink == 0.0 { "   <-- nothing was drawn" } else { "" },
            );
        }
    }
}

/// Whether a scene of `paths` screen-spanning diagonals reaches the texture at all.
fn frame_has_ink(
    device: &vello::wgpu::Device,
    queue: &vello::wgpu::Queue,
    renderer: &mut Renderer,
    width: u32,
    height: u32,
    paths: u32,
) -> bool {
    frame_has_ink_reaching(device, queue, renderer, width, height, paths, 1.0)
}

/// `reach` scales each path's bounding box against the frame: 1.0 covers it exactly,
/// 3.0 hangs far outside it. If vello clamps a path's bbox to the target, the
/// boundary does not move with `reach`; if it does not, the boundary falls by
/// `reach` squared.
fn frame_has_ink_reaching(
    device: &vello::wgpu::Device,
    queue: &vello::wgpu::Queue,
    renderer: &mut Renderer,
    width: u32,
    height: u32,
    paths: u32,
    reach: f64,
) -> bool {
    let mut scene = Scene::new();
    let (w, h) = (f64::from(width), f64::from(height));
    for i in 0..paths {
        let y = f64::from(i % 3);
        let line = Line::new(
            Point::new(w * (0.5 - reach / 2.0), y + h * (0.5 - reach / 2.0)),
            Point::new(w * (0.5 + reach / 2.0), h * (0.5 + reach / 2.0) - y),
        );
        scene.stroke(&Stroke::new(1.0), Affine::IDENTITY, Color::from_rgb8(0x80, 0xa0, 0xf0), None, &line);
    }
    let texture = device.create_texture(&vello::wgpu::TextureDescriptor {
        label: None,
        size: vello::wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: vello::wgpu::TextureDimension::D2,
        format: vello::wgpu::TextureFormat::Rgba8Unorm,
        usage: vello::wgpu::TextureUsages::STORAGE_BINDING | vello::wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&vello::wgpu::TextureViewDescriptor::default());
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
        .expect("vello reports success either way");
    let _ = device.poll(vello::wgpu::PollType::wait_indefinitely());
    ink_fraction(device, queue, &texture, width, height) > 0.0
}

fn ink_fraction(
    device: &vello::wgpu::Device,
    queue: &vello::wgpu::Queue,
    texture: &vello::wgpu::Texture,
    width: u32,
    height: u32,
) -> f64 {
    let row_bytes = width * 4;
    let padded = row_bytes.div_ceil(vello::wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * vello::wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&vello::wgpu::BufferDescriptor {
        label: None,
        size: u64::from(padded) * u64::from(height),
        usage: vello::wgpu::BufferUsages::COPY_DST | vello::wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&vello::wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        vello::wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: vello::wgpu::Origin3d::ZERO,
            aspect: vello::wgpu::TextureAspect::All,
        },
        vello::wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: vello::wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        vello::wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(vello::wgpu::MapMode::Read, |_| {});
    let _ = device.poll(vello::wgpu::PollType::wait_indefinitely());

    let mapped = buffer.slice(..).get_mapped_range();
    let mut drawn = 0_u64;
    for y in 0..height {
        let row = &mapped[(y * padded) as usize..(y * padded + row_bytes) as usize];
        drawn += row.chunks_exact(4).filter(|p| p[3] != 0).count() as u64;
    }
    drop(mapped);
    buffer.unmap();
    drawn as f64 / f64::from(width * height)
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
