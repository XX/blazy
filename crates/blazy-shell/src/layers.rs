//! Keeping the pixels of an area that did not change.
//!
//! The paint pass rebuilds the layer plan every frame and re-appends every widget's
//! cached scene into it, so a rasteriser redraws an idle area exactly as often as a
//! working one — seven areas' worth of wasted work on a screen of eight (§30, §32.7).
//! This is the level B of §7.3: a texture per layer, and a layer that did not change is
//! copied instead of drawn. What it saves and what it costs is §36.
//!
//! Three things make it simple enough to be worth having:
//!
//! * Masonry hands the host a layer per widget that asked for one, with the owner's [`WidgetId`] and the layer's
//!   transform (§26.1);
//! * a layer that did not change is recognisable without help from the application: its `Scene` compares equal to the
//!   one kept from last frame;
//! * putting the pixels back needs no shader and no resampling — frame and cache have the same format, so it is
//!   `copy_texture_to_texture`, pixel for pixel.
//!
//! # What a caller has to promise
//!
//! A cached layer **owns its rectangle**: nothing else may draw into it. That holds for
//! Blender-style areas, which tile the window and paint opaque backgrounds, and it does
//! not hold for a popup over an area. The host cannot check it — the plan carries no
//! bounds, only scenes — so it is asked for rather than inferred: a caller registers the
//! layers it knows to be disjoint through
//! [`GpuFrames::cache_layers`](crate::gpu::GpuFrames::cache_layers).
//!
//! The order the decision is made in is load-bearing rather than incidental: comparing
//! scenes is cheap, working out where a layer sits walks its whole scene, and a layer
//! that has not changed sits where it sat (§36.3).

use std::collections::{HashMap, HashSet};

use masonry::core::WidgetId;
use masonry::dpi::PhysicalSize;
use masonry::imaging::record::{Glyph, Scene, replay};
use masonry::imaging::{
    BlurredRoundedRect, ClipRef, FillRef, GeometryRef, GlyphRunRef, GroupRef, PaintSink, StrokeRef,
};
use masonry::kurbo::{Affine, Rect};

/// What the cache did, summed over frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayerCounters {
    /// Layers offered to the cache — registered, and present in the plan.
    pub offered: u64,
    /// Layers whose pixels were copied from the cache instead of being drawn.
    ///
    /// The counter the whole mechanism is judged on: an idle area should raise this
    /// once per frame and cost the rasteriser nothing.
    pub reused: u64,
    /// Layers that were drawn because their scene or their place changed.
    pub drawn: u64,
    /// Bytes of texture the cache is holding.
    pub bytes: u64,
}

/// A texture and the scene it was drawn from, per layer.
pub(crate) struct LayerCache {
    wanted: HashSet<WidgetId>,
    entries: HashMap<WidgetId, Entry>,
    counters: LayerCounters,
}

struct Entry {
    texture: wgpu::Texture,
    /// The scene this texture was drawn from. Compared, not hashed: equality is exact,
    /// and §36.3 measures what it costs.
    scene: Scene,
    /// Where it sat, in physical pixels, and under which transform.
    transform: Affine,
    rect: PixelRect,
}

/// A rectangle in whole physical pixels, which is what a texture copy takes.
///
/// Public because the benchmark prices the cache's own decision (§36.3), and the
/// decision is this rectangle plus a scene comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    /// The top-left corner, which is what a texture copy takes.
    pub fn origin(self) -> (u32, u32) {
        (self.x, self.y)
    }

    /// The pixels a bounding box covers, rounded outwards and clipped to the frame.
    fn of(bounds: Rect, frame: PhysicalSize<u32>) -> Option<Self> {
        let x0 = bounds.x0.floor().max(0.0) as u32;
        let y0 = bounds.y0.floor().max(0.0) as u32;
        let x1 = (bounds.x1.ceil().max(0.0) as u32).min(frame.width);
        let y1 = (bounds.y1.ceil().max(0.0) as u32).min(frame.height);
        (x1 > x0 && y1 > y0).then_some(Self {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

impl LayerCache {
    pub(crate) fn new() -> Self {
        Self {
            wanted: HashSet::new(),
            entries: HashMap::new(),
            counters: LayerCounters::default(),
        }
    }

    /// Registers the layers whose pixels may be kept. Empty turns the cache off.
    pub(crate) fn set_wanted(&mut self, ids: Vec<WidgetId>) {
        self.wanted = ids.into_iter().collect();
        self.entries.retain(|id, _| self.wanted.contains(id));
        self.recount();
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.wanted.is_empty()
    }

    pub(crate) fn wants(&self, id: WidgetId) -> bool {
        self.wanted.contains(&id)
    }

    /// Re-adds up what the textures cost. Called where the set of them changes, which
    /// is the only time it can move.
    fn recount(&mut self) {
        self.counters.bytes = self.entries.values().map(|entry| texture_bytes(&entry.texture)).sum();
    }

    pub(crate) fn counters(&self) -> LayerCounters {
        self.counters
    }

    /// The rectangle this layer's pixels are already sitting in, if they can be copied
    /// rather than drawn.
    ///
    /// Asked before the layer's bounds are computed, and that order is the difference
    /// between a cache that pays for itself and one that does not (§36.3): working out
    /// where a layer sits means walking its whole scene, and a layer that has not
    /// changed sits where it sat. Same scene and same transform is enough — the
    /// rectangle is a function of both.
    pub(crate) fn reusable(&self, id: WidgetId, scene: &Scene, transform: Affine) -> Option<PixelRect> {
        self.entries
            .get(&id)
            .filter(|entry| entry.transform == transform && entry.scene == *scene)
            .map(|entry| entry.rect)
    }

    pub(crate) fn note_offered(&mut self, count: u64) {
        self.counters.offered += count;
    }

    pub(crate) fn note_reused(&mut self) {
        self.counters.reused += 1;
    }

    pub(crate) fn note_drawn(&mut self) {
        self.counters.drawn += 1;
    }

    pub(crate) fn texture_of(&self, id: WidgetId) -> Option<&wgpu::Texture> {
        self.entries.get(&id).map(|entry| &entry.texture)
    }

    /// Takes ownership of what a layer looked like this frame, allocating its texture
    /// if the size changed.
    pub(crate) fn store(
        &mut self,
        device: &wgpu::Device,
        id: WidgetId,
        scene: &Scene,
        transform: Affine,
        rect: PixelRect,
        format: wgpu::TextureFormat,
    ) {
        let fits = self
            .entries
            .get(&id)
            .is_some_and(|entry| entry.rect.width == rect.width && entry.rect.height == rect.height);
        if !fits {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("blazy layer cache"),
                size: wgpu::Extent3d {
                    width: rect.width,
                    height: rect.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.entries.insert(id, Entry {
                texture,
                scene: Scene::new(),
                transform,
                rect,
            });
        }
        let entry = self.entries.get_mut(&id).expect("just inserted or already there");
        entry.scene.clone_from(scene);
        entry.transform = transform;
        entry.rect = rect;
        if !fits {
            self.recount();
        }
    }
}

fn texture_bytes(texture: &wgpu::Texture) -> u64 {
    u64::from(texture.width()) * u64::from(texture.height()) * 4
}

/// The pixels a layer's scene covers, in physical coordinates.
///
/// The union of every drawn shape's bounding box, transformed and clipped to the frame
/// — the same arithmetic the `tiles` module charges tiles with, for the same reason: the
/// plan carries scenes and no bounds, so a host that wants a rectangle has to work it
/// out from what is drawn.
pub fn scene_bounds(scene: &Scene, transform: Affine, frame: PhysicalSize<u32>) -> Option<PixelRect> {
    let mut bounds = Bounds {
        transform,
        union: None,
        clips: Vec::new(),
    };
    replay(scene, &mut bounds);
    let frame_rect = Rect::new(0.0, 0.0, f64::from(frame.width), f64::from(frame.height));
    bounds
        .union
        .map(|rect| rect.intersect(frame_rect))
        .filter(|rect| !rect.is_zero_area())
        .and_then(|rect| PixelRect::of(rect, frame))
}

struct Bounds {
    transform: Affine,
    union: Option<Rect>,
    /// The clips currently open, in physical coordinates.
    ///
    /// Tracked rather than ignored, and that took a measurement to justify: a stroke at
    /// the edge of an area inflates its bounding box by half a stroke width, so a box
    /// that ignores the clip spills over the neighbouring area and the copy lands on the
    /// wrong pixels (§36.2). The clip is where the true edge of an area is, and it is
    /// already a whole number of pixels because §21 rounds area rectangles.
    clips: Vec<Rect>,
}

impl Bounds {
    fn add(&mut self, transform: Affine, bounds: Rect) {
        let mut rect = (self.transform * transform).transform_rect_bbox(bounds);
        for clip in &self.clips {
            rect = rect.intersect(*clip);
        }
        if rect.is_zero_area() {
            return;
        }
        self.union = Some(match self.union {
            Some(union) => union.union(rect),
            None => rect,
        });
    }

    fn add_shape(&mut self, transform: Affine, shape: &GeometryRef<'_>, outset: f64) {
        self.add(transform, crate::tiles::shape_bounds(shape).inflate(outset, outset));
    }
}

impl PaintSink for Bounds {
    fn push_clip(&mut self, clip: ClipRef<'_>) {
        let (transform, shape, outset) = match clip {
            ClipRef::Fill { transform, shape, .. } => (transform, shape, 0.0),
            ClipRef::Stroke {
                transform,
                shape,
                stroke,
            } => (transform, shape, stroke.width / 2.0),
        };
        let rect = (self.transform * transform)
            .transform_rect_bbox(crate::tiles::shape_bounds(&shape).inflate(outset, outset));
        let narrowed = match self.clips.last() {
            Some(open) => rect.intersect(*open),
            None => rect,
        };
        self.clips.push(narrowed);
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn push_group(&mut self, _group: GroupRef<'_>) {}

    fn pop_group(&mut self) {}

    fn fill(&mut self, draw: FillRef<'_>) {
        self.add_shape(draw.transform, &draw.shape, 0.0);
    }

    fn stroke(&mut self, draw: StrokeRef<'_>) {
        self.add_shape(draw.transform, &draw.shape, draw.stroke.width / 2.0);
    }

    fn glyph_run(&mut self, draw: GlyphRunRef<'_>, glyphs: &mut dyn Iterator<Item = Glyph>) {
        let em = f64::from(draw.font_size);
        let box_of_a_glyph = Rect::new(0.0, -em, em, 0.0);
        for glyph in glyphs {
            let at = Affine::translate((f64::from(glyph.x), f64::from(glyph.y)));
            self.add(draw.transform * at, box_of_a_glyph);
        }
    }

    fn blurred_rounded_rect(&mut self, draw: BlurredRoundedRect) {
        let spread = draw.std_dev * 3.0;
        self.add(draw.transform, draw.rect.inflate(spread, spread));
    }
}
