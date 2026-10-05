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
use masonry::imaging::{BlurredRoundedRect, ClipRef, FillRef, GlyphRunRef, GroupRef, PaintSink, StrokeRef};
use masonry::kurbo::{Affine, Rect};

/// What the cache did, summed over frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
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
    /// Times a layer's rectangle had to be worked out by walking its scene.
    ///
    /// The counter behind the ordering of §36.3: comparing scenes is cheap, finding out
    /// where a layer sits is not, and a layer that did not change sits where it sat. A
    /// frame in which nothing changed must not raise this at all — which is a fact about
    /// the code and not about the machine, so it is counted rather than timed.
    pub walks: u64,
    /// Bytes of texture the cache is holding.
    pub bytes: u64,
    /// Layers whose pixels were promised and then were not there to copy.
    ///
    /// Zero by construction now, and counted because it was not: an eviction used to be
    /// able to drop a texture that *this frame* had already decided to reuse, and the
    /// copy then found nothing and did nothing. What that looks like on screen is an
    /// area that blinks, and what it looked like in the counters was a cache working
    /// perfectly — `reused` had already been raised (§44.9). A promise the cache cannot
    /// keep is a fault, so it is a number.
    pub dropped: u64,
    /// Registered layers whose rectangles overlapped another's, this frame.
    ///
    /// The precondition §36 asks a caller for — a cached layer owns its rectangle — and
    /// the only half of it the host can check cheaply, since it has the rectangles
    /// anyway. It is not free to be wrong about: overlapping layers copy over each
    /// other, and they also add up to more pixels than the window, which is what put
    /// the cache over its ceiling (§44.9).
    pub overlaps: u64,
    /// Times eviction could not bring the cache inside its ceiling.
    ///
    /// Reachable by design rather than by accident: the layers this frame is copying are
    /// not evictable, because their pixels are in the cache and nowhere else (§44.9). So
    /// the ceiling can yield for a frame — and when it does it says so, instead of
    /// leaving a silent `return` where a promise used to be (§45).
    pub over_ceiling: u64,
    /// Textures dropped to stay inside the ceiling.
    ///
    /// An eviction is not a fault — it costs the layer a redraw on the frame it comes
    /// back — but it is the difference between a cache that is bounded and one that
    /// grows with whatever an application registers (§37.2).
    pub evictions: u64,
}

/// A texture and the scene it was drawn from, per layer.
pub(crate) struct LayerCache {
    wanted: HashSet<WidgetId>,
    entries: HashMap<WidgetId, Entry>,
    /// Bytes of texture the cache may hold before it starts evicting.
    budget: u64,
    /// The layers this frame has already decided to copy rather than draw.
    ///
    /// Held so that storing a layer cannot evict one of them: their pixels are not in
    /// the frame, they are only in the cache, and dropping one leaves its area empty.
    protected: HashSet<WidgetId>,
    /// Ticks once per store or reuse, so "least recently used" is a number rather than
    /// a guess. A frame counter would do as well; this one does not need the frame.
    clock: u64,
    counters: LayerCounters,
}

struct Entry {
    texture: wgpu::Texture,
    /// When this entry was last kept or refilled, by [`LayerCache::clock`].
    used: u64,
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

    /// Whether two rectangles share a pixel.
    ///
    /// Touching edges do not: areas tile, so the area to the right starts at the pixel
    /// after this one ends.
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

    /// Sets how many bytes of texture the cache may hold.
    ///
    /// The natural ceiling is a few frames' worth: cached layers **tile** the window, so
    /// their pixels add up to about one frame however many areas there are — 4.9 MiB for
    /// eight areas of a 1400x900 window, which is that window (§37.2). A ceiling above
    /// that covers a resize, where the old textures live until their layers are stored
    /// again, and bounds the damage from a caller that registers overlapping layers
    /// against the precondition.
    pub(crate) fn set_budget(&mut self, bytes: u64) {
        self.budget = bytes;
        self.evict_to_fit(None);
    }

    /// Drops least-recently-used textures until the cache is inside its ceiling.
    ///
    /// `keep` is the entry that has just been filled, which is never worth evicting: it
    /// is the most recently used by definition, and dropping it would mean drawing it
    /// again next frame for nothing.
    /// Layers this frame is copying rather than drawing are off limits as well, and
    /// that is not a refinement: their pixels exist nowhere else, so evicting one to
    /// stay inside a ceiling trades a bounded cache for an empty area. The ceiling
    /// bounds what is kept *between* frames; this frame's own layers are not part of
    /// what there is to save.
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
    ///
    /// Counted from the failing side, like every other criterion: the claim is that a
    /// caller registered layers that tile, so what is measured is how many did not.
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

/// The pixels a layer's scene covers, in physical coordinates.
///
/// The union of every drawn shape's bounding box, transformed and clipped to the frame
/// — the same arithmetic [`crate::tiles`] charges tiles with, for the same reason: the
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
