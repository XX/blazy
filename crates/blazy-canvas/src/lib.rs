//! A zoomable, pannable canvas of freely positioned widgets, for Masonry.
//!
//! This crate exists to answer the Phase 0 question from `rnd/architecture.md`:
//! can a Blender-style node editor be built on top of `masonry_core` without
//! forking it?
//!
//! Three claims are under test:
//!
//! 1. **Pan and zoom cost one `Affine`.** Changing the view sets a transform on the content widget. It must not
//!    re-encode any child's cached scene, and it must not re-run any child's `layout`. This is the payoff of a retained
//!    tree on top of a vector display list: the encoded scene stores curves, not triangles, so it stays sharp at any
//!    scale.
//!
//! 2. **Culling is mandatory, not an optimisation.** Masonry's per-widget scene cache saves the `paint()` call, but the
//!    paint pass still copies every visible widget's commands into the layer scene every frame (`passes/paint.rs`,
//!    `Scene::append_transformed`). Frame cost is proportional to the volume of *visible* commands, so off-screen nodes
//!    must be stashed to be skipped.
//!
//! 3. **Ordinary widgets work inside nodes.** Masonry already inverts `window_transform` when routing pointer events,
//!    so sliders and checkboxes inside a zoomed node need no special handling from us.
//!
//! # Structure
//!
//! The canvas is two widgets, not one:
//!
//! ```text
//! CanvasLayer      viewport: fixed size, clip path, owns the view. No transform.
//!   └ CanvasContent    carries the view transform; owns the placed children.
//! ```
//!
//! They cannot be merged. A widget's transform maps its own border-box into its
//! parent's space, and the paint pass transforms the clip path by that same
//! `window_transform` (`passes/paint.rs`). A single widget holding both the clip
//! and the view would zoom its own viewport clip along with the content.
//!
//! Because a `WidgetPod` hands its widget to the arena on insertion, the canvas
//! cannot read its own children through `&self`. Everything that needs child state
//! is therefore an associated function taking a [`WidgetMut`], which is the normal
//! Masonry idiom.
//!
//! # What is missing
//!
//! Not a finished node editor yet. Culling is a linear scan rather than a spatial
//! index ([`CanvasContent::cull`]) — the only remaining per-frame cost that is linear
//! in the size of the graph — and there is no link layer, no selection model and no
//! serialisation. Those are the next things to build here (§16, item 10); the three
//! claims above have been measured and held (§20), so the shape underneath them is
//! not in question.

use std::any::TypeId;
use std::cell::Cell;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AllowRawMut, ChildrenIds, ComposeCtx, EventCtx, LayoutCtx, MeasureCtx, MutateCtx, NewWidget, NoAction,
    PaintCtx, PointerEvent, PropertiesMut, PropertiesRef, Property, RawCtx, RegisterCtx, UpdateCtx, Widget, WidgetId,
    WidgetMut, WidgetPod,
};
use masonry::dpi::{LogicalPosition, PhysicalPosition};
use masonry::imaging::Painter;
use masonry::kurbo::{Affine, Axis, Point, Rect, Size, Vec2};
use masonry::layout::{AsUnit, LenReq, Length, SizeDef};
use masonry::ui_events::pointer::{PointerButton, PointerScrollEvent, PointerUpdate};
use strum::IntoStaticStr;

/// How much detail a canvas child should draw at the current zoom level.
///
/// Level of detail serves two purposes, and the second matters more. The obvious
/// one is fewer draw commands per node. The important one is that at
/// [`Detail::Box`] a node can stash its contents entirely — and layout, not
/// painting, is what makes a large graph expensive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Detail {
    /// Full contents: header, body and interactive controls.
    Full,
    /// Header only; controls are stashed.
    Simplified,
    /// A flat filled rectangle. Contents are stashed and not laid out.
    Box,
}

impl Detail {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// The zoom levels at which a canvas switches between [`Detail`] levels.
///
/// Policy, not mechanism: how small a node has to get before its controls stop being
/// usable depends on how the application draws it. It lives here, on the canvas,
/// rather than baked into the library — the alternative is editing this crate to
/// retune a demo, which is a smell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetailThresholds {
    /// Above this zoom, nodes are drawn in full and carry interactive controls.
    pub full: f64,
    /// Above this zoom (and below `full`), nodes keep widgets but drop their controls.
    /// Below it the canvas paints nodes itself and materialises nothing.
    pub simplified: f64,
}

impl Default for DetailThresholds {
    fn default() -> Self {
        Self {
            full: 0.2,
            simplified: 0.05,
        }
    }
}

impl DetailThresholds {
    /// Chooses a detail level for an effective scale factor.
    pub fn for_scale(&self, scale: f64) -> Detail {
        if scale > self.full {
            Detail::Full
        } else if scale > self.simplified {
            Detail::Simplified
        } else {
            Detail::Box
        }
    }
}

/// The detail level the canvas as a whole is showing.
///
/// Note this is the *global* level, not the level the individual node was built at:
/// a node under the pointer keeps its controls while everything around it is
/// simplified. Use it to decide how much effort a painted stand-in deserves — at
/// [`Detail::Simplified`] there are hundreds of nodes on screen and each draw command
/// is multiplied by that count, while at [`Detail::Full`] there are few and the
/// stand-in has to resemble the real controls closely enough that swapping them in on
/// hover is not jarring.
///
/// The canvas sets this property on every child when the zoom crosses a threshold.
/// Children opt in by reading it in `layout`/`paint` and handling it in
/// [`Widget::property_changed`]; children that ignore it simply always draw in full.
///
/// A property rather than a trait method, so the canvas can host heterogeneous
/// children. It is also the same mechanism `rnd/architecture.md` §9 earmarks for
/// per-region `ui_scale`, which has the same shape: a value that flows down a
/// subtree and invalidates layout when it changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanvasDetail(pub Detail);

impl Property for CanvasDetail {
    fn static_default() -> &'static Self {
        static DEFAULT: CanvasDetail = CanvasDetail(Detail::Full);
        &DEFAULT
    }
}

impl Default for CanvasDetail {
    fn default() -> Self {
        *Self::static_default()
    }
}

impl CanvasDetail {
    /// Helper for [`Widget::property_changed`]: requests a relayout when the detail
    /// level changed.
    pub fn prop_changed(ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        if property_type == TypeId::of::<Self>() {
            ctx.request_layout();
        }
    }
}

/// What the canvas is doing right now, plus counters for the Phase 0 measurements.
///
/// Deliberately cheap to collect: Phase 0 exists to produce numbers, and numbers
/// nobody can see are not evidence.
///
/// Captured during the canvas's layout. That matters for [`CanvasCounters::far_repaints`],
/// which is bumped during *paint* and is therefore always one frame behind the rest.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CanvasStats {
    /// Total number of nodes.
    pub total: usize,
    /// Nodes that currently have a widget in the tree.
    ///
    /// This is the number the passes walk, which is the point: it is bounded by the
    /// viewport, not by the graph. Zero in far-field mode, where nodes are painted
    /// rather than materialised — they are on screen but they are not widgets.
    pub materialised: usize,
    /// Detail level applied at the last layout.
    pub detail: Option<Detail>,
    /// Current zoom factor.
    pub zoom: f64,
    /// Cumulative work counters.
    pub counters: CanvasCounters,
}

/// Cumulative counters, for spotting work that should not be happening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CanvasCounters {
    /// Layout passes run on the content widget.
    pub content_layouts: u64,
    /// Nodes that actually needed layout, summed over all passes.
    ///
    /// `run_layout_on` early-returns for a clean widget of unchanged size, so panning
    /// should leave this flat. If it climbs with every pan, something is dirtying
    /// children that should not be.
    pub child_layouts: u64,
    /// Compose passes run on the content widget.
    pub composes: u64,
    /// Widgets built, one per node entering the materialised region.
    ///
    /// If this climbs steeply while panning slowly, the overscan is too tight and
    /// nodes are thrashing in and out.
    pub builds: u64,
    /// Times the far-field scene has been re-recorded.
    ///
    /// The scene is in canvas coordinates, so panning and zooming inside the painted
    /// region reuse it untouched. If this climbs while panning, the region is too
    /// tight and the recording is being thrown away every frame.
    pub far_repaints: u64,
}

/// Builds the widget for a node when it scrolls into view.
///
/// The canvas materialises widgets lazily, so a node's *state* cannot live in its
/// widget: the widget does not exist most of the time. The model behind this trait
/// is the source of truth, and the widget is a view over it — which is the normal
/// arrangement for a node editor anyway, since the graph outlives any view of it.
pub trait NodeSource: 'static {
    /// Builds the widget for the node at `index`, at the given detail level.
    ///
    /// Called every time the node enters the materialised region, so it must read
    /// current state from the model rather than assuming defaults.
    ///
    /// `detail` is [`Detail::Full`] or [`Detail::Simplified`]; below that the canvas
    /// paints the node itself and never calls this. Implementations should build
    /// *fewer child widgets* at `Simplified`, not merely stash them: a stashed widget
    /// still costs a visit in every pass. A control a few pixels tall cannot be used,
    /// so it should be drawn rather than built.
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget>;

    /// Draws node `index` when it is too small to deserve a widget.
    ///
    /// Below the [`Detail::Box`] threshold the canvas stops materialising widgets
    /// entirely and paints the nodes itself, in one pass, into its own scene. A node
    /// a few pixels across does not need layout, hit testing, accessibility or an
    /// event route — it needs a filled rectangle, and a rectangle costs nanoseconds
    /// where a widget costs microseconds.
    ///
    /// Every command drawn here is multiplied by the number of nodes in the recorded
    /// region, so keep it to a few cheap shapes.
    ///
    /// `rect` is in canvas coordinates. The default draws nothing.
    fn paint_far(&mut self, index: usize, rect: Rect, painter: &mut Painter<'_>) {
        let _ = (index, rect, painter);
    }
}

impl<F> NodeSource for F
where
    F: FnMut(usize, Detail) -> NewWidget<dyn Widget> + 'static,
{
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget> {
        self(index, detail)
    }
}

/// The canvas's own recording of nodes too small to deserve widgets.
///
/// Recorded in canvas coordinates, so it stays correct under any pan or zoom — that
/// is the whole point of a vector display list. It only has to be re-recorded when
/// the viewport leaves `region`, which with a generous margin means "almost never"
/// rather than "every frame".
#[derive(Default)]
struct FarField {
    /// Whether the canvas is painting nodes itself instead of materialising them.
    active: bool,
    /// The canvas-space region `nodes` was recorded for. `None` means "needs redoing".
    region: Option<Rect>,
    /// The nodes inside `region`. Meaningless unless `region` is `Some`.
    nodes: Vec<usize>,
    /// Set when the recording must be redone; cleared by the parent once it has asked
    /// for a repaint.
    dirty: bool,
}

/// One node's slot: geometry always, a widget only while it is in view.
struct Slot {
    /// Position of the node's top-left corner, in canvas coordinates.
    pos: Point,
    /// Size in canvas coordinates. Fixed, so children are never measured.
    size: Size,
    /// The materialised widget, if this node currently has one.
    pod: Option<WidgetPod<dyn Widget>>,
    /// How `pod` was built: its own detail level, and the canvas-wide one.
    ///
    /// Both matter. The first decides whether the node has controls; the second is
    /// handed to it as [`CanvasDetail`] so it can scale how much effort its painted
    /// stand-in deserves. A change in either makes the widget stale.
    built: Option<(Detail, Detail)>,
}

/// How far past the viewport nodes stay materialised, as a fraction of the viewport.
///
/// Small on purpose. A node entering the viewport is built in the mutate pass and laid
/// out in the same frame, so the margin buys smoothness under a fast drag rather than
/// correctness — and every extra node it keeps alive is paid for in every pass. At
/// 0.02 of a 1100 px viewport it is about four frames of slack at a typical drag
/// speed; larger values were measurably more expensive at low zoom, where a fraction
/// of the viewport covers a lot of canvas.
const DEFAULT_OVERSCAN: f64 = 0.02;
/// How far past the visible region the far-field scene is recorded.
///
/// Larger than [`DEFAULT_OVERSCAN`] because the trade is different: a bigger recorded
/// scene costs more to append every frame, but re-recording it is what a pan must
/// avoid entirely. Half a screen of margin turns "every frame" into "every few
/// hundred".
const FAR_OVERSCAN: f64 = 0.5;

/// Zoom changes smaller than this are treated as no change at all.
const ZOOM_EPSILON: f64 = 1e-9;
/// How fast a wheel notch zooms, as an exponent on the scroll distance in pixels.
const WHEEL_ZOOM_RATE: f64 = 0.0015;
/// A wheel notch, in pixels, matching what `Portal` assumes.
const WHEEL_LINE_PX: f64 = 120.0;

/// What a state change asks Masonry to redo.
///
/// Exists so the operations below can be written once against `&mut self` and then
/// applied through whichever context the caller happens to hold — a `MutateCtx` from
/// the public API, a `RawCtx` from the pointer handler. Without it each operation
/// needs two copies that drift apart; they already had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
enum Invalidate {
    Nothing,
    Layout,
    LayoutAndPaint,
}

/// The subset of a Masonry context this crate needs to invalidate a widget.
trait Invalidator {
    fn request_layout(&mut self);
    fn request_paint_only(&mut self);
}

impl Invalidator for MutateCtx<'_> {
    fn request_layout(&mut self) {
        Self::request_layout(self);
    }

    fn request_paint_only(&mut self) {
        Self::request_paint_only(self);
    }
}

impl Invalidator for RawCtx<'_> {
    fn request_layout(&mut self) {
        Self::request_layout(self);
    }

    fn request_paint_only(&mut self) {
        Self::request_paint_only(self);
    }
}

impl Invalidate {
    fn apply(self, ctx: &mut impl Invalidator) {
        match self {
            Self::Nothing => {},
            Self::Layout => ctx.request_layout(),
            Self::LayoutAndPaint => {
                ctx.request_layout();
                ctx.request_paint_only();
            },
        }
    }
}

/// Splits two ascending index lists into what left and what arrived.
///
/// `keep_stale` is asked about indices present in both: returning `true` puts the
/// index in *both* outputs, which is how a widget gets rebuilt in place at a new
/// detail level.
///
/// A merge walk rather than a lookup per element: during a pan this runs over the
/// whole visible set every frame, and a search per node was measurably worse.
fn diff_sorted(
    live: &[usize],
    desired: &[usize],
    mut keep_stale: impl FnMut(usize) -> bool,
    removed: &mut Vec<usize>,
    added: &mut Vec<usize>,
) {
    removed.clear();
    added.clear();
    let (mut a, mut b) = (0, 0);
    loop {
        match (live.get(a), desired.get(b)) {
            (Some(&l), Some(&d)) if l == d => {
                if keep_stale(l) {
                    removed.push(l);
                    added.push(l);
                }
                a += 1;
                b += 1;
            },
            (Some(&l), Some(&d)) if l < d => {
                removed.push(l);
                a += 1;
            },
            (Some(_), Some(&d)) => {
                added.push(d);
                b += 1;
            },
            (Some(&l), None) => {
                removed.push(l);
                a += 1;
            },
            (None, Some(&d)) => {
                added.push(d);
                b += 1;
            },
            (None, None) => break,
        }
    }
}

/// Whether `outer` fully contains `inner`.
fn contains_rect(outer: Rect, inner: Rect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

// --- MARK: CONTENT

/// The transformed inner half of a [`CanvasLayer`].
///
/// Holds the freely positioned children and carries the pan/zoom transform. Not
/// constructed directly; a [`CanvasLayer`] owns one.
pub struct CanvasContent {
    /// Geometry for every node; widgets for the materialised ones only.
    slots: Vec<Slot>,
    /// Builds widgets on demand.
    source: Box<dyn NodeSource>,
    /// Indices with a live widget, ascending.
    ///
    /// Empty in far-field mode: below the [`Detail::Box`] threshold no node gets a
    /// widget at all.
    live: Vec<usize>,
    /// Indices inside the visible rect, ascending.
    ///
    /// Equal to `live` above the far-field threshold. Below it, this is what gets
    /// painted directly.
    visible: Vec<usize>,
    /// The far-field recording, used below the [`Detail::Box`] threshold.
    far: FarField,
    /// The node the pointer is on, if any. Only meaningful with `controls_on_hover`.
    active: Option<usize>,
    /// Whether only the node under the pointer gets interactive controls.
    controls_on_hover: bool,
    /// Set when `detail` or `active` changed, so stale widgets get rebuilt.
    detail_dirty: bool,
    /// Whether the queued `pending` needs a staleness sweep as well as a set diff.
    pending_stale: bool,
    /// Reused buffers for the set difference, so a pan allocates nothing.
    scratch_removed: Vec<usize>,
    scratch_added: Vec<usize>,
    /// Indices that should have a widget, computed by the last cull and applied in
    /// the next mutate pass.
    pending: Option<Vec<usize>>,
    /// Visible region in canvas coordinates, pushed down by the parent.
    visible_rect: Rect,
    /// Detail level pushed down by the parent.
    detail: Option<Detail>,
    layouts: u64,
    child_layouts: u64,
    composes: u64,
    builds: u64,
    far_repaints: u64,
}

// `CanvasLayer` owns this widget completely and reaches into it during layout to
// push down the view transform, the visible rect and the detail level. This is the
// case the escape hatch is documented for: "a parent widget completely controls
// their child, but needs it to be a separate widget for user interaction to behave
// as expected".
impl AllowRawMut for CanvasContent {}

impl CanvasContent {
    fn new(slots: Vec<Slot>, source: Box<dyn NodeSource>) -> Self {
        Self {
            slots,
            source,
            live: Vec::new(),
            visible: Vec::new(),
            far: FarField::default(),
            active: None,
            controls_on_hover: false,
            detail_dirty: false,
            pending_stale: false,
            scratch_removed: Vec::new(),
            scratch_added: Vec::new(),
            pending: None,
            visible_rect: Rect::ZERO,
            detail: None,
            layouts: 0,
            child_layouts: 0,
            composes: 0,
            builds: 0,
            far_repaints: 0,
        }
    }

    /// The detail level node `index` should be built at.
    ///
    /// By default this is simply the canvas-wide level: at [`Detail::Full`] every node
    /// gets real controls. With [`CanvasLayer::with_controls_on_hover`] only the node
    /// under the pointer does, which is several times cheaper but swaps a painted
    /// stand-in for real widgets as the pointer arrives. See that method.
    fn effective_detail(&self, index: usize) -> Detail {
        let global = self.detail.unwrap_or(Detail::Full);
        if !self.controls_on_hover || global == Detail::Box {
            return global;
        }
        if self.active == Some(index) {
            Detail::Full
        } else {
            Detail::Simplified
        }
    }

    /// How node `index` should be built right now.
    fn build_spec(&self, index: usize) -> (Detail, Detail) {
        (self.effective_detail(index), self.detail.unwrap_or(Detail::Full))
    }

    /// Materialises and dematerialises children to match the last cull.
    ///
    /// Runs in the mutate pass, which is where adding and removing children is
    /// legal. The mutate pass runs before the layout pass inside the rewrite loop,
    /// so a node entering the view is built, laid out and painted in the same frame.
    fn apply_pending(this: &mut WidgetMut<'_, Self>) {
        let Some(desired) = this.widget.pending.take() else {
            return;
        };
        let stale = std::mem::take(&mut this.widget.pending_stale);

        // Checking staleness is only worth it when a detail level actually changed —
        // a threshold crossing, or a new node under the pointer. Never during a pan.
        let mut removed = std::mem::take(&mut this.widget.scratch_removed);
        let mut added = std::mem::take(&mut this.widget.scratch_added);
        {
            let content = &*this.widget;
            diff_sorted(
                &content.live,
                &desired,
                |i| stale && content.slots[i].built != Some(content.build_spec(i)),
                &mut removed,
                &mut added,
            );
        }

        // Removing takes the widget out of the tree entirely. This is the whole
        // point: a stashed widget is still walked by every pass, a removed one is
        // not. Its state is not lost — it lives in the model behind `NodeSource`.
        let changed = !removed.is_empty() || !added.is_empty();
        for &index in &removed {
            if let Some(pod) = this.widget.slots[index].pod.take() {
                this.widget.slots[index].built = None;
                this.ctx.remove_child(pod);
            }
        }

        for &index in &added {
            let (detail, global) = this.widget.build_spec(index);
            let widget = this.widget.source.build(index, detail).with_props(CanvasDetail(global));
            this.widget.slots[index].pod = Some(widget.to_pod());
            this.widget.slots[index].built = Some((detail, global));
            this.widget.builds += 1;
        }

        this.widget.scratch_removed = removed;
        this.widget.scratch_added = added;
        this.widget.live = desired;
        if changed {
            this.ctx.children_changed();
            this.ctx.request_layout();
        }
        // The far-field repaint is requested by the parent, which is where culling
        // and the region check happen.
    }

    /// Records a new canvas-space position for a node.
    ///
    /// In far-field mode the node is part of this widget's own scene, so moving it
    /// means re-recording that scene rather than re-placing a child widget.
    fn store_child_pos(&mut self, index: usize, pos: Point) -> Invalidate {
        let far_field = self.far.active;
        let Some(slot) = self.slots.get_mut(index) else {
            return Invalidate::Nothing;
        };
        if slot.pos == pos {
            return Invalidate::Nothing;
        }
        slot.pos = pos;
        if far_field {
            self.far.region = None;
            Invalidate::LayoutAndPaint
        } else {
            // Only this widget is dirtied. Sibling nodes keep their cached scenes;
            // the moved node keeps its own too, since only its position changed.
            Invalidate::Layout
        }
    }

    /// Computes the set of nodes inside the visible rect.
    ///
    /// A linear scan over the geometry array. That is O(total) per frame, but it
    /// touches 32 bytes per node and no widget state, so it costs a few microseconds
    /// where materialising the same nodes costs milliseconds. A spatial index drops
    /// in here when the scan itself starts to matter.
    fn cull(&mut self) {
        let mut visible = Vec::with_capacity(self.visible.len() + 8);
        for (index, slot) in self.slots.iter().enumerate() {
            if Rect::from_origin_size(slot.pos, slot.size).overlaps(self.visible_rect) {
                visible.push(index);
            }
        }

        // Below the box threshold the canvas paints nodes itself, so nothing is
        // materialised. This is what keeps a fully zoomed-out graph affordable: the
        // visible set stops bounding the cost, so the cost must stop depending on
        // widgets. See `paint`.
        let far_field = self.detail.unwrap_or(Detail::Full) == Detail::Box;
        let desired: Vec<usize> = if far_field { Vec::new() } else { visible.clone() };

        self.visible = visible;

        // A widget built at the wrong detail level has to be replaced, not repainted:
        // the levels differ in which child widgets exist, not only in how they look.
        let stale = std::mem::take(&mut self.detail_dirty)
            && self
                .live
                .iter()
                .any(|&i| self.slots[i].built != Some(self.build_spec(i)));

        if far_field {
            self.refresh_far_region();
        } else if self.far.region.take().is_some() {
            self.far.nodes.clear();
        }

        if desired != self.live || far_field != self.far.active || stale || self.far.dirty {
            self.far.active = far_field;
            self.pending_stale = stale;
            self.pending = Some(desired);
        } else {
            self.pending = None;
        }
    }

    /// Re-records the far-field node set when the viewport leaves the painted region.
    ///
    /// The recorded scene lives in canvas coordinates, so panning and zooming inside
    /// the region cost one `Affine` and nothing else. The margin is what turns
    /// "re-record every frame" into "re-record when you have travelled half a
    /// screen": it is bought with a larger scene, which the paint pass appends every
    /// frame either way, so it should be generous but not unbounded.
    fn refresh_far_region(&mut self) {
        if self.far.region.is_some_and(|r| contains_rect(r, self.visible_rect)) {
            return;
        }

        let region = self.visible_rect.inflate(
            self.visible_rect.width() * FAR_OVERSCAN,
            self.visible_rect.height() * FAR_OVERSCAN,
        );

        self.far.nodes.clear();
        for (index, slot) in self.slots.iter().enumerate() {
            if Rect::from_origin_size(slot.pos, slot.size).overlaps(region) {
                self.far.nodes.push(index);
            }
        }
        self.far.region = Some(region);
        self.far.dirty = true;
    }
}

impl Widget for CanvasContent {
    type Action = NoAction;

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        // Never measures its children. Node sizes come from the model, so they are
        // known before layout starts.
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::ZERO,
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {
        self.layouts += 1;

        // Children are laid out in canvas coordinates at their natural size; the
        // zoom lives entirely in this widget's transform. That separation is the
        // point. A per-region `ui_scale` would belong here, in layout — but `view`
        // must not, or every zoom step would relayout the whole graph.
        for i in 0..self.live.len() {
            let index = self.live[i];
            let size = self.slots[index].size;
            let pos = self.slots[index].pos;
            let Some(pod) = self.slots[index].pod.as_mut() else {
                continue;
            };
            if ctx.child_needs_layout(pod) {
                self.child_layouts += 1;
            }
            // The size is known from the model, so there is nothing to resolve.
            ctx.run_layout(pod, size);
            ctx.place_child(pod, pos);
        }
    }

    fn compose(&mut self, _ctx: &mut ComposeCtx<'_>) {
        self.composes += 1;
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        if !self.far.active {
            return;
        }
        self.far_repaints += 1;
        // One pass over the visible nodes, straight into this widget's cached scene.
        // The scene is in canvas coordinates, so panning and zooming re-use it via
        // the layer transform without re-encoding anything.
        for &index in &self.far.nodes {
            let slot = &self.slots[index];
            let rect = Rect::from_origin_size(slot.pos, slot.size);
            self.source.paint_far(index, rect, painter);
        }
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        for &index in &self.live {
            if let Some(pod) = self.slots[index].pod.as_mut() {
                ctx.register_child(pod);
            }
        }
    }

    fn children_ids(&self) -> ChildrenIds {
        self.live
            .iter()
            .filter_map(|&i| self.slots[i].pod.as_ref())
            .map(|pod| pod.id())
            .collect()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

/// What the pointer is currently doing on the canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    /// Nothing.
    None,
    /// Panning the view. Holds the last pointer position in viewport space.
    Pan { last: Point },
    /// Dragging a node. Holds its index and the grab offset in canvas space.
    Node { index: usize, grab: Vec2 },
}

// --- MARK: LAYER

/// A canvas of freely positioned children with a pan/zoom view.
///
/// This is the viewport: fixed size, clips its content, and owns the view transform
/// which it pushes down to its [`CanvasContent`] child during layout.
pub struct CanvasLayer {
    content: WidgetPod<CanvasContent>,
    /// Canvas-space to viewport-space transform (pan and zoom).
    view: Affine,
    /// Whether `view` still needs pushing down to the content widget.
    view_dirty: bool,
    /// Viewport size in widget coordinates, set during layout.
    viewport: Size,
    /// How far past the viewport to keep nodes alive, as a fraction of the viewport.
    ///
    /// Culling exactly at the viewport edge makes nodes pop in mid-drag. Expressed as
    /// a fraction rather than in canvas units on purpose: a fixed canvas-space margin
    /// means a huge screen margin when zoomed in and a sliver when zoomed out, which
    /// is backwards.
    overscan: f64,
    /// Mirror of the content's counters, refreshed at the end of each layout.
    stats: Cell<CanvasStats>,
    /// Current pointer gesture.
    drag: Drag,
    /// Whether only the node under the pointer gets interactive controls.
    controls_on_hover: bool,
    /// Where the detail levels switch over.
    thresholds: DetailThresholds,
    /// Smallest and largest permitted zoom.
    zoom_limits: (f64, f64),
}

impl CanvasLayer {
    /// Creates a canvas over `count` nodes.
    ///
    /// `geometry` supplies each node's position and size, and `source` builds its
    /// widget when it scrolls into view. Only the geometry is stored up front: a
    /// graph of a million nodes costs a million `(Point, Size)` pairs, not a million
    /// widgets.
    pub fn new(count: usize, mut geometry: impl FnMut(usize) -> (Point, Size), source: impl NodeSource) -> Self {
        let slots = (0..count)
            .map(|i| {
                let (pos, size) = geometry(i);
                Slot {
                    pos,
                    size,
                    pod: None,
                    built: None,
                }
            })
            .collect();
        Self {
            content: WidgetPod::new(CanvasContent::new(slots, Box::new(source))),
            view: Affine::IDENTITY,
            view_dirty: true,
            viewport: Size::ZERO,
            overscan: DEFAULT_OVERSCAN,
            stats: Cell::new(CanvasStats {
                zoom: 1.0,
                ..CanvasStats::default()
            }),
            drag: Drag::None,
            controls_on_hover: false,
            thresholds: DetailThresholds::default(),
            zoom_limits: (0.02, 8.0),
        }
    }

    /// Materialises interactive controls only for the node under the pointer.
    ///
    /// Off by default. When on, every other node gets whatever its `Simplified` form
    /// paints instead of real control widgets, which at 140 visible nodes is roughly
    /// five times cheaper per frame — a control nobody is touching is still three or
    /// four widgets that every pass has to walk.
    ///
    /// The catch is visual: the painted stand-in is swapped for real widgets as the
    /// pointer arrives, so unless it matches them closely the interface appears to
    /// change under the cursor. Matching Masonry's themed controls by hand is also
    /// fragile — a theme change silently breaks the resemblance. Turn this on only
    /// where the node body is drawn by the application anyway, or where nodes are
    /// small enough that the difference does not read.
    pub fn with_controls_on_hover(mut self, enabled: bool) -> Self {
        self.controls_on_hover = enabled;
        self
    }

    /// The current zoom factor, derived from the view transform.
    pub fn zoom(&self) -> f64 {
        let c = self.view.as_coeffs();
        (c[0] * c[0] + c[1] * c[1]).sqrt()
    }

    /// Statistics as of the last layout pass.
    pub fn stats(&self) -> CanvasStats {
        self.stats.get()
    }

    /// The region of canvas space currently visible, plus the overscan margin.
    fn visible_canvas_rect(&self) -> Rect {
        let viewport = Rect::from_origin_size(Point::ORIGIN, self.viewport);
        let rect = self.view.inverse().transform_rect_bbox(viewport);
        rect.inflate(rect.width() * self.overscan, rect.height() * self.overscan)
    }

    // --- MARK: WIDGETMUT

    /// Sets the view transform.
    ///
    /// Requests a layout pass on the canvas itself, because culling depends on the
    /// view. It does *not* dirty the content widget: child positions are in canvas
    /// coordinates, so a view change moves nobody and the transform does all the
    /// work. Keeping the content clean is what stops Masonry from marking it for
    /// repaint — see the note in [`CanvasLayer::layout`].
    pub fn set_view(this: &mut WidgetMut<'_, Self>, view: Affine) {
        if this.widget.store_view(view) {
            this.ctx.request_layout();
        }
    }

    /// Pans the view by a delta in viewport coordinates.
    pub fn pan(this: &mut WidgetMut<'_, Self>, delta: Vec2) {
        let view = Affine::translate(delta) * this.widget.view;
        Self::set_view(this, view);
    }

    /// The view that results from zooming about `origin`, or `None` if the zoom is
    /// already at its limit.
    ///
    /// Pure, so the `WidgetMut` entry point and the wheel handler share one copy of
    /// the arithmetic instead of two that can drift apart.
    fn zoomed_view(&self, origin: Point, factor: f64) -> Option<Affine> {
        let current = self.zoom();
        let clamped = (current * factor).clamp(self.zoom_limits.0, self.zoom_limits.1);
        let factor = clamped / current;
        if (factor - 1.0).abs() < ZOOM_EPSILON {
            return None;
        }
        // Zoom about the cursor: the canvas point under `origin` stays under it.
        Some(
            Affine::translate(origin.to_vec2())
                * Affine::scale(factor)
                * Affine::translate(-origin.to_vec2())
                * self.view,
        )
    }

    /// Zooms around a fixed point given in viewport coordinates.
    ///
    /// The canvas point under `origin` stays under `origin`, which is what makes
    /// wheel-zoom feel anchored to the cursor.
    pub fn zoom_around(this: &mut WidgetMut<'_, Self>, origin: Point, factor: f64) {
        if let Some(view) = this.widget.zoomed_view(origin, factor) {
            Self::set_view(this, view);
        }
    }

    /// Moves a child to a new canvas-space position.
    ///
    /// Only the moved child is affected: Masonry re-places it in the next layout,
    /// and no other child's scene is re-encoded.
    pub fn move_child(this: &mut WidgetMut<'_, Self>, index: usize, pos: Point) {
        let mut content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.store_child_pos(index, pos).apply(&mut content.ctx);
    }

    /// The nodes that currently have a widget, as `(index, widget id)` pairs.
    ///
    /// Useful for tests and for apps that need to reach into a live node. The list
    /// changes as nodes scroll in and out, so ids must not be cached across frames.
    pub fn live_children(this: &mut WidgetMut<'_, Self>) -> Vec<(usize, WidgetId)> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content
            .widget
            .live
            .iter()
            .filter_map(|&i| content.widget.slots[i].pod.as_ref().map(|pod| (i, pod.id())))
            .collect()
    }

    /// The canvas-space position of a child.
    pub fn child_pos(this: &mut WidgetMut<'_, Self>, index: usize) -> Option<Point> {
        let content = this.ctx.get_mut(&mut this.widget.content);
        content.widget.slots.get(index).map(|s| s.pos)
    }

    // --- MARK: INTERNAL

    /// Records a new view transform. Returns `true` if a layout pass is needed.
    ///
    /// Split out because the two entry points hold different context types — a
    /// `WidgetMut` from the public API, an `EventCtx` from the pointer handler — and
    /// only the "who do I tell" half differs between them.
    #[must_use]
    fn store_view(&mut self, view: Affine) -> bool {
        if self.view == view {
            return false;
        }
        self.view = view;
        self.view_dirty = true;
        true
    }

    /// Applies a new view transform from an event handler.
    fn apply_view(&mut self, view: Affine, ctx: &mut EventCtx<'_>) {
        if self.store_view(view) {
            ctx.request_layout();
        }
    }

    /// Moves a child from an event handler.
    fn move_child_at(&mut self, index: usize, pos: Point, ctx: &mut EventCtx<'_>) {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        content.store_child_pos(index, pos).apply(&mut raw);
    }

    /// Marks the node under the pointer, so only it gets interactive controls.
    ///
    /// Returns `true` if the active node changed.
    fn set_active(&mut self, active: Option<usize>, ctx: &mut EventCtx<'_>) -> bool {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        if content.active == active || content.far.active {
            return false;
        }
        content.active = active;
        content.detail_dirty = true;
        raw.request_layout();
        true
    }

    /// Finds the topmost child under a canvas-space point, from an event handler.
    fn hit_child(&mut self, canvas_pos: Point, ctx: &mut EventCtx<'_>) -> Option<(usize, Point)> {
        let (content, _) = ctx.get_raw(&mut self.content);
        // Hit testing uses the visible set, not the materialised one, so nodes stay
        // grabbable in far-field mode where they have no widget at all.
        content
            .visible
            .iter()
            .rev()
            .map(|&i| (i, &content.slots[i]))
            .find(|(_, s)| Rect::from_origin_size(s.pos, s.size).contains(canvas_pos))
            .map(|(i, s)| (i, s.pos))
    }
}

impl Widget for CanvasLayer {
    type Action = NoAction;

    /// Handles pan, zoom and node dragging.
    ///
    /// This runs *after* the event has been offered to the widget under the pointer
    /// and bubbled up, so a slider inside a node gets first refusal: if it marked
    /// the event handled, the canvas leaves it alone. That is what makes claim 3
    /// work — controls inside nodes need no cooperation from the canvas.
    fn on_pointer_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &PointerEvent) {
        match event {
            PointerEvent::Down(e) if !ctx.is_handled() => {
                // A control inside a node may have taken pointer capture without
                // marking the event handled — `Checkbox` and `Slider` both do
                // exactly that. Starting a drag here would steal the capture out
                // from under them and break every control on the canvas, so the
                // capture target is the signal to defer to, not `is_handled`.
                if ctx.pointer_capture_target_id().is_some_and(|id| id != ctx.widget_id()) {
                    return;
                }
                let pos = ctx.local_position(e.state.position);
                let canvas_pos = self.view.inverse() * pos;
                self.drag = match e.button {
                    // Left button drags a node if there is one under the pointer,
                    // and pans otherwise.
                    Some(PointerButton::Primary) => match self.hit_child(canvas_pos, ctx) {
                        Some((index, child_pos)) => Drag::Node {
                            index,
                            grab: canvas_pos - child_pos,
                        },
                        None => Drag::Pan { last: pos },
                    },
                    // Middle button always pans, as in Blender.
                    Some(PointerButton::Auxiliary) => Drag::Pan { last: pos },
                    _ => Drag::None,
                };
                if self.drag != Drag::None {
                    ctx.capture_pointer();
                    ctx.set_handled();
                }
            },
            PointerEvent::Move(PointerUpdate { current, .. }) => {
                let pos = ctx.local_position(current.position);
                match self.drag {
                    Drag::None if self.controls_on_hover => {
                        // Materialise controls for the node under the pointer, and
                        // only that one. Everything else keeps its painted stand-in.
                        let canvas_pos = self.view.inverse() * pos;
                        let hit = self.hit_child(canvas_pos, ctx).map(|(index, _)| index);
                        self.set_active(hit, ctx);
                    },
                    Drag::None => {},
                    Drag::Pan { last } => {
                        self.drag = Drag::Pan { last: pos };
                        let view = Affine::translate(pos - last) * self.view;
                        self.apply_view(view, ctx);
                        ctx.set_handled();
                    },
                    Drag::Node { index, grab } => {
                        let canvas_pos = self.view.inverse() * pos;
                        self.move_child_at(index, canvas_pos - grab, ctx);
                        ctx.set_handled();
                    },
                }
            },
            PointerEvent::Leave(_) if self.controls_on_hover => {
                self.set_active(None, ctx);
            },
            PointerEvent::Up(_) | PointerEvent::Cancel(_) => {
                if self.drag != Drag::None {
                    self.drag = Drag::None;
                    ctx.release_pointer();
                    ctx.set_handled();
                }
            },
            PointerEvent::Scroll(PointerScrollEvent { delta, state, .. }) if !ctx.is_handled() => {
                // Wheel notches are converted the same way `Portal` does it, so the
                // zoom speed matches the platform's idea of a scroll step.
                let scale_factor = ctx.scale_factor();
                let line_px = PhysicalPosition {
                    x: WHEEL_LINE_PX * scale_factor,
                    y: WHEEL_LINE_PX * scale_factor,
                };
                let viewport = self.viewport;
                let page_px = PhysicalPosition {
                    x: viewport.width * scale_factor,
                    y: viewport.height * scale_factor,
                };
                let delta_px = delta.to_pixel_delta(line_px, page_px);
                let LogicalPosition { y, .. } = delta_px.to_logical::<f64>(scale_factor);
                if y == 0.0 {
                    return;
                }

                let origin = ctx.local_position(state.position);
                let Some(view) = self.zoomed_view(origin, (-y * WHEEL_ZOOM_RATE).exp()) else {
                    return;
                };
                self.apply_view(view, ctx);
                ctx.set_handled();
            },
            _ => {},
        }
    }

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        // The viewport fills whatever space it is given.
        match len_req {
            LenReq::MinContent => Length::ZERO,
            LenReq::MaxContent => match axis {
                Axis::Horizontal => 800.0.px(),
                Axis::Vertical => 600.0.px(),
            },
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        self.viewport = size;

        // Clip to the viewport so children panned out of view cannot paint over the
        // surrounding UI, and so Masonry excludes them from hit testing.
        ctx.set_clip_path(Rect::from_origin_size(Point::ORIGIN, size));

        let visible_rect = self.visible_canvas_rect();
        let detail = self.thresholds.for_scale(self.zoom());
        let view = self.view;
        let view_dirty = std::mem::take(&mut self.view_dirty);

        // Push the view down to the content widget. `set_transform` marks it as
        // needing compose, which runs after layout — so this does not violate the
        // "don't set flags for an earlier pass" rule that `get_raw_mut` warns about.
        // Culling belongs here, not in the content widget: it depends on the view and
        // the viewport, both of which live on this side. Doing it in the content's
        // own `layout` would mean asking the content to re-lay-out on every pan — and
        // Masonry marks anything that re-lays-out for repaint (`passes/layout.rs`,
        // "TODO - Not everything that has been re-laid out needs to be repainted").
        // That is what made a far-field pan re-record its scene every frame.
        //
        // Child positions are in canvas coordinates, so a view change moves nobody:
        // the transform does all the work and no layout is needed at all.
        let needs_mutate = {
            let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
            content.controls_on_hover = self.controls_on_hover;
            content.visible_rect = visible_rect;
            if content.detail != Some(detail) {
                content.detail = Some(detail);
                content.detail_dirty = true;
            }
            if view_dirty {
                raw.set_transform(view);
            }

            content.cull();
            if std::mem::take(&mut content.far.dirty) {
                raw.request_paint_only();
            }
            content.pending.is_some()
        };
        if needs_mutate {
            // Adding and removing children needs a `WidgetMut`, which layout does not
            // have. The mutate pass runs before the next layout pass in the same
            // rewrite loop, so a node entering the view is built and placed in the
            // same frame.
            ctx.mutate_child_later(&mut self.content, |mut content| {
                CanvasContent::apply_pending(&mut content);
            });
        }

        let content_size = ctx.compute_size(&mut self.content, SizeDef::fixed(size), size.into());
        ctx.run_layout(&mut self.content, content_size);
        ctx.place_child(&mut self.content, Point::ORIGIN);

        let zoom = self.zoom();
        let (content, _) = ctx.get_raw(&mut self.content);
        self.stats.set(CanvasStats {
            total: content.slots.len(),
            materialised: content.live.len(),
            detail: content.detail,
            zoom,
            counters: CanvasCounters {
                content_layouts: content.layouts,
                child_layouts: content.child_layouts,
                composes: content.composes,
                builds: content.builds,
                far_repaints: content.far_repaints,
            },
        });
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.content);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.content.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(live: &[usize], desired: &[usize]) -> (Vec<usize>, Vec<usize>) {
        let (mut removed, mut added) = (Vec::new(), Vec::new());
        diff_sorted(live, desired, |_| false, &mut removed, &mut added);
        (removed, added)
    }

    #[test]
    fn diff_of_equal_sets_is_empty() {
        assert_eq!(diff(&[1, 2, 3], &[1, 2, 3]), (vec![], vec![]));
    }

    #[test]
    fn diff_reports_arrivals_and_departures() {
        assert_eq!(diff(&[1, 3, 5], &[3, 4, 5, 6]), (vec![1], vec![4, 6]));
        assert_eq!(diff(&[], &[0, 1]), (vec![], vec![0, 1]));
        assert_eq!(diff(&[0, 1], &[]), (vec![0, 1], vec![]));
    }

    #[test]
    fn stale_entries_are_rebuilt_in_place() {
        let (mut removed, mut added) = (Vec::new(), Vec::new());
        diff_sorted(&[1, 2, 3], &[1, 2, 3], |i| i == 2, &mut removed, &mut added);
        assert_eq!((removed, added), (vec![2], vec![2]));
    }

    #[test]
    fn diff_reuses_its_buffers() {
        let (mut removed, mut added) = (vec![99], vec![99]);
        diff_sorted(&[1], &[1], |_| false, &mut removed, &mut added);
        assert!(removed.is_empty() && added.is_empty(), "stale contents must be cleared");
    }

    #[test]
    fn contains_rect_is_inclusive() {
        let outer = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(contains_rect(outer, outer));
        assert!(contains_rect(outer, Rect::new(1.0, 1.0, 9.0, 9.0)));
        assert!(!contains_rect(outer, Rect::new(-1.0, 0.0, 5.0, 5.0)));
    }

    #[test]
    fn detail_thresholds_are_ordered() {
        let thresholds = DetailThresholds::default();
        assert_eq!(thresholds.for_scale(1.0), Detail::Full);
        assert_eq!(thresholds.for_scale(0.2), Detail::Simplified);
        assert_eq!(thresholds.for_scale(0.01), Detail::Box);
    }

    #[test]
    fn detail_thresholds_are_configurable() {
        let thresholds = DetailThresholds {
            full: 0.6,
            simplified: 0.25,
        };
        assert_eq!(thresholds.for_scale(0.4), Detail::Simplified);
        assert_eq!(thresholds.for_scale(0.1), Detail::Box);
    }
}
