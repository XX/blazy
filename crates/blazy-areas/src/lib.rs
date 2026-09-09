//! Blender-style screen areas for Masonry: one widget tree tiled by a split tree.
//!
//! This crate answers the second half of the Phase 0 question. Phase 0 established
//! that the cost of a frame is the cost of walking the widget tree, and that the
//! tree is *per window* (`rnd/architecture.md` §20.2). A node canvas escaped that
//! by virtualising — but a Blender screen puts six or eight editors in the same
//! window at once, and nothing measured so far says whether their costs add up or
//! whether a splitter drag relays out everything on screen.
//!
//! Five claims are under test. The first three are about the tiling (§21), the last
//! two about what lives inside an area (§22).
//!
//! 1. **Areas do not add up.** Splitting a window into more areas does not add widgets to the tree, it divides the same
//!    viewport into smaller pieces. The total live widget count should therefore be roughly flat as the area count
//!    grows, not proportional to it.
//!
//! 2. **A splitter drag disturbs its two neighbours, not the screen.** Only the areas whose rect actually changes may
//!    re-run layout. For a splitter between two leaves that is two areas regardless of how many the screen holds; for
//!    the root splitter it is inherently half the screen, and that asymmetry is a property of tiling, not a defect.
//!
//! 3. **An idle screen is idle.** Areas exist as data even when nothing about them changes, and computing rects for
//!    them every frame must not be mistaken for laying them out.
//!
//! 4. **`ui_scale` is a layout input, and a local one.** Changing a region's interface scale has to reach that region's
//!    layout, and has to reach nothing else — not the area around it, not the areas beside it.
//!
//! 5. **`view` is not a layout input.** Panning and zooming a region's content is a transform at composition time.
//!    Mixing the two knobs means re-running layout on every frame of a zoom, which is the mistake §9 exists to warn
//!    about.
//!
//! # Structure
//!
//! [`SplitTree`] is pure geometry and knows nothing of widgets; [`AreaScreen`] is
//! the Masonry widget that owns one child per area and places it at the rect the
//! tree computed. §8 asks for exactly this seam, so the tree can later be
//! serialised, or replaced by a vertex-and-edge graph, without touching the widget.
//! [`AreaContent`] fills one area with regions, each carrying its own [`UiScale`].
//! [`Workspace`] is where the tree meets what fills each of its areas, because the
//! tree deliberately does not know (§41.5).
//!
//! # Operations, and the one that does not fit
//!
//! [`AreaScreen`] splits, joins, swaps, maximizes and restores, and a [`Workspace`]
//! writes the result to a file and reads it back (§41). All of them go through the seam
//! above, and all of them obey one rule that is a requirement rather than an
//! optimisation: **an area that survives an operation keeps its widget**. An
//! [`AreaId`] is the identity, nothing renumbers one, and what an area's widget holds —
//! a view, a selection, materialised nodes — lives nowhere else (§30).
//!
//! **Join is where the binary tree stops being enough, and by how much is measured.**
//! Blender merges any two areas whose border coincides; a tree can only merge
//! *siblings*. On eight areas, ten pairs share a whole border and four of them are
//! siblings; on sixteen, eight of twenty-four (§41.1). That is not a defect to fix
//! here — it is the price §8 named in advance, and the vertex-and-edge graph it
//! recommends instead is its own work with its own numbers.
//!
//! # What is missing
//!
//! Not the finished subsystem yet. There is no detach into a second OS window, and no
//! regions beyond a header and a main view — no toolbar, no sidebar, no footer.
//!
//! One limit is not a matter of features and will not go away by writing more of
//! them: **Masonry's own widgets do not honour [`UiScale`]**. Nothing in
//! `masonry::widgets` reads it, and their sizes come from the theme's
//! `DefaultProperties`, which is one map per application rather than one per region.
//! The test `masonry_widgets_do_not_follow_ui_scale` pins that down, and §22.1 says
//! what would have to change upstream.

#![warn(missing_docs, unreachable_pub)]

mod region;
mod tree;
mod workspace;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, EventCtx, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PointerEvent,
    PropertiesMut, PropertiesRef, RegisterCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Rect, Size};
use masonry::layout::{AsUnit, LenReq, Length, SizeDef};
use masonry::peniko::Color;
use masonry::ui_events::pointer::{PointerButton, PointerUpdate};

pub use crate::region::{AreaContent, RegionCounters, RegionKind, UiScale};
pub use crate::tree::{AreaId, Bar, NodeId, SplitTree, ratio_at};
pub use crate::workspace::{Workspace, WorkspaceError};

/// Thickness of a splitter, in logical pixels.
const BAR_THICKNESS: f64 = 4.0;

/// How far either side of a splitter still counts as grabbing it.
///
/// A four pixel bar is a four pixel target, which is below what a pointer can
/// reliably hit. Blender solves this the same way: the visible border is thin and
/// the grab zone around it is not.
const GRAB_SLOP: f64 = 3.0;

/// What the screen is doing right now, plus counters for the Phase 0.5 measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScreenStats {
    /// Areas the screen currently holds.
    pub areas: usize,
    /// Cumulative work counters.
    pub counters: ScreenCounters,
}

/// Cumulative counters, for spotting work that should not be happening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScreenCounters {
    /// Area widgets built, one per area that appeared.
    ///
    /// The counter the area operations are judged on (§41.2). A join, a swap and a
    /// maximize must leave it flat: the widget of a surviving area holds a view, a
    /// selection and materialised nodes, and rebuilding it throws all of that away —
    /// which §30 already priced. Only a split, and the first build of the screen, may
    /// raise it.
    pub builds: u64,
    /// Layout passes run on the screen itself.
    pub layouts: u64,
    /// Areas handed a border-box size different from their last one, summed over
    /// all passes.
    ///
    /// This is the honest measure of what a splitter drag costs. Masonry re-runs a
    /// child's layout when it is dirty *or* when its border-box size changed
    /// (`passes/layout.rs`, `run_layout_on`), and a resize is the half a screen
    /// controls — so counting resizes counts the work the screen is responsible for.
    pub area_resizes: u64,
    /// Areas Masonry considered dirty on entry to the screen's layout.
    ///
    /// Informational only, and only meaningful in a release build: with debug
    /// assertions on, `run_layout_on` deliberately marks every child as needing
    /// layout so it can check the parent visited them all. The benchmark runs
    /// under the `bench` profile, where this reads true.
    pub area_layouts: u64,
}

/// A window tiled into areas, each holding one widget.
///
/// The screen owns the rects. Areas are laid out at exactly the size the split tree
/// computed, never at a size they asked for: §8's "layout of an area runs against
/// rectangles we computed, so Masonry does not recompute the split layout".
pub struct AreaScreen {
    tree: SplitTree,
    /// Builds the widget for an area that appears.
    ///
    /// Kept rather than consumed by the constructor, and that is what an operation which
    /// *adds* an area runs into first: a split needs a child for the id the tree just
    /// handed out, and there is nobody else to ask.
    build: Box<dyn FnMut(AreaId) -> NewWidget<dyn Widget>>,
    /// One child per area, indexed by [`AreaId`]; `None` where an area was joined away.
    ///
    /// A tombstone rather than a compacted list, for the same reason the tree keeps free
    /// slots: the id is the caller's index, so removing an entry would move everyone
    /// after it. The hole is filled again when the id is handed out again.
    pods: Vec<Option<WidgetPod<dyn Widget>>>,
    /// The border-box size each area was last given, for counting real resizes.
    sizes: Vec<Option<Size>>,
    /// Where each area goes, recomputed every layout. Reused, so a resize allocates
    /// nothing.
    rects: Vec<(AreaId, Rect)>,
    /// Where each splitter goes. Also the hit-test source for a drag.
    bars: Vec<Bar>,
    /// The splitter currently being dragged, if any.
    drag: Option<NodeId>,
    /// The area the pointer is over, as of the last pointer event.
    ///
    /// Published because a driver above the screen cannot work it out for itself: it
    /// holds no widget context, so it cannot turn a window position into a local one —
    /// the same reason the canvas publishes what it picked (§38.3). An operation on
    /// "the area under the pointer" needs this and nothing else.
    hovered: Option<AreaId>,
    layouts: u64,
    builds: u64,
    area_resizes: u64,
    area_layouts: u64,
}

impl AreaScreen {
    /// Builds a screen over `tree`, calling `build` once per area.
    ///
    /// Every area is materialised up front, unlike the canvas's nodes: an area is
    /// on screen by definition, and there are tens of them rather than thousands.
    pub fn new(tree: SplitTree, mut build: impl FnMut(AreaId) -> NewWidget<dyn Widget> + 'static) -> Self {
        // By area id rather than by count: a tree loaded from a workspace, or one that
        // has been joined, holds ids that are not dense.
        let slots = tree.areas().map(|area| area + 1).max().unwrap_or(0);
        let mut pods: Vec<Option<WidgetPod<dyn Widget>>> = (0..slots).map(|_| None).collect();
        let mut builds = 0;
        for area in tree.areas() {
            pods[area] = Some(build(area).to_pod());
            builds += 1;
        }
        let count = tree.area_count();
        Self {
            tree,
            build: Box::new(build),
            pods,
            sizes: vec![None; slots],
            rects: Vec::with_capacity(count),
            bars: Vec::with_capacity(count.saturating_sub(1)),
            drag: None,
            hovered: None,
            layouts: 0,
            builds,
            area_resizes: 0,
            area_layouts: 0,
        }
    }

    /// Current counters and area count.
    pub fn stats(&self) -> ScreenStats {
        ScreenStats {
            areas: self.tree.area_count(),
            counters: ScreenCounters {
                builds: self.builds,
                layouts: self.layouts,
                area_resizes: self.area_resizes,
                area_layouts: self.area_layouts,
            },
        }
    }

    /// The splitters, as laid out. Empty until the first layout pass has run.
    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    /// The split tree the screen is placing areas from.
    ///
    /// Read-only on purpose: every change goes through an operation on the screen, so
    /// the widgets can follow it. This is what an application serialises (see
    /// [`Workspace`](crate::Workspace)).
    pub fn tree(&self) -> &SplitTree {
        &self.tree
    }

    /// The widget id of each area, by area id, skipping the ids that hold no area.
    ///
    /// The way a test or a benchmark reaches inside an area to read its own
    /// counters; the screen deliberately knows nothing about what an area contains.
    pub fn area_ids(&self) -> Vec<WidgetId> {
        self.pods.iter().flatten().map(|pod| pod.id()).collect()
    }

    /// The widget id of one area, if it has one.
    pub fn area_widget(&self, area: AreaId) -> Option<WidgetId> {
        self.pods.get(area).and_then(|pod| pod.as_ref()).map(|pod| pod.id())
    }

    /// Moves a splitter so that it sits under `pos`, in screen coordinates.
    ///
    /// The scripted form of a drag: what [`Widget::on_pointer_event`] does with a
    /// real pointer, exposed so the benchmark can do it without synthesising input.
    pub fn drag_bar(this: &mut WidgetMut<'_, Self>, split: NodeId, pos: Point) {
        if this.widget.move_bar(split, pos) {
            this.ctx.request_layout();
        }
    }

    /// Puts splitter `split` under `pos`. Returns whether anything moved.
    ///
    /// Both drag routes come through here — the scripted one above and the pointer
    /// handler below — because the two differ only in which context they ask for the
    /// relayout with, and a second copy of "find the bar, invert the layout, clamp" is
    /// a second place for the splitter to start lagging the pointer.
    fn move_bar(&mut self, split: NodeId, pos: Point) -> bool {
        let Some(bar) = self.bars.iter().find(|b| b.split == split).copied() else {
            return false;
        };
        self.tree.set_ratio(split, ratio_at(&bar, pos, BAR_THICKNESS))
    }

    // --- MARK: OPERATIONS

    /// Splits `area` in two, building a widget for the area that appears.
    ///
    /// Returns the new area's id, or `None` if `area` is not on the screen. The existing
    /// area keeps its id, its rectangle's first `ratio` and — the point — its widget:
    /// nothing about it is rebuilt.
    ///
    /// This is the operation the builder is kept for. Everything else here only ever
    /// moves or hides areas that already exist.
    pub fn split(this: &mut WidgetMut<'_, Self>, area: AreaId, axis: Axis, ratio: f64) -> Option<AreaId> {
        let fresh = this.widget.tree.split(area, axis, ratio)?;
        let widget = (this.widget.build)(fresh);
        if fresh >= this.widget.pods.len() {
            this.widget.pods.resize_with(fresh + 1, || None);
            this.widget.sizes.resize(fresh + 1, None);
        }
        this.widget.pods[fresh] = Some(widget.to_pod());
        // A reused id may carry the size its previous occupant was given, and a stale
        // one would swallow the first resize of the new area.
        this.widget.sizes[fresh] = None;
        this.widget.builds += 1;
        this.ctx.children_changed();
        this.ctx.request_layout();
        Some(fresh)
    }

    /// Merges two sibling areas, keeping `keep` and taking `dropped` off the screen.
    ///
    /// Returns whether anything happened; see [`SplitTree::join`] for when it does not.
    /// The survivor's widget is **not** rebuilt — it is the same widget, given a bigger
    /// rectangle — so its view, its selection and everything else it holds come through
    /// the operation untouched.
    pub fn join(this: &mut WidgetMut<'_, Self>, keep: AreaId, dropped: AreaId) -> bool {
        if !this.widget.tree.join(keep, dropped) {
            return false;
        }
        if let Some(pod) = this.widget.pods.get_mut(dropped).and_then(Option::take) {
            this.ctx.remove_child(pod);
        }
        this.widget.sizes[dropped] = None;
        this.ctx.children_changed();
        this.ctx.request_layout();
        true
    }

    /// Exchanges the places of two areas. Returns whether anything happened.
    ///
    /// Neither widget is touched: the tree moves the ids, and the ids are what the
    /// widgets are keyed by, so both editors arrive at the other rectangle whole.
    pub fn swap(this: &mut WidgetMut<'_, Self>, a: AreaId, b: AreaId) -> bool {
        if !this.widget.tree.swap(a, b) {
            return false;
        }
        this.ctx.request_layout();
        true
    }

    /// Shows one area alone. Returns whether anything happened.
    ///
    /// The others are stashed rather than removed, so they keep everything they hold and
    /// cost nothing while they are hidden; [`restore`](Self::restore) brings them back at
    /// the rectangles they had.
    pub fn maximize(this: &mut WidgetMut<'_, Self>, area: AreaId) -> bool {
        if !this.widget.tree.maximize(area) {
            return false;
        }
        this.ctx.request_layout();
        true
    }

    /// Shows the whole screen again. Returns whether anything happened.
    pub fn restore(this: &mut WidgetMut<'_, Self>) -> bool {
        if !this.widget.tree.restore() {
            return false;
        }
        this.ctx.request_layout();
        true
    }

    /// Replaces the tree, keeping every widget whose area is in both.
    ///
    /// What loading a workspace does. An area that is in the new tree and was in the old
    /// one keeps its widget — the id is the identity, and §30 already said what a rebuilt
    /// view costs — so a load that changes one splitter builds nothing at all.
    pub fn set_tree(this: &mut WidgetMut<'_, Self>, tree: SplitTree) {
        let slots = tree
            .areas()
            .map(|area| area + 1)
            .max()
            .unwrap_or(0)
            .max(this.widget.pods.len());
        this.widget.pods.resize_with(slots, || None);
        this.widget.sizes.resize(slots, None);

        for area in 0..slots {
            match (tree.holds(area), this.widget.pods[area].is_some()) {
                (false, true) => {
                    if let Some(pod) = this.widget.pods[area].take() {
                        this.ctx.remove_child(pod);
                    }
                    this.widget.sizes[area] = None;
                },
                (true, false) => {
                    let widget = (this.widget.build)(area);
                    this.widget.pods[area] = Some(widget.to_pod());
                    this.widget.sizes[area] = None;
                    this.widget.builds += 1;
                },
                _ => {},
            }
        }

        this.widget.tree = tree;
        this.ctx.children_changed();
        this.ctx.request_layout();
    }

    /// The area under a screen-space point, if the point is on one rather than on a
    /// splitter.
    ///
    /// Empty until the first layout pass has run, like [`bars`](Self::bars).
    pub fn area_at(&self, pos: Point) -> Option<AreaId> {
        self.rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(area, _)| *area)
    }

    /// The area the pointer is over, as of the last pointer event it saw.
    ///
    /// `None` before the pointer has moved over the screen, and while it is over a
    /// splitter rather than an area.
    pub fn hovered_area(&self) -> Option<AreaId> {
        self.hovered
    }

    /// The splitter under `pos`, if the pointer is close enough to grab one.
    fn bar_at(&self, pos: Point) -> Option<NodeId> {
        self.bars
            .iter()
            .find(|bar| bar.rect.inset(GRAB_SLOP).contains(pos))
            .map(|bar| bar.split)
    }
}

impl Widget for AreaScreen {
    type Action = NoAction;

    fn on_pointer_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &PointerEvent) {
        match event {
            PointerEvent::Down(e) if !ctx.is_handled() => {
                if e.button != Some(PointerButton::Primary) {
                    return;
                }
                // Areas are laid out over the whole screen and the bars sit in the
                // gaps between them, so a press that reaches here without being
                // handled is either on a bar or on an area that ignored it.
                let pos = ctx.local_position(e.state.position);
                if let Some(split) = self.bar_at(pos) {
                    self.drag = Some(split);
                    ctx.capture_pointer();
                    ctx.set_handled();
                }
            },
            PointerEvent::Move(PointerUpdate { current, .. }) => {
                let pos = ctx.local_position(current.position);
                // Recorded whether or not this widget acts on the event, and whether or
                // not something below already handled it: knowing where the pointer is
                // is not acting on it.
                self.hovered = self.area_at(pos);
                let Some(split) = self.drag else {
                    return;
                };
                if self.move_bar(split, pos) {
                    ctx.request_layout();
                }
                ctx.set_handled();
            },
            PointerEvent::Up(_) | PointerEvent::Cancel(_) if self.drag.take().is_some() => {
                ctx.release_pointer();
                ctx.set_handled();
            },
            PointerEvent::Leave(_) => self.hovered = None,
            _ => {},
        }
    }

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: masonry::kurbo::Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        // A screen fills its window. It never asks its areas how big they would
        // like to be: their size is a consequence of the split tree, not of their
        // contents, which is the whole difference between a screen and a flex box.
        match len_req {
            LenReq::MinContent => Length::ZERO,
            LenReq::MaxContent => match axis {
                masonry::kurbo::Axis::Horizontal => 1280.0.px(),
                masonry::kurbo::Axis::Vertical => 800.0.px(),
            },
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        self.layouts += 1;

        // Recomputing rects is a walk over a tree with tens of nodes and no
        // allocation; it is not what a frame costs. What a frame costs is which of
        // those rects came out different, because that is what `run_layout` will
        // refuse to early-return on.
        self.tree.layout(
            Rect::from_origin_size(Point::ORIGIN, size),
            BAR_THICKNESS,
            &mut self.rects,
            &mut self.bars,
        );

        // An area the tree did not place is hidden rather than absent — that is what
        // maximize is (§41.3). A stashed child is not laid out, not painted and not hit
        // tested, and Masonry excuses it from the "every child was laid out" check, so
        // this is the whole of hiding one.
        for (area, pod) in self.pods.iter_mut().enumerate() {
            let Some(pod) = pod else { continue };
            let placed = self.rects.iter().any(|(id, _)| *id == area);
            ctx.set_stashed(pod, !placed);
        }

        for i in 0..self.rects.len() {
            let (area, rect) = self.rects[i];
            let area_size = rect.size();
            if self.sizes[area] != Some(area_size) {
                self.sizes[area] = Some(area_size);
                self.area_resizes += 1;
            }
            let Some(pod) = self.pods[area].as_mut() else {
                continue;
            };
            if ctx.child_needs_layout(pod) {
                self.area_layouts += 1;
            }
            // The area gets the rect, not a size of its own choosing.
            let chosen = ctx.compute_size(pod, SizeDef::fixed(area_size), area_size.into());
            ctx.run_layout(pod, chosen);
            ctx.place_child(pod, rect.origin());
        }
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        // Bars only. Everything else on screen belongs to an area.
        for bar in &self.bars {
            painter.fill(bar.rect, Color::from_rgb8(0x18, 0x18, 0x1c)).draw();
        }
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        for pod in self.pods.iter_mut().flatten() {
            ctx.register_child(pod);
        }
    }

    fn children_ids(&self) -> ChildrenIds {
        self.pods.iter().flatten().map(|pod| pod.id()).collect()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

#[cfg(test)]
mod tests {
    use masonry::dpi::PhysicalSize;
    use masonry::kurbo::Axis;
    use masonry::testing::{ModularWidget, TestHarness};
    use masonry::theme::default_property_set;

    use super::*;

    const SCREEN: (u32, u32) = (1400, 900);

    /// An area that takes whatever rect it is given and does nothing with it.
    ///
    /// Deliberately inert. These tests are about the screen: a child with opinions
    /// about its own size would make it impossible to tell a screen that ignores the
    /// split tree from a child that overrode it.
    fn leaf() -> NewWidget<dyn Widget> {
        NewWidget::new(
            ModularWidget::new(()).measure_fn(|_, _, _, _, len_req, _| match len_req {
                LenReq::FitContent(space) => space,
                _ => Length::ZERO,
            }),
        )
        .erased()
    }

    fn harness(areas: usize) -> TestHarness<AreaScreen> {
        let screen = AreaScreen::new(SplitTree::balanced(areas), |_| leaf());
        let mut harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new(screen),
            PhysicalSize::new(SCREEN.0, SCREEN.1),
        );
        let _ = harness.redraw();
        harness
    }

    /// What the split tree says the areas should be, computed independently.
    fn expected(areas: usize) -> Vec<(AreaId, Rect)> {
        let (mut rects, mut bars) = (Vec::new(), Vec::new());
        SplitTree::balanced(areas).layout(
            Rect::from_origin_size(Point::ORIGIN, Size::new(SCREEN.0 as f64, SCREEN.1 as f64)),
            BAR_THICKNESS,
            &mut rects,
            &mut bars,
        );
        rects
    }

    /// The screen's whole job: an area is the size the tree says, not a size it chose.
    #[test]
    fn areas_are_laid_out_at_the_rects_the_tree_computed() {
        for count in [1, 2, 4, 8] {
            let harness = harness(count);
            let ids = harness.root_widget().area_ids();
            assert_eq!(ids.len(), count);

            for (area, rect) in expected(count) {
                let size = harness.get_widget_with_id(ids[area]).ctx().border_box().size();
                assert_eq!(size, rect.size(), "area {area} of {count}");
            }
        }
    }

    /// A screen with nothing happening to it must not be handing areas new sizes.
    #[test]
    fn an_idle_screen_resizes_nothing() {
        let mut harness = harness(8);
        let before = harness.root_widget().stats().counters.area_resizes;
        for _ in 0..5 {
            let _ = harness.redraw();
        }
        assert_eq!(harness.root_widget().stats().counters.area_resizes, before);
    }

    /// Claim 2, as a test rather than a timing: the two areas sharing a leaf splitter
    /// change size and nobody else does.
    #[test]
    fn dragging_a_leaf_splitter_resizes_two_areas() {
        let mut harness = harness(8);
        let bar = *harness
            .root_widget()
            .bars()
            .iter()
            .min_by(|a, b| {
                let span = |x: &Bar| x.span.width() * x.span.height();
                span(a).total_cmp(&span(b))
            })
            .expect("eight areas have splitters");

        let ids = harness.root_widget().area_ids();
        let sizes = |h: &TestHarness<AreaScreen>| -> Vec<Size> {
            ids.iter()
                .map(|id| h.get_widget_with_id(*id).ctx().border_box().size())
                .collect()
        };
        let before = sizes(&harness);
        let resizes_before = harness.root_widget().stats().counters.area_resizes;

        let step = match bar.axis {
            Axis::Horizontal => Point::new(bar.rect.center().x + 20.0, bar.rect.center().y),
            Axis::Vertical => Point::new(bar.rect.center().x, bar.rect.center().y + 20.0),
        };
        harness.edit_root_widget(|mut screen| AreaScreen::drag_bar(&mut screen, bar.split, step));
        let _ = harness.redraw();

        let after = sizes(&harness);
        let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
        assert_eq!(changed, 2, "before {before:?}\nafter  {after:?}");
        assert_eq!(
            harness.root_widget().stats().counters.area_resizes - resizes_before,
            2,
            "the screen's own counter must agree with the measured sizes"
        );
    }

    /// A splitter that stops where the pointer is not is a splitter that feels broken,
    /// and the drag path through the widget is not the one the tree's tests cover.
    #[test]
    fn a_drag_moves_the_bar_to_the_pointer() {
        let mut harness = harness(2);
        let bar = harness.root_widget().bars()[0];
        let target = Point::new(400.0, 450.0);

        harness.edit_root_widget(|mut screen| AreaScreen::drag_bar(&mut screen, bar.split, target));
        let _ = harness.redraw();

        let moved = harness.root_widget().bars()[0].rect.center().x;
        assert!((moved - target.x).abs() <= 1.0, "bar landed at {moved}");
    }

    /// The requirement, not the optimisation: an operation must not rebuild the widget
    /// of an area that survived it.
    ///
    /// Checked by widget id, because a rebuilt widget is a *new* widget however alike it
    /// looks — and what would be lost with the old one is the view, the selection and the
    /// materialised nodes §30 put in the model's way.
    #[test]
    fn no_operation_rebuilds_a_surviving_area() {
        let mut harness = harness(8);
        let before: Vec<Option<WidgetId>> = (0..8).map(|a| harness.root_widget().area_widget(a)).collect();
        let builds = harness.root_widget().stats().counters.builds;

        harness.edit_root_widget(|mut screen| {
            assert!(AreaScreen::join(&mut screen, 0, 1));
            assert!(AreaScreen::swap(&mut screen, 2, 3));
            assert!(AreaScreen::maximize(&mut screen, 4));
            assert!(AreaScreen::restore(&mut screen));
        });
        let _ = harness.redraw();

        assert_eq!(
            harness.root_widget().stats().counters.builds,
            builds,
            "nothing was built"
        );
        for area in [0, 2, 3, 4, 5, 6, 7] {
            assert_eq!(
                harness.root_widget().area_widget(area),
                before[area],
                "area {area} kept the widget it had"
            );
        }
        assert_eq!(harness.root_widget().area_widget(1), None, "the joined area is gone");
    }

    /// A join must move only what changed rectangle.
    #[test]
    fn joining_resizes_only_the_survivor() {
        let mut harness = harness(8);
        let resizes = harness.root_widget().stats().counters.area_resizes;

        harness.edit_root_widget(|mut screen| assert!(AreaScreen::join(&mut screen, 0, 1)));
        let _ = harness.redraw();

        assert_eq!(
            harness.root_widget().stats().counters.area_resizes - resizes,
            1,
            "area 0 took the pair's rectangle and nobody else moved"
        );
        assert_eq!(harness.root_widget().area_ids().len(), 7);
    }

    /// A split is the one operation that builds, and it builds exactly one child.
    #[test]
    fn splitting_builds_one_child_and_leaves_the_rest() {
        let mut harness = harness(4);
        let before = harness.root_widget().area_widget(0);
        let builds = harness.root_widget().stats().counters.builds;

        let fresh = harness
            .edit_root_widget(|mut screen| AreaScreen::split(&mut screen, 0, Axis::Vertical, 0.5))
            .expect("area 0 exists");
        let _ = harness.redraw();

        assert_eq!(harness.root_widget().stats().counters.builds - builds, 1);
        assert_eq!(harness.root_widget().area_widget(0), before, "area 0 was not rebuilt");
        assert!(harness.root_widget().area_widget(fresh).is_some());
        assert_eq!(harness.root_widget().area_ids().len(), 5);
    }

    /// Maximize hides the rest of the screen rather than removing it, and restore gives
    /// back exactly the sizes that were there.
    #[test]
    fn maximize_hides_the_others_and_restore_gives_the_sizes_back() {
        let mut harness = harness(8);
        let ids = harness.root_widget().area_ids();
        let sizes = |h: &TestHarness<AreaScreen>| -> Vec<Size> {
            ids.iter()
                .map(|id| h.get_widget_with_id(*id).ctx().border_box().size())
                .collect()
        };
        let before = sizes(&harness);

        harness.edit_root_widget(|mut screen| assert!(AreaScreen::maximize(&mut screen, 3)));
        let _ = harness.redraw();
        assert_eq!(
            harness.root_widget().area_ids().len(),
            8,
            "the hidden areas are still children"
        );
        let maximized = harness.get_widget_with_id(ids[3]).ctx().border_box().size();
        assert_eq!(maximized, Size::new(f64::from(SCREEN.0), f64::from(SCREEN.1)));

        harness.edit_root_widget(|mut screen| assert!(AreaScreen::restore(&mut screen)));
        let _ = harness.redraw();
        assert_eq!(sizes(&harness), before, "bit for bit, not approximately");
    }

    /// Swapping two areas exchanges their rectangles and touches nothing else.
    #[test]
    fn swapping_exchanges_two_rectangles() {
        let mut harness = harness(4);
        let ids = harness.root_widget().area_ids();
        let size_of = |h: &TestHarness<AreaScreen>, at: usize| h.get_widget_with_id(ids[at]).ctx().border_box().size();
        let boxes: Vec<Size> = (0..4).map(|a| size_of(&harness, a)).collect();

        harness.edit_root_widget(|mut screen| assert!(AreaScreen::swap(&mut screen, 0, 3)));
        let _ = harness.redraw();

        assert_eq!(size_of(&harness, 0), boxes[3], "area 0's widget went to 3's rectangle");
        assert_eq!(size_of(&harness, 3), boxes[0]);
        assert_eq!(size_of(&harness, 1), boxes[1], "the others stayed put");
    }

    /// Loading a workspace builds only what was not already there.
    ///
    /// The whole reason an id is the identity: a layout that comes back from a file is
    /// the same areas in different rectangles, and rebuilding them would throw away
    /// everything they hold to achieve exactly that.
    #[test]
    fn loading_a_tree_keeps_the_areas_both_trees_hold() {
        let mut harness = harness(4);
        let before: Vec<Option<WidgetId>> = (0..4).map(|a| harness.root_widget().area_widget(a)).collect();
        let builds = harness.root_widget().stats().counters.builds;

        // The same four areas, tiled differently: nothing to build, nothing to drop.
        let same = SplitTree::balanced(4);
        harness.edit_root_widget(|mut screen| AreaScreen::set_tree(&mut screen, same));
        let _ = harness.redraw();
        assert_eq!(
            harness.root_widget().stats().counters.builds,
            builds,
            "nothing was built"
        );
        for (area, was) in before.iter().enumerate() {
            assert_eq!(harness.root_widget().area_widget(area), *was);
        }

        // A smaller screen: the areas that went are dropped, the rest are not rebuilt.
        harness.edit_root_widget(|mut screen| AreaScreen::set_tree(&mut screen, SplitTree::balanced(2)));
        let _ = harness.redraw();
        assert_eq!(harness.root_widget().stats().counters.builds, builds);
        assert_eq!(harness.root_widget().area_ids().len(), 2);
        assert_eq!(harness.root_widget().area_widget(0), before[0]);
        assert_eq!(harness.root_widget().area_widget(3), None);

        // And back up: only the areas that were not there are built.
        harness.edit_root_widget(|mut screen| AreaScreen::set_tree(&mut screen, SplitTree::balanced(4)));
        let _ = harness.redraw();
        assert_eq!(
            harness.root_widget().stats().counters.builds - builds,
            2,
            "areas 2 and 3 came back, and only those"
        );
    }

    /// A workspace that has been through a file puts every widget back in its own area.
    #[test]
    fn a_workspace_round_trip_puts_every_area_back() {
        let mut harness = harness(8);
        harness.edit_root_widget(|mut screen| {
            assert!(AreaScreen::join(&mut screen, 0, 1));
            assert!(AreaScreen::swap(&mut screen, 2, 5));
        });
        let _ = harness.redraw();

        let ids = harness.root_widget().area_ids();
        let sizes = |h: &TestHarness<AreaScreen>| -> Vec<Size> {
            ids.iter()
                .map(|id| h.get_widget_with_id(*id).ctx().border_box().size())
                .collect()
        };
        let before = sizes(&harness);
        let builds = harness.root_widget().stats().counters.builds;

        let text = crate::Workspace::new(harness.root_widget().tree().clone()).write();
        let read = crate::Workspace::parse(&text).expect("what we wrote reads back");
        harness.edit_root_widget(|mut screen| AreaScreen::set_tree(&mut screen, read.tree().clone()));
        let _ = harness.redraw();

        assert_eq!(sizes(&harness), before, "every area is where it was");
        assert_eq!(harness.root_widget().area_ids(), ids, "and it is the same widget");
        assert_eq!(harness.root_widget().stats().counters.builds, builds);
    }

    /// The screen has to know where the pointer is, because the driver above it cannot.
    #[test]
    fn the_screen_records_the_area_under_the_pointer() {
        let mut harness = harness(4);
        assert_eq!(harness.root_widget().hovered_area(), None, "before the pointer moved");

        for area in 0..4 {
            let rect = expected(4)[area].1;
            harness.mouse_move(rect.center());
            assert_eq!(harness.root_widget().hovered_area(), Some(area));
        }
    }

    /// A screen of one area still tiles, and has no splitter to grab.
    #[test]
    fn a_single_area_screen_has_no_splitters() {
        let harness = harness(1);
        assert!(harness.root_widget().bars().is_empty());
        assert_eq!(harness.root_widget().area_ids().len(), 1);
    }
}
