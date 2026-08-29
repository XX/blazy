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
//!    must be stashed to be skipped. The same sentence applies to the commands the canvas draws itself, and it is the
//!    reason links and far-field nodes are batched rather than drawn one shape at a time: a command costs ~0.2-0.4 us
//!    in every frame it sits in the scene, the same geometry inside a shared command ~0.03 us, and an idle canvas pays
//!    that bill as surely as a busy one (§31).
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
//! # Level of detail
//!
//! Two rules decide what a node is built as, and the stricter one wins (§29):
//!
//! * [`DetailThresholds`] asks whether the zoom still leaves a control large enough to use. Readability, and it is what
//!   the level meant until §29.
//! * [`DetailBudget`] asks whether the resulting tree is affordable, in widgets. A zoom threshold is a constant tuned
//!   for one node size and one density; at four times the density the same zoom puts four times the tree in the window,
//!   which is how a canvas ends up holding 4507 widgets and a 31 ms frame at a zoom nothing looked wrong at.
//!
//! An application tiling several canvases in one window should divide one budget
//! between them ([`DetailBudget::split`]): the frame walks the window's tree, not any
//! single canvas's.
//!
//! # What is missing
//!
//! Not a finished node editor yet. Virtualisation, level of detail, the link layer
//! and the spatial index are in and measured (§20, §24), and so is picking by shape
//! rather than by rectangle (§25) — but there is no selection model and no
//! serialisation of the graph. Those are domain work on top of a canvas whose shape
//! is no longer in question.

mod index;
mod links;

use std::any::TypeId;
use std::cell::Cell;

use blazy_shape::{near_segment, scale_of};
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AllowRawMut, ChildrenIds, ComposeCtx, EventCtx, LayoutCtx, MeasureCtx, MutateCtx, NewWidget, NoAction,
    PaintCtx, PointerEvent, PropertiesMut, PropertiesRef, Property, RawCtx, RegisterCtx, UpdateCtx, Widget, WidgetId,
    WidgetMut, WidgetPod,
};
use masonry::dpi::{LogicalPosition, PhysicalPosition};
use masonry::imaging::Painter;
use masonry::kurbo::{Affine, Axis, BezPath, Point, Rect, Shape, Size, Stroke, Vec2};
use masonry::layout::{AsUnit, LenReq, Length, SizeDef};
use masonry::peniko::Color;
use masonry::ui_events::pointer::{PointerButton, PointerScrollEvent, PointerUpdate};
use strum::IntoStaticStr;

use crate::index::SpatialIndex;
pub use crate::links::Link;
use crate::links::{LinkLayer, link_curve, push_link};

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

/// Widgets one canvas may keep in the tree, and how a level is chosen to stay under it.
///
/// The second half of the level-of-detail decision, and the one derived from a
/// measurement rather than from how a node looks. [`DetailThresholds`] asks whether a
/// control is still large enough to use; this asks whether the tree that would result
/// is still affordable. Both rules apply and the **stricter one wins**, because they
/// guard against different failures: a slider three pixels tall is useless however
/// cheap it is, and four thousand widgets are unaffordable however legible they are.
///
/// **Why widgets and not nodes.** §20.2 measured the frame as the cost of walking the
/// widget tree, and a node is not one widget: at [`Detail::Full`] the example's node
/// carries a slider and a checkbox (which carries a label) and costs four, at
/// [`Detail::Simplified`] it costs one. Measured across the whole zoom range and two
/// graph sizes, a panned frame costs 6.5–8.5 us per widget in the tree and does not
/// otherwise care which level produced them (§29.1) — so widgets are the unit the
/// ceiling belongs in, and the per-level cost is what converts a node count into it.
///
/// **Why the costs are given rather than counted.** The canvas builds a node through
/// [`NodeSource`] and never looks inside the result; how many widgets a level costs is
/// the application's knowledge, like the thresholds themselves (§20.7). The defaults
/// are the example's 4 and 1.
///
/// **The budget is a window quantity, not a canvas one.** What a frame walks is the
/// whole window's tree, and a screen of areas holds one canvas per area (§21, §29.1):
/// eight canvases each honestly inside a budget of their own put eight times that in
/// one window, and no canvas can see it happening. An application that tiles canvases
/// should divide one window budget between them — see [`DetailBudget::split`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetailBudget {
    /// Widgets this canvas may put in the tree.
    pub widgets: usize,
    /// Widgets one node costs at [`Detail::Full`].
    pub full_cost: usize,
    /// Widgets one node costs at [`Detail::Simplified`].
    pub simplified_cost: usize,
    /// How far under the budget the estimate has to fall before a finer level is
    /// taken up again, as a fraction of it.
    ///
    /// Without it the visible set's own jitter drives the switch: a pan moves nodes
    /// in and out at the viewport edge, and the count wobbles by 10–22% from frame to
    /// frame at every zoom worth budgeting (§29.1). A policy that flips level on each
    /// wobble rebuilds every visible node twice a second and costs more than the
    /// widgets it saves, so the margin is taken from that measured spread rather than
    /// picked.
    pub hysteresis: f64,
}

/// Widgets one canvas may hold by default.
///
/// Derived, not chosen: a panned frame costs 6.5–8.5 us per widget in the tree
/// (§29.1), so 1200 widgets is about 8–9 ms — half a 60 Hz frame, leaving the rest for
/// the far field, the links and everything else in the window. The bottom end matters
/// as much as the top: ordinary work at zoom 1–2 holds 50–100 widgets, two orders
/// below the ceiling, so the policy never touches it.
pub const DEFAULT_WIDGET_BUDGET: usize = 1200;

impl Default for DetailBudget {
    fn default() -> Self {
        Self {
            widgets: DEFAULT_WIDGET_BUDGET,
            full_cost: 4,
            simplified_cost: 1,
            hysteresis: 0.25,
        }
    }
}

impl DetailBudget {
    /// A budget that never binds, leaving the zoom thresholds as the only rule.
    pub const fn unlimited() -> Self {
        Self {
            widgets: usize::MAX,
            full_cost: 4,
            simplified_cost: 1,
            hysteresis: 0.0,
        }
    }

    /// This budget divided between `ways` canvases sharing one window.
    ///
    /// The honest way to spend a window budget on a screen of areas: the frame walks
    /// the window's tree, so the sum is what has to fit, and dividing it is the
    /// smallest thing that makes each canvas's decision add up to the window's
    /// (§29.1). Even shares because an area's cost does not depend on its size — a
    /// small area at a small zoom holds as much as a large one.
    pub fn split(self, ways: usize) -> Self {
        Self {
            widgets: self.widgets / ways.max(1),
            ..self
        }
    }

    /// Widgets one node costs at `level`.
    pub fn cost_of(&self, level: Detail) -> usize {
        match level {
            Detail::Full => self.full_cost,
            Detail::Simplified => self.simplified_cost,
            // The far field builds no widgets at all; its cost is a scene, and the
            // budget cannot bid it lower — there is no coarser level to fall to
            // (§29.4).
            Detail::Box => 0,
        }
    }

    /// The finest level whose widgets fit, given how many nodes are on screen.
    ///
    /// `current` is the level in force, and it is what makes this hysteretic: staying
    /// where we are only has to fit the budget, while moving to a finer level has to
    /// clear it by [`hysteresis`](Self::hysteresis).
    pub fn level_for(&self, visible: usize, current: Option<Detail>) -> Detail {
        let margin = 1.0 - self.hysteresis.clamp(0.0, 1.0);
        for level in [Detail::Full, Detail::Simplified] {
            // `Detail` is ordered finest-first, so `level < cur` is "finer than now".
            let finer = current.is_some_and(|cur| level < cur);
            let ceiling = if finer {
                (self.widgets as f64 * margin) as usize
            } else {
                self.widgets
            };
            if visible.saturating_mul(self.cost_of(level)) <= ceiling {
                return level;
            }
        }
        Detail::Box
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

/// How the canvas strokes its links.
///
/// Style rather than mechanism, like [`DetailThresholds`]: how a link should look
/// depends on the application, and baking it into the crate would mean editing this
/// file to retune a demo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkStyle {
    pub color: Color,
    /// Colour of the link under the pointer.
    pub hover_color: Color,
    /// Stroke width in canvas units, so links thicken with the zoom like everything
    /// else the canvas draws.
    ///
    /// Canvas units and not a minimum in screen pixels, which is a decision rather
    /// than an oversight (§31.3): a constant on-screen width means the *recorded*
    /// width depends on the zoom, and the scene is recorded in canvas coordinates
    /// precisely so that panning and zooming reuse it untouched. `imaging` has no
    /// non-scaling stroke — the transform is prepended to the whole draw — so buying
    /// a constant hairline means giving the scene a second axis of invalidation.
    pub width: f64,
    /// Below this on-screen length, a link is not drawn at all, in **logical pixels**.
    ///
    /// A curve two pixels long carries no information and still costs a subpath in
    /// every frame it is recorded for. Measured on the curve's bounding box, at
    /// selection time rather than at paint time, so a link leaves the picture and the
    /// pointer's reach in one action (§31.4).
    ///
    /// The rule needs no "only when zoomed out" clause: an on-screen length grows
    /// with the zoom, so it stops firing on its own. Set to zero to switch it off.
    pub min_screen_length: f64,
    /// How far the pointer may miss a link and still pick it, in **screen pixels**.
    ///
    /// Screen pixels rather than canvas units, because the tolerance is about the
    /// pointer and not about the drawing: four canvas units are 0.08 px at the bottom
    /// of the zoom range and 32 px at the top, which would make a link unpickable
    /// exactly where it is thinnest (`rnd/architecture.md` §25.2).
    pub slop: f64,
}

impl Default for LinkStyle {
    fn default() -> Self {
        Self {
            color: Color::from_rgb8(0x8a, 0x8a, 0x96),
            hover_color: Color::from_rgb8(0xd8, 0xd8, 0xe4),
            width: 2.0,
            min_screen_length: 2.0,
            slop: blazy_shape::DEFAULT_SLOP,
        }
    }
}

/// What the canvas found under a point.
///
/// Nodes win over links, at every detail level, because that is the order they are
/// painted in (§25.3): a pointer that disagrees with the picture is worse than one
/// that is imprecise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CanvasHit {
    /// A node, with the canvas-space position of its top-left corner.
    Node { index: usize, pos: Point },
    /// A link, by its index in the edge list.
    Link { edge: usize, link: Link },
}

impl CanvasHit {
    /// The node index, if this is a node.
    pub fn node(self) -> Option<usize> {
        match self {
            Self::Node { index, .. } => Some(index),
            Self::Link { .. } => None,
        }
    }

    /// The edge index, if this is a link.
    pub fn link(self) -> Option<usize> {
        match self {
            Self::Link { edge, .. } => Some(edge),
            Self::Node { .. } => None,
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
    /// Nodes inside the visible rect, whether or not they have a widget.
    ///
    /// The number the budget is decided on, and it is not the same as
    /// [`materialised`](Self::materialised): in the far field every node on screen is
    /// visible and none is a widget. Published because a policy nobody can see the
    /// input of cannot be checked — the criteria in §29.3 are written on this and on
    /// [`CanvasCounters::level_switches`].
    pub visible: usize,
    /// Detail level applied at the last layout.
    ///
    /// The stricter of the two rules that choose it: readability by zoom
    /// ([`DetailThresholds`]) and cost by widgets ([`DetailBudget`]).
    pub detail: Option<Detail>,
    /// Current zoom factor.
    pub zoom: f64,
    /// Nodes in the far-field recording, and therefore drawn on every repaint of it.
    pub recorded_far: usize,
    /// Link curves currently recorded, and therefore drawn on every repaint.
    ///
    /// Bounded by the region the set was chosen for rather than by the viewport, and
    /// the two part company as soon as the view zooms out (§28).
    pub recorded_links: usize,
    /// Link curves the last selection dropped as too short to see.
    ///
    /// The output of [`LinkStyle::min_screen_length`], and the only way to tell a rule
    /// that is doing nothing from one that is quietly deleting the graph's structure.
    /// Zero at any zoom a node is still readable at.
    pub hidden_links: usize,
    /// What the pointer was last found to be over, as of the last pointer move.
    ///
    /// One frame behind whatever moved the pointer, because it is recorded during
    /// event handling and read here during layout — which is exactly the point: a
    /// pick must not ask for a layout pass of its own (§25.4).
    pub hovered: Option<CanvasHit>,
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
    /// Node geometries examined while deciding what is on screen, summed over all
    /// passes.
    ///
    /// The number the spatial index exists to bound. A count rather than a duration
    /// on purpose: it is exact and machine-independent, where the microseconds it
    /// replaced are the kind of measurement that disappears into noise until the
    /// graph is large enough for the problem to be urgent.
    pub slot_visits: u64,
    /// Times the link curves have been re-emitted into the scene.
    ///
    /// Rises whenever the content widget repaints for any reason — a node entering
    /// view is enough — so it measures work done rather than a decision made.
    /// Informational; the decision is [`link_reselects`](Self::link_reselects).
    pub link_repaints: u64,
    /// Times the canvas has re-chosen *which* links are recorded.
    ///
    /// This is the one to bound. Panning inside the recorded region must not raise
    /// it, because the curves are in canvas coordinates and the layer transform does
    /// the work; neither must dragging a node, which moves its own curves without
    /// changing which curves are on screen. Only leaving the region should.
    pub link_reselects: u64,
    /// Times the effective detail level actually changed.
    ///
    /// The counter the hysteresis is judged on. A pan near the budget boundary jitters
    /// the visible set by a fifth (§29.1), and a policy without a margin answers every
    /// wobble with a rebuild of every visible node — which is more expensive than the
    /// widgets it saves. Zero while panning at zoom 1–2 is the other half of the
    /// claim: ordinary work must not reach the policy at all.
    pub level_switches: u64,
    /// Times the far-field scene has been re-emitted into the widget's scene.
    ///
    /// Rises whenever the content widget repaints for any reason, so it measures work
    /// done rather than a decision made — the same relation `link_repaints` has to
    /// `link_reselects`. Informational; the decision is
    /// [`far_records`](Self::far_records).
    pub far_repaints: u64,
    /// Times the canvas has re-chosen *which* nodes the far field records.
    ///
    /// This is the one to bound, and it is the counter the overscan trades against
    /// (§35.2): the recorded scene is in canvas coordinates, so panning and zooming
    /// inside the region reuse it untouched and only leaving the region costs a new
    /// selection. A wider margin buys fewer of these with a bigger scene — and the
    /// scene is what the rasteriser is charged for in every frame (§32.3).
    pub far_records: u64,
    /// Picks performed: pointer moves, button presses, explicit hit tests.
    ///
    /// The denominator of the two counters below. On its own it says only how often
    /// the question was asked.
    pub hit_queries: u64,
    /// Node geometries examined while answering picks, summed.
    ///
    /// Bounded by the grid cell the point falls in plus its slack, so it is bounded
    /// by node *density* and not by the size of the graph — which is the difference
    /// between a pointer that stays cheap on a million nodes and one that does not.
    pub hit_node_tests: u64,
    /// Link curves examined while answering picks, summed.
    ///
    /// Candidates are the links the canvas has actually recorded, so what can be
    /// clicked is exactly what can be seen and the count is bounded by the viewport
    /// region rather than by the edge list.
    pub hit_curve_tests: u64,
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

    /// Called once, when the canvas is in the widget tree, with the canvas's own id.
    ///
    /// A source usually belongs to one canvas, and several canvases over one model is
    /// the normal arrangement rather than an exotic one (§21): the same graph shown in
    /// two areas is two canvases, two sets of geometry and two widget trees over one
    /// model. Knowing which canvas it serves is what lets a source tell the *others*
    /// apart from itself when a change has to be broadcast — see [`moved`](Self::moved).
    fn attached(&mut self, canvas: WidgetId) {
        let _ = canvas;
    }

    /// The user has dragged node `index` to `pos`, and the canvas has already moved
    /// its own copy of the geometry.
    ///
    /// Where the position goes back into the model. Node geometry is *state*, and by
    /// §20.2 state lives in the model, not in the view — the widget does not exist
    /// most of the time, and neither does the canvas's copy of the graph survive a
    /// second view of it. Without this the two views of one graph drift apart on the
    /// first drag, which is exactly what happened before §30.
    ///
    /// Push into `peers` the ids of the other canvases over the same model: the canvas
    /// schedules the same move on each of them. The fan-out goes through the canvas
    /// because a source holds no widget context and cannot reach another widget; the
    /// canvas is handling an event and can. `peers` arrives empty and is a buffer the
    /// canvas reuses, so pushing into it allocates nothing after the first drag.
    fn moved(&mut self, index: usize, pos: Point, peers: &mut Vec<WidgetId>) {
        let _ = (index, pos, peers);
    }

    /// Draws the nodes that are too small to deserve widgets, all of them at once.
    ///
    /// Below the [`Detail::Box`] threshold the canvas stops materialising widgets
    /// entirely and paints the nodes itself, in one pass, into its own scene. A node
    /// a few pixels across does not need layout, hit testing, accessibility or an
    /// event route — it needs a filled rectangle, and a rectangle costs nanoseconds
    /// where a widget costs microseconds.
    ///
    /// **The whole set rather than one node at a time, and that is the point.** What a
    /// far-field frame costs is the number of *draw commands* in the recorded scene,
    /// not the geometry in them: the paint pass re-appends the scene every frame, and
    /// a command costs ~0.2 us there against ~0.014 us for the same rectangle inside a
    /// shared one (§31.1). A per-node signature forces the expensive shape and gives
    /// an implementation no way out; this one lets it group — by colour, by kind — and
    /// pay for the groups instead. The example draws six tints in six commands where
    /// it used to draw five thousand rectangles in five thousand.
    ///
    /// `nodes` is `(index, rect)` in canvas coordinates, ascending by index, and is a
    /// buffer the canvas reuses. The default draws nothing.
    ///
    /// **`scale` is how many screen pixels a canvas unit is worth** when the scene is
    /// recorded, and it is here because the other half of a far-field frame is charged
    /// in *path segments* (§32.3, §35): a rounded rectangle is eight of them and a
    /// plain one is four, so what an implementation draws at a given size is worth as
    /// much as how many commands it draws it in. The recorded scene is in canvas
    /// coordinates and survives a pan untouched (§20.6a), so this value is the scale at
    /// recording time and goes slightly stale between re-recordings — the same trade
    /// the short-link rule makes and for the same reason (§31.4).
    fn paint_far(&mut self, nodes: &[(usize, Rect)], scale: f64, painter: &mut Painter<'_>) {
        let _ = (nodes, scale, painter);
    }

    /// Whether the canvas-space `point` is inside node `index`, whose rectangle is
    /// `rect`.
    ///
    /// The canvas knows where a node is and how big it is; only the application knows
    /// what it looks like, and a node is not usually its bounding box — a rounded
    /// corner, a notch, a circular port. This is the seam: the canvas narrows the
    /// candidates down through its index and asks this about each survivor, so an
    /// implementation is called a handful of times per pick and can afford to be
    /// exact. `blazy_shape::ShapeHit` is the intended tool, kept by the implementor
    /// so that its flattened cache survives between picks.
    ///
    /// Called for nodes that have no widget as well — the far field, and anything
    /// off the materialised set — which is why it cannot be a method on the widget.
    ///
    /// The default is the rectangle, which is what the canvas would answer on its own.
    fn hit(&mut self, index: usize, rect: Rect, point: Point) -> bool {
        let _ = index;
        rect.contains(point)
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
/// avoid entirely.
///
/// **A quarter, measured rather than guessed (§35.2).** It started at a half, from the
/// reasoning above and nothing else; once the frame could be counted in path segments
/// the trade turned out to be priced steeply. At a far zoom over a graph that carries
/// on past the viewport, a half records 47 502 segments and a quarter 27 580 — a fifth
/// to a quarter of the frame's rasterisation — and the re-selections it buys back are
/// 0.02 per frame against 0.00, which is one every fifty. Tighter still is cheaper
/// again, and 0.10 crosses the line where a pan starts re-choosing the link set often
/// enough for `pan_does_not_reselect_links` to see it.
const FAR_OVERSCAN: f64 = 0.25;

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
pub(crate) fn contains_rect(outer: Rect, inner: Rect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

/// How far the viewport may zoom *in* on a recorded region before it is redone.
///
/// Four times the area, that is two times the zoom. The rule cannot be an absolute
/// multiple of the viewport, and that took a measurement to notice (§35.3): the region
/// starts out `(1 + 2 * overscan)^2` times the viewport, so a fixed ceiling means a
/// tighter margin tolerates *more* zooming before it re-records — the recorded set goes
/// staler as the margin gets smaller, which is backwards. Narrowing the default margin
/// from a half to a quarter left 1015 link curves hidden at a zoom whose nodes were
/// widgets again, and it was this constant, not the margin, that was wrong.
const ZOOM_SLACK: f64 = 4.0;

/// How much larger than the viewport a region recorded with this margin may be before
/// it is redone.
pub(crate) fn region_slack(far_overscan: f64) -> f64 {
    let recorded = (1.0 + 2.0 * far_overscan).powi(2);
    recorded * ZOOM_SLACK
}

/// Whether a region recorded earlier still serves this viewport.
///
/// Containment alone is not enough, and that was a real bug rather than a subtlety
/// (§28): a region chosen while zoomed out contains every viewport that follows, so
/// asking only "did the viewport leave it?" means the recorded set never shrinks
/// again. Zoom out to see the whole graph, zoom back in, and the canvas keeps drawing
/// every edge in it — measured at 9857 curves at a zoom whose viewport holds 96.
pub(crate) fn region_covers(region: Rect, visible: Rect, slack: f64) -> bool {
    contains_rect(region, visible) && region.area() <= visible.area() * slack
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
    /// Edges between nodes, drawn as curves rather than built as widgets.
    links: LinkLayer,
    /// How links are stroked.
    link_style: LinkStyle,
    /// How far past the viewport the far field and the link set are recorded, pushed
    /// down by the parent.
    far_overscan: f64,
    /// The node the pointer is on, if any. Only meaningful with `controls_on_hover`.
    active: Option<usize>,
    /// What the pointer is over, node or link. Repainted, never relaid out.
    hovered: Option<CanvasHit>,
    /// Whether only the node under the pointer gets interactive controls.
    controls_on_hover: bool,
    /// Set when `detail` or `active` changed, so stale widgets get rebuilt.
    detail_dirty: bool,
    /// Whether the queued `pending` needs a staleness sweep as well as a set diff.
    pending_stale: bool,
    /// Finds what is on screen without walking the graph.
    index: SpatialIndex,
    /// Reused buffers for the set difference, so a pan allocates nothing.
    scratch_removed: Vec<usize>,
    scratch_added: Vec<usize>,
    /// Reused buffer for index candidates, so culling allocates nothing either.
    scratch_candidates: Vec<usize>,
    /// Reused paths for the two link batches, so a repaint allocates nothing.
    ///
    /// One holds every ordinary link, the other the hovered one; each is stroked with
    /// a single command. `BezPath` has no `clear`, but `truncate(0)` keeps the
    /// capacity, which is the whole reason these are fields.
    scratch_links: BezPath,
    scratch_hot_links: BezPath,
    /// Reused buffer for the far-field batch handed to [`NodeSource::paint_far`].
    scratch_far: Vec<(usize, Rect)>,
    /// Indices that should have a widget, computed by the last cull and applied in
    /// the next mutate pass.
    pending: Option<Vec<usize>>,
    /// Visible region in canvas coordinates, pushed down by the parent.
    visible_rect: Rect,
    /// How many screen pixels one canvas unit covers, pushed down by the parent.
    ///
    /// The zoom only. The rest of the chain — a region's `ui_scale`, the device scale
    /// — would need `window_transform`, which `LayoutCtx` does not offer; the pointer
    /// path uses the full product because `EventCtx` does (see `hit_scale`). It
    /// matters for one thing, [`LinkStyle::min_screen_length`], where the error is a
    /// factor on a two-pixel threshold, and it is written down rather than hidden.
    scale: f64,
    /// The coarsest level the zoom still makes readable, pushed down by the parent.
    ///
    /// Half of the decision. The other half is [`DetailBudget`], and it cannot be
    /// taken by the parent because it needs the number of visible nodes, which only
    /// the cull knows.
    readable: Option<Detail>,
    /// Cost ceiling, pushed down by the parent.
    budget: DetailBudget,
    /// Effective level, decided in `cull` as the stricter of the two rules.
    detail: Option<Detail>,
    layouts: u64,
    child_layouts: u64,
    composes: u64,
    builds: u64,
    level_switches: u64,
    far_repaints: u64,
    far_records: u64,
    visits: u64,
    link_repaints: u64,
    hit_queries: u64,
    hit_node_tests: u64,
    hit_curve_tests: u64,
}

// `CanvasLayer` owns this widget completely and reaches into it during layout to
// push down the view transform, the visible rect and the detail level. This is the
// case the escape hatch is documented for: "a parent widget completely controls
// their child, but needs it to be a separate widget for user interaction to behave
// as expected".
impl AllowRawMut for CanvasContent {}

/// So a parent driving gestures from above can reach the canvas inside its own event.
///
/// The alternative is `mutate_later`, which defers to the mutate pass and boxes a
/// closure per node per event; a driver moving a selection is the case that makes the
/// difference (§38.3).
impl AllowRawMut for CanvasLayer {}

impl CanvasContent {
    fn new(slots: Vec<Slot>, source: Box<dyn NodeSource>) -> Self {
        let index = SpatialIndex::build(slots.iter().map(|s| Rect::from_origin_size(s.pos, s.size)));
        Self {
            index,
            slots,
            source,
            live: Vec::new(),
            visible: Vec::new(),
            far: FarField::default(),
            links: LinkLayer::default(),
            link_style: LinkStyle::default(),
            far_overscan: FAR_OVERSCAN,
            active: None,
            hovered: None,
            controls_on_hover: false,
            detail_dirty: false,
            pending_stale: false,
            scratch_removed: Vec::new(),
            scratch_added: Vec::new(),
            scratch_candidates: Vec::new(),
            scratch_links: BezPath::new(),
            scratch_hot_links: BezPath::new(),
            scratch_far: Vec::new(),
            pending: None,
            visible_rect: Rect::ZERO,
            scale: 1.0,
            readable: None,
            budget: DetailBudget::default(),
            detail: None,
            layouts: 0,
            child_layouts: 0,
            composes: 0,
            builds: 0,
            level_switches: 0,
            far_repaints: 0,
            far_records: 0,
            visits: 0,
            link_repaints: 0,
            hit_queries: 0,
            hit_node_tests: 0,
            hit_curve_tests: 0,
        }
    }

    // --- MARK: HIT TESTING

    /// What is under a canvas-space point.
    ///
    /// `scale` is how many screen pixels one canvas unit covers, which is what turns
    /// a tolerance in pixels into one in canvas units (§25.2).
    ///
    /// Answered from the model rather than from the widget tree, and that is the
    /// whole reason this exists: below the far-field threshold no node has a widget
    /// at all, and a link never has one. `find_widget_under_pointer` cannot see
    /// either of them, so the canvas has to answer for both.
    fn hit(&mut self, canvas_pos: Point, scale: f64) -> Option<CanvasHit> {
        self.hit_queries += 1;
        self.hit_node(canvas_pos).or_else(|| self.hit_link(canvas_pos, scale))
    }

    /// The topmost node under a point, or `None`.
    fn hit_node(&mut self, canvas_pos: Point) -> Option<CanvasHit> {
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        // A point, not a rect: the grid widens the query by its own slack, which is
        // derived from the widest node, so a node reaching into the cell from
        // outside is still a candidate (§24.2).
        self.index
            .candidates(Rect::from_points(canvas_pos, canvas_pos), &mut candidates);
        self.hit_node_tests += candidates.len() as u64;

        // Descending index order: children are placed in ascending order, so the
        // highest index is the one painted last and therefore on top.
        let mut found = None;
        for &index in candidates.iter().rev() {
            let slot = &self.slots[index];
            let rect = Rect::from_origin_size(slot.pos, slot.size);
            // The rectangle first because it is free and rejects almost everything;
            // the exact shape only for what survives.
            if rect.contains(canvas_pos) && self.source.hit(index, rect, canvas_pos) {
                found = Some(CanvasHit::Node {
                    index,
                    pos: rect.origin(),
                });
                break;
            }
        }
        self.scratch_candidates = candidates;
        found
    }

    /// A link under a point, or `None`.
    ///
    /// Candidates are the links the canvas has recorded for this viewport, so what
    /// can be picked is exactly what is drawn — the selection rule of §24.4 and its
    /// limitation, inherited rather than reinvented. Choosing candidates a second
    /// way would mean a pointer that picks a curve nobody can see, or misses one
    /// everybody can.
    fn hit_link(&mut self, canvas_pos: Point, scale: f64) -> Option<CanvasHit> {
        if self.links.is_empty() {
            return None;
        }
        let radius = self.link_style.width / 2.0
            + if scale > f64::EPSILON {
                self.link_style.slop / scale
            } else {
                0.0
            };

        let mut examined = 0_u64;
        let mut found = None;
        // Any of the links under the pointer, not the topmost one: the curves are
        // stroked batched by style, so recorded order is no longer drawing order and
        // "the last one drawn" cannot be recovered from this list (§31.2). Declaring
        // the order undefined is the cheap half of that trade — the alternative is a
        // command per style change, which is the cost the batch exists to remove. What
        // still holds is the property that matters: the candidates are exactly the
        // links that are drawn.
        for &edge in self.links.recorded() {
            let link = self.links.edge(edge);
            let (Some(from), Some(to)) = (self.slots.get(link.from as usize), self.slots.get(link.to as usize)) else {
                continue;
            };
            examined += 1;
            let curve = link_curve(
                Rect::from_origin_size(from.pos, from.size),
                Rect::from_origin_size(to.pos, to.size),
            );
            if near_segment(curve.into(), canvas_pos, radius) {
                found = Some(CanvasHit::Link {
                    edge: edge as usize,
                    link,
                });
                break;
            }
        }
        self.hit_curve_tests += examined;
        found
    }

    /// Records what the pointer is over. Returns whether the canvas has to repaint.
    ///
    /// A repaint, never a layout: highlighting the link under the pointer changes
    /// pixels and nothing else. Materialising controls for the node under the pointer
    /// is a different question with a different answer, and it lives in
    /// [`CanvasLayer::set_active`].
    fn set_hovered(&mut self, hit: Option<CanvasHit>) -> bool {
        if self.hovered == hit {
            return false;
        }
        let was = self.hovered.and_then(CanvasHit::link);
        self.hovered = hit;
        was.is_some() || hit.and_then(CanvasHit::link).is_some()
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
        self.index.moved(index, pos);
        self.links.node_moved(index);
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
    /// Asks the grid for candidates and tests their rectangles exactly. The grid is
    /// what keeps this proportional to what is on screen rather than to the graph:
    /// the linear scan it replaced was invisible up to about 64 000 nodes and cost
    /// 6.8 ms a frame at a million (§24).
    fn cull(&mut self) {
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(self.visible_rect, &mut candidates);

        let mut visible = Vec::with_capacity(self.visible.len() + 8);
        for &index in &candidates {
            let slot = &self.slots[index];
            if Rect::from_origin_size(slot.pos, slot.size).overlaps(self.visible_rect) {
                visible.push(index);
            }
        }
        self.visits += candidates.len() as u64;
        self.scratch_candidates = candidates;

        // The level is decided here rather than by the parent, and this is the only
        // place it can be: the cost rule needs the number of visible nodes, and that
        // number is what the loop above has just computed. Deciding it earlier, from
        // the zoom alone, is what tied the tree's size to the graph's density —
        // 442 nodes at zoom 0.21 on 5000 nodes, 224 on 20 000, same zoom, same
        // decision, twice the tree (§29.1). Nothing here costs an extra pass: the
        // estimate is a multiplication, and the set it changes is computed below.
        //
        // The two rules are independent and the coarser wins. `Detail` is ordered
        // finest-first, so that is `max`.
        let readable = self.readable.unwrap_or(Detail::Full);
        let affordable = self.budget.level_for(visible.len(), self.detail);
        let level = readable.max(affordable);
        if self.detail != Some(level) {
            // Only a change between two known levels is a switch; the first layout is
            // not one, or every canvas would report one before it has shown anything.
            if self.detail.is_some() {
                self.level_switches += 1;
            }
            self.detail = Some(level);
            self.detail_dirty = true;
        }

        // Below the box threshold the canvas paints nodes itself, so nothing is
        // materialised. This is what keeps a fully zoomed-out graph affordable: the
        // visible set stops bounding the cost, so the cost must stop depending on
        // widgets. See `paint`.
        let far_field = level == Detail::Box;
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

        self.refresh_links();

        if desired != self.live || far_field != self.far.active || stale || self.far.dirty {
            self.far.active = far_field;
            self.pending_stale = stale;
            self.pending = Some(desired);
        } else {
            self.pending = None;
        }
    }

    /// Re-chooses which links are recorded when the viewport leaves their region.
    ///
    /// The same margin the far field uses, and for the same reason: the scene is in
    /// canvas coordinates, so panning inside the region reuses it untouched and only
    /// leaving it costs anything.
    fn refresh_links(&mut self) {
        if self.links.is_empty() {
            return;
        }
        let region = self.visible_rect.inflate(
            self.visible_rect.width() * self.far_overscan,
            self.visible_rect.height() * self.far_overscan,
        );
        if !self.links.needs_reselect(self.visible_rect) {
            return;
        }

        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(region, &mut candidates);
        self.visits += candidates.len() as u64;
        candidates.retain(|&i| Rect::from_origin_size(self.slots[i].pos, self.slots[i].size).overlaps(region));
        let reselected = self.links.refresh(region, self.visible_rect, &candidates);
        self.scratch_candidates = candidates;
        if reselected {
            self.drop_short_links();
        }
    }

    /// Drops the links too short to be seen at the current zoom.
    ///
    /// Here rather than in `paint`, and that placement is the design (§31.4). The set
    /// chosen here is the one both the picture and the pointer read, so a link leaves
    /// both at once and they cannot disagree — the property `link_curve` exists to
    /// protect. It also adds no invalidation of its own: the threshold is evaluated
    /// when the set is re-chosen anyway, and between selections it is simply a little
    /// stale, which shows a hairline slightly longer than needed and costs a few
    /// curves. There is no error in the other direction.
    fn drop_short_links(&mut self) {
        let min_screen = self.link_style.min_screen_length;
        if min_screen <= 0.0 || self.scale <= f64::EPSILON {
            return;
        }
        // Screen pixels into canvas units, the same conversion the pick tolerance
        // makes and for the same reason: canvas units span a factor of 400 across the
        // zoom range, so a threshold expressed in them would mean something different
        // at each end (§25.2).
        let min_canvas = min_screen / self.scale;
        let slots = &self.slots;
        self.links.retain_recorded(|link| {
            let (Some(from), Some(to)) = (slots.get(link.from as usize), slots.get(link.to as usize)) else {
                // An edge naming a node that does not exist is skipped when drawn.
                // Keeping it here keeps "hidden" meaning "too short to see".
                return true;
            };
            let bounds = link_curve(
                Rect::from_origin_size(from.pos, from.size),
                Rect::from_origin_size(to.pos, to.size),
            )
            .bounding_box()
            .size();
            // The diagonal of the box the curve occupies, not the chord: a link that
            // bows away and comes back is visible even when its endpoints nearly
            // coincide. It is also the conservative choice — never smaller than either
            // side — and a rule that removes picture should err towards keeping it.
            bounds.width.hypot(bounds.height) >= min_canvas
        });
    }

    /// Re-records the far-field node set when the viewport leaves the painted region.
    ///
    /// The recorded scene lives in canvas coordinates, so panning and zooming inside
    /// the region cost one `Affine` and nothing else. The margin is what turns
    /// "re-record every frame" into "re-record when you have travelled half a
    /// screen": it is bought with a larger scene, which the paint pass appends every
    /// frame either way, so it should be generous but not unbounded.
    fn refresh_far_region(&mut self) {
        if self
            .far
            .region
            .is_some_and(|r| region_covers(r, self.visible_rect, region_slack(self.far_overscan)))
        {
            return;
        }

        let region = self.visible_rect.inflate(
            self.visible_rect.width() * self.far_overscan,
            self.visible_rect.height() * self.far_overscan,
        );

        self.far_records += 1;
        let mut candidates = std::mem::take(&mut self.scratch_candidates);
        self.index.candidates(region, &mut candidates);
        self.far.nodes.clear();
        for &index in &candidates {
            let slot = &self.slots[index];
            if Rect::from_origin_size(slot.pos, slot.size).overlaps(region) {
                self.far.nodes.push(index);
            }
        }
        self.visits += candidates.len() as u64;
        self.scratch_candidates = candidates;
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
        // Links go under the nodes, at every detail level: they are the graph's
        // structure, and a graph too small to show a node's controls still has to
        // show what is wired to what.
        if !self.links.is_empty() {
            self.link_repaints += 1;
            let stroke = Stroke::new(self.link_style.width);
            let hovered = self.hovered.and_then(CanvasHit::link);

            // One path per style, and one command per path. The curves keep apart
            // because each link starts a subpath; what they lose is their order
            // relative to each other, which is why picking no longer claims to return
            // the topmost link. Stroking each link on its own instead costs 15x per
            // curve in every frame the scene is appended — which is every frame, idle
            // or not (§31.1).
            let mut plain = std::mem::take(&mut self.scratch_links);
            let mut hot = std::mem::take(&mut self.scratch_hot_links);
            plain.truncate(0);
            hot.truncate(0);
            for &edge in self.links.recorded() {
                let link = self.links.edge(edge);
                let (Some(from), Some(to)) = (self.slots.get(link.from as usize), self.slots.get(link.to as usize))
                else {
                    continue;
                };
                let path = if hovered == Some(edge as usize) {
                    &mut hot
                } else {
                    &mut plain
                };
                push_link(
                    path,
                    Rect::from_origin_size(from.pos, from.size),
                    Rect::from_origin_size(to.pos, to.size),
                );
            }
            if !plain.is_empty() {
                painter.stroke(&plain, &stroke, self.link_style.color).draw();
            }
            // Last, so the highlighted link is the one on top — the only ordering the
            // batch still guarantees, and the only one that matters.
            if !hot.is_empty() {
                painter.stroke(&hot, &stroke, self.link_style.hover_color).draw();
            }
            self.scratch_links = plain;
            self.scratch_hot_links = hot;
        }

        if !self.far.active {
            return;
        }
        self.far_repaints += 1;
        // The whole set in one call, straight into this widget's cached scene, which
        // is in canvas coordinates so panning and zooming re-use it via the layer
        // transform without re-encoding anything. Handing over the set rather than
        // calling per node is what lets the application group its fills: the frame
        // costs commands, and a command's content is nearly free next to its
        // existence (§31.1).
        let mut batch = std::mem::take(&mut self.scratch_far);
        batch.clear();
        batch.extend(self.far.nodes.iter().map(|&index| {
            let slot = &self.slots[index];
            (index, Rect::from_origin_size(slot.pos, slot.size))
        }));
        self.source.paint_far(&batch, self.scale, painter);
        self.scratch_far = batch;
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

/// Copies what a pick just learned into the published statistics.
///
/// Everything else in [`CanvasStats`] is refreshed during layout, which is the
/// cheapest honest place for it. Picks cannot use it: a hover is meant *not* to run
/// layout (§25.4), so counters that waited for one would report zero for the very
/// scenario they exist to measure — and the one place they would be seen is a
/// benchmark comparing them before and after.
fn publish_hit_stats(stats: &Cell<CanvasStats>, content: &CanvasContent) {
    let mut current = stats.get();
    current.hovered = content.hovered;
    current.counters.hit_queries = content.hit_queries;
    current.counters.hit_node_tests = content.hit_node_tests;
    current.counters.hit_curve_tests = content.hit_curve_tests;
    stats.set(current);
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
    /// How far past the viewport the far-field scene and the link set are recorded.
    far_overscan: f64,
    /// Mirror of the content's counters, refreshed at the end of each layout.
    stats: Cell<CanvasStats>,
    /// Current pointer gesture.
    drag: Drag,
    /// Whether only the node under the pointer gets interactive controls.
    controls_on_hover: bool,
    /// Whether the canvas acts on the primary button itself.
    builtin_gestures: bool,
    /// Where the detail levels switch over for readability.
    thresholds: DetailThresholds,
    /// What the tree may cost, in widgets.
    budget: DetailBudget,
    /// Smallest and largest permitted zoom.
    zoom_limits: (f64, f64),
    /// Whether the source has been told this canvas's id yet.
    attached: bool,
    /// Reused buffer for the peers a move has to be broadcast to.
    peers: Vec<WidgetId>,
    /// Links handed to [`CanvasLayer::with_links`] before the canvas was in a tree.
    pending_links: Option<Vec<Link>>,
    link_style: LinkStyle,
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
            far_overscan: FAR_OVERSCAN,
            stats: Cell::new(CanvasStats {
                zoom: 1.0,
                ..CanvasStats::default()
            }),
            drag: Drag::None,
            controls_on_hover: false,
            builtin_gestures: true,
            thresholds: DetailThresholds::default(),
            budget: DetailBudget::default(),
            attached: false,
            peers: Vec::new(),
            zoom_limits: (0.02, 8.0),
            pending_links: None,
            link_style: LinkStyle::default(),
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
    /// Adds edges between nodes.
    ///
    /// Indices into the node array given to [`new`](Self::new); an edge naming a node
    /// that does not exist is skipped when drawn rather than rejected here, because
    /// the graph is the application's to validate.
    ///
    /// Held here and handed down at the first layout: a `WidgetPod` gives its widget
    /// to the arena on insertion, so the canvas cannot reach its own content between
    /// construction and being in a tree.
    pub fn with_links(mut self, links: Vec<Link>) -> Self {
        self.pending_links = Some(links);
        self
    }

    /// Restyles the links.
    /// Sets how far past the viewport the far field and the link set are recorded, as
    /// a fraction of the viewport.
    ///
    /// The margin that turns "re-record every frame" into "re-record every few hundred"
    /// (§20.6a). It is bought with a bigger recorded scene, and the scene is what the
    /// rasteriser is charged for every frame (§32.3), so the two sides of the trade are
    /// re-recordings and path segments. [`FAR_OVERSCAN`](Self::DEFAULT_FAR_OVERSCAN) is
    /// what §35.2 measured the trade at.
    pub fn with_far_overscan(mut self, fraction: f64) -> Self {
        self.far_overscan = fraction.max(0.0);
        self
    }

    /// The default of [`Self::with_far_overscan`]: half a viewport on each side.
    pub const DEFAULT_FAR_OVERSCAN: f64 = FAR_OVERSCAN;

    pub fn with_link_style(mut self, style: LinkStyle) -> Self {
        self.link_style = style;
        self
    }

    pub fn with_controls_on_hover(mut self, enabled: bool) -> Self {
        self.controls_on_hover = enabled;
        self
    }

    /// Whether the canvas acts on the primary button itself. On by default.
    ///
    /// The seam an operator layer needs (§11, §38). The canvas's own primary-button
    /// gestures — drag the node under the pointer, pan when there is none — are a
    /// default, not the mechanism: an application whose keymap binds that button
    /// cannot have the canvas answering it first, because the canvas sits *below* the
    /// application's driver and Masonry routes to the deepest widget before it
    /// bubbles.
    ///
    /// Turning them off leaves the rest alone: the middle button still pans, the wheel
    /// still zooms, and the pointer still picks on every press and move — the pick is
    /// what an operator's context is made of, and it is measured not to cost a layout
    /// (§25.4).
    pub fn with_builtin_gestures(mut self, enabled: bool) -> Self {
        self.builtin_gestures = enabled;
        self
    }

    /// Sets where the detail levels switch over for readability.
    ///
    /// Policy, and the application's: how small a control may get before it stops
    /// being usable depends on how the node is drawn (§20.7).
    pub fn with_thresholds(mut self, thresholds: DetailThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Sets the ceiling on widgets this canvas may keep in the tree.
    ///
    /// The other half of the level decision, and the one that does not follow the
    /// zoom: see [`DetailBudget`]. An application tiling several canvases in one
    /// window should hand each of them a share of one window budget
    /// ([`DetailBudget::split`]), because the frame walks the window's tree and not
    /// any single canvas's.
    pub fn with_budget(mut self, budget: DetailBudget) -> Self {
        self.budget = budget;
        self
    }

    /// The cost ceiling in force.
    pub fn budget(&self) -> DetailBudget {
        self.budget
    }

    /// The canvas-space to viewport-space transform.
    ///
    /// Public because anything drawing over the canvas — an overlay, a rubber band,
    /// a tooltip anchored to a node — has to agree with it about where things are,
    /// and rederiving it from the zoom and the pan is how two answers start to
    /// differ.
    pub fn view(&self) -> Affine {
        self.view
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

    /// Moves a child from the parent's raw context, without telling the model.
    ///
    /// The seam an operator layer moves nodes through (§38.3). By then the operator
    /// has already written the position to the model — that is where the truth lives
    /// (§30) — and what is left is this view's own copy of the geometry. Three ways
    /// in, and the difference is who has already been told:
    ///
    /// * [`move_child`](Self::move_child) — a `WidgetMut`, from outside any pass;
    /// * this one — the parent widget, holding an `EventCtx`, in the same event;
    /// * the canvas's own drag, which also calls [`NodeSource::moved`] because there the canvas is the one that heard
    ///   the user.
    ///
    /// Nothing is broadcast to the other views of the same model: the caller wrote the
    /// model and knows who else is looking at it.
    pub fn move_child_raw(&mut self, index: usize, pos: Point, ctx: &mut RawCtx<'_>) {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        content.store_child_pos(index, pos).apply(&mut raw);
    }

    /// Pans the view from the parent's raw context.
    ///
    /// The view twin of [`move_child_raw`](Self::move_child_raw), and it exists for the
    /// same caller: an operator layer that has taken the primary button owns panning
    /// too, and the driver holding an `EventCtx` is the one that has to carry it in.
    /// Only the canvas is dirtied — child positions are in canvas coordinates, so a
    /// view change moves nobody (§22).
    pub fn pan_raw(&mut self, delta: Vec2, ctx: &mut RawCtx<'_>) {
        let view = Affine::translate(delta) * self.view;
        if self.store_view(view) {
            ctx.request_layout();
        }
    }

    /// Reaches node `index`'s widget, if it currently has one.
    ///
    /// The way a change in the model reaches a view that is already on screen. A node
    /// widget is built from the model and then keeps its own copy — it has to, because
    /// it is painted far more often than it is built — so a model change that happens
    /// while the node is materialised has to be pushed into it.
    ///
    /// Returns `false` when the node has no widget: it is off screen, or the canvas is
    /// in the far field. That is not a failure and needs no repair — a node without a
    /// widget reads the model when it is next built. What a far-field canvas *draws*
    /// comes from [`NodeSource::paint_far`], and if a change affects that drawing the
    /// caller invalidates it by moving the node, not by this.
    pub fn update_child(
        this: &mut WidgetMut<'_, Self>,
        index: usize,
        f: impl FnOnce(WidgetMut<'_, dyn Widget>),
    ) -> bool {
        let mut content = this.ctx.get_mut(&mut this.widget.content);
        let Some(pod) = content.widget.slots.get_mut(index).and_then(|slot| slot.pod.as_mut()) else {
            return false;
        };
        f(content.ctx.get_mut(pod));
        true
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

    /// What is under a point given in this widget's coordinates.
    ///
    /// Nodes first, then links; `None` for empty canvas. Answered from the model,
    /// so it works below the far-field threshold where no node has a widget, and for
    /// links, which never do.
    pub fn hit_test(this: &mut WidgetMut<'_, Self>, pos: Point) -> Option<CanvasHit> {
        let canvas_pos = this.widget.view.inverse() * pos;
        let scale = this.widget.hit_scale(this.ctx.window_transform());
        let content = this.ctx.get_mut(&mut this.widget.content);
        let hit = content.widget.hit(canvas_pos, scale);
        publish_hit_stats(&this.widget.stats, content.widget);
        hit
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

    /// Moves a child from an event handler — the drag, as opposed to the programmatic
    /// [`move_child`](Self::move_child).
    ///
    /// The two differ in exactly one thing and it is the point of the split: a drag is
    /// the *user* moving a node, so the model has to hear about it and so do the other
    /// canvases showing the same model. A programmatic move is what those other
    /// canvases then receive, and it must not bounce back out again.
    fn move_child_at(&mut self, index: usize, pos: Point, ctx: &mut EventCtx<'_>) {
        let mut peers = std::mem::take(&mut self.peers);
        peers.clear();
        {
            let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
            let invalidate = content.store_child_pos(index, pos);
            if invalidate != Invalidate::Nothing {
                content.source.moved(index, pos, &mut peers);
            }
            invalidate.apply(&mut raw);
        }
        for &peer in &peers {
            // A mutate callback rather than a direct reach: another canvas is not this
            // widget's child, and the mutate pass is where a widget outside the current
            // subtree may legally be changed. It runs before the next layout, so the
            // other areas move in the same frame.
            ctx.mutate_later(peer, move |mut widget| {
                let mut canvas = widget.downcast::<Self>();
                Self::move_child(&mut canvas, index, pos);
            });
        }
        self.peers = peers;
    }

    /// How many screen pixels one canvas unit covers.
    ///
    /// The zoom is only part of it: the canvas may itself be scaled by whatever it
    /// sits inside — a region with its own `ui_scale`, a device scale factor — and a
    /// tolerance in screen pixels has to account for the whole chain. The content
    /// widget's own transform is exactly this product, but reading it back through
    /// the arena during an event would cost more than multiplying two numbers.
    fn hit_scale(&self, window: Affine) -> f64 {
        scale_of(window) * self.zoom()
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

    /// Picks what is under a point, from an event handler.
    fn hit_at(&mut self, pos: Point, ctx: &mut EventCtx<'_>) -> Option<CanvasHit> {
        let canvas_pos = self.view.inverse() * pos;
        let scale = self.hit_scale(ctx.window_transform());
        let (content, _) = ctx.get_raw_mut(&mut self.content);
        let hit = content.hit(canvas_pos, scale);
        publish_hit_stats(&self.stats, content);
        hit
    }

    /// Records what the pointer is over and asks for whatever that changes.
    ///
    /// A hover changes pixels — the link under the pointer is highlighted — and with
    /// `controls_on_hover` it also changes which node has real controls, which is a
    /// layout. Keeping the two apart is the same distinction the link layer makes
    /// between repainting a curve and re-choosing the set (§24.3): a highlight must
    /// not drag a relayout of the graph behind it.
    fn hover(&mut self, pos: Point, ctx: &mut EventCtx<'_>) -> Option<CanvasHit> {
        let hit = self.hit_at(pos, ctx);
        self.set_hovered(hit, ctx);
        if self.controls_on_hover {
            self.set_active(hit.and_then(CanvasHit::node), ctx);
        }
        hit
    }

    /// Stores what the pointer is over, repainting if the highlight changed.
    fn set_hovered(&mut self, hit: Option<CanvasHit>, ctx: &mut EventCtx<'_>) {
        let (content, mut raw) = ctx.get_raw_mut(&mut self.content);
        if content.set_hovered(hit) {
            raw.request_paint_only();
        }
        publish_hit_stats(&self.stats, content);
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
                // A press picks whatever a move would have picked, whether or not the
                // canvas is going to act on it. The record is what an operator layer
                // above reads as its context (§38.3): a driver holding an `EventCtx`
                // cannot hit-test a child, and this costs the pick the drag decision
                // needed anyway.
                let hit = self.hover(pos, ctx);
                self.drag = match e.button {
                    // Left button drags a node if there is one under the pointer,
                    // and pans otherwise — unless the application has taken the
                    // primary button for its keymap.
                    Some(PointerButton::Primary) if self.builtin_gestures => match hit {
                        Some(CanvasHit::Node { index, pos: child_pos }) => Drag::Node {
                            index,
                            grab: canvas_pos - child_pos,
                        },
                        // A link is pickable but not draggable by the canvas:
                        // selection and rewiring are operators (§11). A press on a
                        // curve pans, as it did before curves could be picked at all.
                        Some(CanvasHit::Link { .. }) | None => Drag::Pan { last: pos },
                    },
                    // Middle button always pans, as in Blender. Not a gesture an
                    // operator layer competes for, so it is not switched off with the
                    // others.
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
                    Drag::None => {
                        self.hover(pos, ctx);
                    },
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
            PointerEvent::Leave(_) => {
                self.set_hovered(None, ctx);
                if self.controls_on_hover {
                    self.set_active(None, ctx);
                }
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

        // The first layout is the first moment this widget knows its own id and can
        // hand it to the source. Construction is too early: a `CanvasLayer` is built
        // before it is a widget, and the id is minted when it enters the tree.
        if !self.attached {
            self.attached = true;
            let id = ctx.widget_id();
            let (content, _) = ctx.get_raw_mut(&mut self.content);
            content.source.attached(id);
        }

        // Clip to the viewport so children panned out of view cannot paint over the
        // surrounding UI, and so Masonry excludes them from hit testing.
        ctx.set_clip_path(Rect::from_origin_size(Point::ORIGIN, size));

        let visible_rect = self.visible_canvas_rect();
        let zoom = self.zoom();
        // Only the readability half of the decision can be taken here: the cost half
        // needs the number of visible nodes, which the cull computes (§29.2).
        let readable = self.thresholds.for_scale(zoom);
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
            if let Some(links) = self.pending_links.take() {
                let count = content.slots.len();
                content.links = LinkLayer::new(links, count);
                content.links.invalidate();
            }
            content.link_style = self.link_style;
            content.far_overscan = self.far_overscan;
            content.links.set_slack(region_slack(self.far_overscan));
            content.controls_on_hover = self.controls_on_hover;
            content.visible_rect = visible_rect;
            content.scale = zoom;
            content.readable = Some(readable);
            content.budget = self.budget;
            if view_dirty {
                raw.set_transform(view);
            }

            content.cull();
            if std::mem::take(&mut content.far.dirty) | content.links.take_repaint() {
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

        let (content, _) = ctx.get_raw(&mut self.content);
        self.stats.set(CanvasStats {
            total: content.slots.len(),
            materialised: content.live.len(),
            visible: content.visible.len(),
            detail: content.detail,
            zoom,
            recorded_far: content.far.nodes.len(),
            recorded_links: content.links.recorded().len(),
            hidden_links: content.links.hidden(),
            hovered: content.hovered,
            counters: CanvasCounters {
                content_layouts: content.layouts,
                child_layouts: content.child_layouts,
                composes: content.composes,
                builds: content.builds,
                level_switches: content.level_switches,
                far_repaints: content.far_repaints,
                far_records: content.far_records,
                slot_visits: content.visits,
                link_repaints: content.link_repaints,
                link_reselects: content.links.refreshes(),
                hit_queries: content.hit_queries,
                hit_node_tests: content.hit_node_tests,
                hit_curve_tests: content.hit_curve_tests,
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

    // --- MARK: HIT TESTS

    /// A source whose nodes are rounded rectangles, like a real one.
    struct RoundedSource {
        shape: blazy_shape::ShapeHit,
        size: Size,
    }

    impl RoundedSource {
        fn new(size: Size) -> Self {
            Self {
                shape: blazy_shape::ShapeHit::fill(masonry::kurbo::RoundedRect::from_rect(
                    Rect::from_origin_size(Point::ORIGIN, size),
                    12.0,
                )),
                size,
            }
        }
    }

    impl NodeSource for RoundedSource {
        fn build(&mut self, _index: usize, _detail: Detail) -> NewWidget<dyn Widget> {
            unimplemented!("these tests never materialise a widget: that is the point")
        }

        fn hit(&mut self, _index: usize, rect: Rect, point: Point) -> bool {
            assert_eq!(rect.size(), self.size);
            self.shape.contains(point - rect.origin().to_vec2(), 1.0)
        }
    }

    /// A canvas content over the given node rectangles and edges, already culled.
    ///
    /// Built directly rather than through a harness: everything under test answers
    /// from the model, so a widget tree would only add a way for the test to be
    /// about something else.
    fn content(rects: &[Rect], edges: Vec<Link>, visible: Rect) -> CanvasContent {
        content_at_scale(rects, edges, visible, 1.0)
    }

    /// The same, at a given zoom — which is what the short-link rule reads.
    fn content_at_scale(rects: &[Rect], edges: Vec<Link>, visible: Rect, scale: f64) -> CanvasContent {
        let size = rects.first().map_or(Size::ZERO, Rect::size);
        let slots = rects
            .iter()
            .map(|r| Slot {
                pos: r.origin(),
                size: r.size(),
                pod: None,
                built: None,
            })
            .collect();
        let mut content = CanvasContent::new(slots, Box::new(RoundedSource::new(size)));
        content.links = LinkLayer::new(edges, rects.len());
        content.links.invalidate();
        content.detail = Some(Detail::Full);
        content.visible_rect = visible;
        content.scale = scale;
        content.cull();
        content
    }

    fn node_rect(x: f64, y: f64) -> Rect {
        Rect::from_origin_size(Point::new(x, y), Size::new(100.0, 60.0))
    }

    const EVERYTHING: Rect = Rect::new(-1000.0, -1000.0, 1000.0, 1000.0);

    /// The claim of §6.1: the hit geometry is the shape, not the box it sits in.
    #[test]
    fn a_point_in_the_corner_of_a_node_misses_it() {
        let mut canvas = content(&[node_rect(0.0, 0.0)], Vec::new(), EVERYTHING);
        let corner = Point::new(1.0, 1.0);

        assert!(node_rect(0.0, 0.0).contains(corner), "inside the rectangle");
        assert_eq!(canvas.hit(corner, 1.0), None, "outside the rounded body");
        assert!(matches!(
            canvas.hit(Point::new(50.0, 30.0), 1.0),
            Some(CanvasHit::Node { index: 0, .. })
        ));
    }

    /// Picking a link is the case Masonry cannot answer at all: a curve is not a
    /// widget, so nothing in the tree knows it is there.
    #[test]
    fn a_link_is_picked_along_its_curve() {
        let mut canvas = content(
            &[node_rect(0.0, 0.0), node_rect(400.0, 0.0)],
            vec![Link::new(0, 1)],
            EVERYTHING,
        );

        // The curve runs from the right edge of one node to the left edge of the
        // other, both centred on y = 30.
        assert!(matches!(
            canvas.hit(Point::new(250.0, 30.0), 1.0),
            Some(CanvasHit::Link { edge: 0, .. })
        ));
        assert_eq!(canvas.hit(Point::new(250.0, 90.0), 1.0), None);
    }

    /// Nodes are painted over links, so they are picked over links too (§25.3).
    #[test]
    fn a_node_wins_over_a_link_running_under_it() {
        let mut canvas = content(
            &[node_rect(0.0, 0.0), node_rect(400.0, 0.0), node_rect(200.0, 0.0)],
            vec![Link::new(0, 1)],
            EVERYTHING,
        );
        let on_both = Point::new(250.0, 30.0);

        assert!(
            blazy_shape::near_segment(
                crate::links::link_curve(node_rect(0.0, 0.0), node_rect(400.0, 0.0)).into(),
                on_both,
                4.0
            ),
            "the point really is on the curve"
        );
        assert!(matches!(
            canvas.hit(on_both, 1.0),
            Some(CanvasHit::Node { index: 2, .. })
        ));
    }

    /// The tolerance is in screen pixels, so the same canvas point picks a link when
    /// the canvas is zoomed out and misses it when zoomed in (§25.2).
    #[test]
    fn the_link_tolerance_follows_the_zoom() {
        let mut canvas = content(
            &[node_rect(0.0, 0.0), node_rect(400.0, 0.0)],
            vec![Link::new(0, 1)],
            EVERYTHING,
        );
        // Three canvas units off the curve, with a stroke one unit wide either side.
        let near = Point::new(250.0, 33.0);

        assert!(canvas.hit(near, 1.0).is_some(), "3 px away at 1x");
        assert_eq!(canvas.hit(near, 8.0), None, "24 px away at 8x");
        assert!(canvas.hit(near, 0.25).is_some(), "well under a pixel at 0.25x");
    }

    /// What can be picked is what is drawn, including where that is not enough: a
    /// link whose ends are both outside the recorded region is neither (§24.4).
    #[test]
    fn a_link_that_is_not_drawn_is_not_picked() {
        let mut canvas = content(
            &[node_rect(-5000.0, 0.0), node_rect(5000.0, 0.0)],
            vec![Link::new(0, 1)],
            Rect::new(-200.0, -200.0, 200.0, 200.0),
        );

        assert!(canvas.links.recorded().is_empty(), "neither end is near the viewport");
        assert_eq!(canvas.hit(Point::new(0.0, 30.0), 1.0), None);
    }

    /// Nodes below the far-field threshold have no widget at all, and must still be
    /// pickable — the reason picking asks the model and not the tree (§20.6).
    #[test]
    fn a_node_with_no_widget_is_still_picked() {
        let mut canvas = content(&[node_rect(0.0, 0.0)], Vec::new(), EVERYTHING);
        canvas.detail = Some(Detail::Box);
        canvas.cull();

        assert!(canvas.live.is_empty(), "far field: nothing is materialised");
        assert!(matches!(
            canvas.hit(Point::new(50.0, 30.0), 1.0),
            Some(CanvasHit::Node { index: 0, .. })
        ));
    }

    /// Picking must not walk the graph: the candidates come from the grid.
    #[test]
    fn picking_does_not_examine_the_whole_graph() {
        let rects: Vec<Rect> = (0..4000)
            .map(|i| node_rect((i % 80) as f64 * 220.0, (i / 80) as f64 * 220.0))
            .collect();
        let mut canvas = content(&rects, Vec::new(), Rect::new(0.0, 0.0, 1100.0, 750.0));

        let before = canvas.hit_node_tests;
        canvas.hit(Point::new(50.0, 30.0), 1.0);
        let examined = canvas.hit_node_tests - before;

        assert!(examined < 64, "examined {examined} geometries of 4000");
    }

    /// A hover highlights a curve, and a highlight is a repaint. Nothing here is
    /// allowed to ask for layout — that is what `set_active` is for.
    #[test]
    fn only_a_link_hover_asks_for_a_repaint() {
        let mut canvas = content(&[node_rect(0.0, 0.0)], Vec::new(), EVERYTHING);
        let node = Some(CanvasHit::Node {
            index: 0,
            pos: Point::ORIGIN,
        });
        let link = Some(CanvasHit::Link {
            edge: 0,
            link: Link::new(0, 1),
        });

        assert!(!canvas.set_hovered(node), "a node highlight is not drawn");
        assert!(!canvas.set_hovered(node), "and an unchanged hover is not a change");
        assert!(canvas.set_hovered(link), "arriving on a curve repaints it");
        assert!(canvas.set_hovered(None), "and leaving it repaints it back");
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

    // --- MARK: budget

    /// The budget picks the most detailed level that fits, and nothing finer.
    #[test]
    fn the_budget_takes_the_finest_level_that_fits() {
        let budget = DetailBudget {
            widgets: 1000,
            full_cost: 4,
            simplified_cost: 1,
            hysteresis: 0.0,
        };
        // 100 nodes cost 400 widgets in full; 300 cost 1200 and do not fit, but the
        // same 300 cost 300 simplified.
        assert_eq!(budget.level_for(100, None), Detail::Full);
        assert_eq!(budget.level_for(300, None), Detail::Simplified);
        assert_eq!(budget.level_for(1001, None), Detail::Box);
    }

    /// An unlimited budget leaves the zoom thresholds as the only rule.
    #[test]
    fn an_unlimited_budget_never_binds() {
        let budget = DetailBudget::unlimited();
        assert_eq!(budget.level_for(1_000_000, None), Detail::Full);
    }

    /// Coming back up costs more than staying put, which is what stops the flapping.
    ///
    /// The gap is the measured jitter of the visible set during a pan (§29.1): with
    /// the two thresholds equal, a set wobbling between 249 and 251 nodes switches
    /// level twice a second and rebuilds every visible node each time.
    #[test]
    fn the_budget_is_hysteretic() {
        let budget = DetailBudget {
            widgets: 1000,
            full_cost: 4,
            simplified_cost: 1,
            hysteresis: 0.25,
        };
        // 240 nodes cost 960 widgets: inside the budget, so `Full` holds...
        assert_eq!(budget.level_for(240, Some(Detail::Full)), Detail::Full);
        // ...but is not reached from below, where the ceiling is 750.
        assert_eq!(budget.level_for(240, Some(Detail::Simplified)), Detail::Simplified);
        // Well clear of the margin, it is reached.
        assert_eq!(budget.level_for(180, Some(Detail::Simplified)), Detail::Full);
    }

    /// A window budget divided between the canvases sharing the window.
    #[test]
    fn a_split_budget_shares_one_ceiling() {
        let budget = DetailBudget::default().split(8);
        assert_eq!(budget.widgets, DEFAULT_WIDGET_BUDGET / 8);
        assert_eq!(budget.full_cost, DetailBudget::default().full_cost);
        // Dividing by nothing is the whole budget rather than a panic.
        assert_eq!(DetailBudget::default().split(0).widgets, DEFAULT_WIDGET_BUDGET);
    }

    /// Readability and cost are separate rules and the stricter one decides.
    ///
    /// Both directions matter: a zoom too small for a slider cannot be rescued by a
    /// generous budget, and a graph too dense cannot be rescued by a legible zoom.
    #[test]
    fn the_stricter_of_the_two_rules_wins() {
        let thresholds = DetailThresholds::default();
        let budget = DetailBudget {
            widgets: 1000,
            full_cost: 4,
            simplified_cost: 1,
            hysteresis: 0.0,
        };
        // Legible zoom, unaffordable set: cost decides.
        let readable = thresholds.for_scale(1.0);
        assert_eq!(readable.max(budget.level_for(400, None)), Detail::Simplified);
        // Affordable set, illegible zoom: readability decides.
        let readable = thresholds.for_scale(0.01);
        assert_eq!(readable.max(budget.level_for(1, None)), Detail::Box);
    }

    // --- MARK: SHORT LINKS

    /// Room for the long link of the tests below, which reaches out to x = 2000.
    const WIDE: Rect = Rect::new(-3000.0, -3000.0, 3000.0, 3000.0);

    /// A curve too short to see is dropped when the set is chosen, not when it is
    /// drawn — so it leaves the picture and the pointer's reach together.
    ///
    /// One link only, so "no link here" is unambiguous: with a second one on screen
    /// the pick tolerance at this zoom (screen pixels divided by 0.01) reaches far
    /// enough to find it, and the test would be measuring the tolerance instead.
    #[test]
    fn a_link_too_short_to_see_is_neither_drawn_nor_picked() {
        let rects = [node_rect(0.0, 0.0), node_rect(120.0, 0.0)];
        let mut canvas = content_at_scale(&rects, vec![Link::new(0, 1)], WIDE, 0.01);

        assert!(
            canvas.links.recorded().is_empty(),
            "a curve under a pixel long should not be recorded"
        );
        assert_eq!(canvas.links.hidden(), 1);
        // Between the two nodes, where the curve would run.
        assert_eq!(canvas.hit(Point::new(110.0, 30.0), 0.01), None);
    }

    /// And it drops only the short one: the rule is a threshold, not an off switch.
    #[test]
    fn the_short_link_rule_keeps_the_links_that_are_visible() {
        let rects = [
            node_rect(0.0, 0.0),
            node_rect(120.0, 0.0),
            node_rect(0.0, 400.0),
            node_rect(2000.0, 400.0),
        ];
        let edges = vec![Link::new(0, 1), Link::new(2, 3)];
        let canvas = content_at_scale(&rects, edges, WIDE, 0.01);

        assert_eq!(canvas.links.recorded(), &[1], "the long link survives");
        assert_eq!(canvas.links.hidden(), 1);
    }

    /// The rule needs no "only when zoomed out" clause because an on-screen length
    /// grows with the zoom. This is what says so: at a zoom anything is readable at,
    /// it hides nothing at all.
    #[test]
    fn the_short_link_rule_is_silent_at_a_working_zoom() {
        let rects = [node_rect(0.0, 0.0), node_rect(120.0, 0.0)];
        let mut canvas = content_at_scale(&rects, vec![Link::new(0, 1)], EVERYTHING, 1.0);

        assert_eq!(canvas.links.recorded(), &[0]);
        assert_eq!(canvas.links.hidden(), 0);
        assert!(matches!(
            canvas.hit(Point::new(110.0, 30.0), 1.0),
            Some(CanvasHit::Link { edge: 0, .. })
        ));
    }

    // --- MARK: BATCHING

    /// Strokes some curves, either as one path or as one command each.
    ///
    /// Two widgets' worth of behaviour in one, because the whole question is whether
    /// the two are the same picture.
    struct Curves {
        links: Vec<(Rect, Rect)>,
        batched: bool,
    }

    impl Widget for Curves {
        type Action = NoAction;

        fn measure(
            &mut self,
            _ctx: &mut MeasureCtx<'_>,
            _props: &PropertiesRef<'_>,
            _axis: Axis,
            len_req: LenReq,
            _cross: Option<Length>,
        ) -> Length {
            match len_req {
                LenReq::MinContent | LenReq::MaxContent => Length::ZERO,
                LenReq::FitContent(space) => space,
            }
        }

        fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {}

        fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
            let stroke = Stroke::new(3.0);
            let colour = Color::from_rgb8(0xd0, 0xd0, 0xe0);
            if self.batched {
                let mut path = BezPath::new();
                for &(from, to) in &self.links {
                    push_link(&mut path, from, to);
                }
                if !path.is_empty() {
                    painter.stroke(&path, &stroke, colour).draw();
                }
            } else {
                for &(from, to) in &self.links {
                    let mut path = BezPath::new();
                    push_link(&mut path, from, to);
                    painter.stroke(&path, &stroke, colour).draw();
                }
            }
        }

        fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

        fn children_ids(&self) -> ChildrenIds {
            ChildrenIds::new()
        }

        fn accessibility_role(&self) -> Role {
            Role::GenericContainer
        }

        fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
    }

    fn drawn(links: &[(Rect, Rect)], batched: bool) -> Vec<u8> {
        let mut harness = masonry::testing::TestHarness::create_with_size(
            masonry::theme::default_property_set(),
            NewWidget::new(Curves {
                links: links.to_vec(),
                batched,
            }),
            masonry::dpi::PhysicalSize::new(400, 200),
        );
        harness.render().into_raw()
    }

    /// The claim the whole batch rests on: `move_to` starts a subpath, and separate
    /// subpaths are stroked separately. If they were joined up, a segment would run
    /// from the end of one link to the start of the next and these would differ.
    #[test]
    fn a_batched_stroke_draws_what_separate_strokes_draw() {
        let links = [
            (node_rect(20.0, 20.0), node_rect(240.0, 30.0)),
            (node_rect(20.0, 120.0), node_rect(240.0, 130.0)),
        ];
        assert_eq!(
            drawn(&links, true),
            drawn(&links, false),
            "one command with two subpaths must paint what two commands paint"
        );

        // And the space between the two links stays empty, which is the same claim
        // read the other way round.
        let empty = drawn(&[], true);
        let batched = drawn(&links, true);
        let midpoint = (100 * 400 + 200) * 4;
        assert_eq!(
            batched[midpoint..midpoint + 4],
            empty[midpoint..midpoint + 4],
            "no segment joins the end of one link to the start of the next"
        );
    }

    /// Where the batch is *not* pixel-identical, and it is worth knowing which way.
    ///
    /// Two curves leaving the same point overlap, and their antialiased coverage is
    /// composited once inside a shared command against twice as separate ones. It
    /// moved 50 pixels of 46 800 in the `canvas_with_links` snapshot, by at most 25
    /// of 255, all of them on curve overlaps — small, real, and not something to
    /// discover later from a failing gate.
    #[test]
    fn overlapping_curves_composite_once_in_a_batch() {
        let shared = node_rect(20.0, 90.0);
        let links = [(shared, node_rect(240.0, 20.0)), (shared, node_rect(240.0, 150.0))];
        assert_ne!(
            drawn(&links, true),
            drawn(&links, false),
            "if this ever matches, the difference the snapshot records has gone away"
        );
    }
}
