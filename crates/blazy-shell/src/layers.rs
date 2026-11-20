//! Keeping the pixels of an area that did not change.
//!
//! The paint pass re-appends every widget's scene each frame, so an idle area is
//! rasterised as often as a busy one. Here a layer gets a texture, and a layer whose
//! `Scene` compares equal to last frame's is copied (`copy_texture_to_texture`, pixel for
//! pixel) instead of drawn.
//!
//! A cached layer must own its rectangle — true of areas tiling a window, false of a
//! popup over one. The host cannot check it, so a caller registers the layers it knows
//! to be disjoint ([`GpuFrames::cache_layers`](crate::gpu::GpuFrames::cache_layers)).
//! Scenes are compared before a layer's rectangle is worked out, which walks its scene.

use std::collections::{HashMap, HashSet};

use masonry::core::WidgetId;
use masonry::dpi::PhysicalSize;
use masonry::imaging::record::{Glyph, Scene, replay};
use masonry::imaging::{BlurredRoundedRect, ClipRef, FillRef, GlyphRunRef, GroupRef, PaintSink, StrokeRef};
use masonry::kurbo::{Affine, Rect};

/// What the cache did, summed over frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LayerCounters {
    /// Layers offered to the cache — registered, and present in the plan.
    pub offered: u64,
    /// Layers whose pixels were copied from the cache instead of being drawn.
    pub reused: u64,
    /// Layers that were drawn because their scene or their place changed.
    pub drawn: u64,
    /// Times a layer's rectangle had to be worked out by walking its scene; zero in a frame
    /// where nothing changed.
    pub walks: u64,
    /// Bytes of texture the cache is holding.
    pub bytes: u64,
    /// Layers whose pixels were promised and then were not there to copy (an empty area);
    /// zero by construction.
    pub dropped: u64,
    /// Registered layers whose rectangles overlapped another's, this frame: a broken
    /// precondition.
    pub overlaps: u64,
    /// Times eviction could not bring the cache inside its ceiling (this frame's copies are
    /// not evictable).
    pub over_ceiling: u64,
    /// Textures dropped to stay inside the ceiling.
    pub evictions: u64,
}

/// A texture and the scene it was drawn from, per layer.
pub(crate) struct LayerCache {
    wanted: HashSet<WidgetId>,
    entries: HashMap<WidgetId, Entry>,
    /// Bytes of texture the cache may hold before it starts evicting.
    budget: u64,
    /// The layers this frame copies rather than draws, which must not be evicted.
    protected: HashSet<WidgetId>,
    /// Ticks once per store or reuse, for least-recently-used eviction.
    clock: u64,
    counters: LayerCounters,
}

struct Entry {
    texture: wgpu::Texture,
    /// When this entry was last kept or refilled, by [`LayerCache::clock`].
    used: u64,
    /// The scene this texture was drawn from, compared for equality.
    scene: Scene,
    /// Where it sat, in physical pixels, and under which transform.
    transform: Affine,
    rect: PixelRect,
}

/// A rectangle in whole physical pixels, which is what a texture copy takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    /// Left edge, in physical pixels.
    pub x: u32,
    /// Top edge, in physical pixels.
    pub y: u32,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

impl PixelRect {
    /// The top-left corner, which is what a texture copy takes.
    pub fn origin(self) -> (u32, u32) {
        (self.x, self.y)
    }

    /// Whether two rectangles share a pixel (touching edges do not).
    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
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
            budget: u64::MAX,
            protected: HashSet::new(),
            clock: 0,
            counters: LayerCounters::default(),
        }
    }

    /// Registers the layers whose pixels may be kept. Empty turns the cache off.
    pub(crate) fn set_wanted(&mut self, ids: Vec<WidgetId>) {
        self.wanted = ids.into_iter().collect();
        self.entries.retain(|id, _| self.wanted.contains(id));
        self.recount();
    }

    /// Sets how many bytes of texture the cache may hold (tiling layers add up to one frame).
    pub(crate) fn set_budget(&mut self, bytes: u64) {
        self.budget = bytes;
        self.evict_to_fit(None);
    }

    /// Drops least-recently-used textures until the cache is inside its ceiling, never
    /// `keep` (just filled) nor a layer this frame copies.
    fn evict_to_fit(&mut self, keep: Option<WidgetId>) {
        while self.counters.bytes > self.budget {
            let oldest = self
                .entries
                .iter()
                .filter(|(id, _)| Some(**id) != keep && !self.protected.contains(*id))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(id, _)| *id);
            let Some(id) = oldest else {
                self.counters.over_ceiling += 1;
                return;
            };
            self.entries.remove(&id);
            self.counters.evictions += 1;
            self.recount();
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.wanted.is_empty()
    }

    pub(crate) fn wants(&self, id: WidgetId) -> bool {
        self.wanted.contains(&id)
    }

    /// Re-adds up what the textures cost, when the set changes.
    fn recount(&mut self) {
        self.counters.bytes = self.entries.values().map(|entry| texture_bytes(&entry.texture)).sum();
    }

    pub(crate) fn counters(&self) -> LayerCounters {
        self.counters
    }

    /// The rectangle this layer's pixels already sit in, if the scene and transform are
    /// unchanged; asked before its bounds are worked out.
    pub(crate) fn reusable(&mut self, id: WidgetId, scene: &Scene, transform: Affine) -> Option<PixelRect> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self
            .entries
            .get_mut(&id)
            .filter(|entry| entry.transform == transform && entry.scene == *scene)?;
        entry.used = clock;
        Some(entry.rect)
    }

    /// Names the layers this frame will copy, so storing another cannot evict them.
    ///
    /// Cleared by [`Self::release`] once the copies are encoded.
    pub(crate) fn protect(&mut self, ids: impl IntoIterator<Item = WidgetId>) {
        self.protected.clear();
        self.protected.extend(ids);
    }

    /// Ends the protection [`Self::protect`] gave, and re-applies the ceiling.
    pub(crate) fn release(&mut self) {
        self.protected.clear();
        self.evict_to_fit(None);
    }

    pub(crate) fn note_dropped(&mut self) {
        self.counters.dropped += 1;
    }

    /// Counts the registered layers that overlap another one this frame.
    pub(crate) fn note_overlaps(&mut self, rects: &[PixelRect]) {
        for (i, a) in rects.iter().enumerate() {
            if rects[i + 1..].iter().any(|b| a.overlaps(*b)) {
                self.counters.overlaps += 1;
            }
        }
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

    pub(crate) fn note_walk(&mut self) {
        self.counters.walks += 1;
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
                used: 0,
                scene: Scene::new(),
                transform,
                rect,
            });
        }
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(&id).expect("just inserted or already there");
        entry.scene.clone_from(scene);
        entry.transform = transform;
        entry.rect = rect;
        entry.used = clock;
        if !fits {
            self.recount();
            self.evict_to_fit(Some(id));
        }
    }
}

fn texture_bytes(texture: &wgpu::Texture) -> u64 {
    u64::from(texture.width()) * u64::from(texture.height()) * 4
}

/// The pixels a layer's scene covers, in physical coordinates: the union of its shapes'
/// boxes, transformed and clipped.
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
    /// The clips currently open, in physical coordinates: a stroke at an area's edge would
    /// otherwise spill into its neighbour.
    clips: Vec<Rect>,
}

/// The union so far, widened by one box under the clips currently open.
fn widen(union: Option<Rect>, clips: &[Rect], transform: Affine, bounds: Rect) -> Option<Rect> {
    let mut rect = transform.transform_rect_bbox(bounds);
    for clip in clips {
        rect = rect.intersect(*clip);
    }
    if rect.is_zero_area() {
        return union;
    }
    Some(match union {
        Some(union) => union.union(rect),
        None => rect,
    })
}

impl Bounds {
    fn add(&mut self, transform: Affine, bounds: Rect) {
        self.union = widen(self.union, &self.clips, self.transform * transform, bounds);
    }
}

impl PaintSink for Bounds {
    fn push_clip(&mut self, clip: ClipRef<'_>) {
        let (transform, bounds, _) = crate::bounds::clip(&clip);
        let rect = (self.transform * transform).transform_rect_bbox(bounds);
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
        let (transform, bounds) = crate::bounds::fill(&draw);
        self.add(transform, bounds);
    }

    fn stroke(&mut self, draw: StrokeRef<'_>) {
        let (transform, bounds) = crate::bounds::stroke(&draw);
        self.add(transform, bounds);
    }

    fn glyph_run(&mut self, draw: GlyphRunRef<'_>, glyphs: &mut dyn Iterator<Item = Glyph>) {
        // The clip stack is lent to the closure and handed back, rather than copied into
        // a buffer: a glyph run is drawn every frame a layer is dirty, and an allocation
        // there would be paid for by every text label on screen.
        let (base, clips) = (self.transform, std::mem::take(&mut self.clips));
        let mut union = self.union;
        crate::bounds::for_each_glyph(&draw, glyphs, |transform, box_of_a_glyph| {
            union = widen(union, &clips, base * transform, box_of_a_glyph);
        });
        self.clips = clips;
        self.union = union;
    }

    fn blurred_rounded_rect(&mut self, draw: BlurredRoundedRect) {
        let (transform, bounds) = crate::bounds::blurred(&draw);
        self.add(transform, bounds);
    }
}
