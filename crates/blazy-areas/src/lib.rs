//! Blender-style screen areas for Masonry: one widget tree tiled by a split tree.
//!
//! This crate answers the second half of the Phase 0 question. Phase 0 established
//! that the cost of a frame is the cost of walking the widget tree, and that the
//! tree is *per window* (`rnd/architecture.md` §20.2). A node canvas escaped that
//! by virtualising — but a Blender screen puts six or eight editors in the same
//! window at once, and nothing measured so far says whether their costs add up or
//! whether a splitter drag relays out everything on screen.
//!
//! Three claims are under test:
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
//! # Structure
//!
//! [`SplitTree`] is pure geometry and knows nothing of widgets; [`AreaScreen`] is
//! the Masonry widget that owns one child per area and places it at the rect the
//! tree computed. §8 asks for exactly this seam, so the tree can later be
//! serialised, or replaced by a vertex-and-edge graph, without touching the widget.
//!
//! # What this is not
//!
//! Not the finished subsystem. There is no join, no maximize/restore, no swap, no
//! detach into a second OS window, no region model inside an area (header, toolbar,
//! sidebar) and no per-region `ui_scale`. Those are features; this crate exists to
//! find out whether the shape they would be built on is sound.

mod tree;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, EventCtx, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PointerEvent,
    PropertiesMut, PropertiesRef, RegisterCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Point, Rect, Size};
use masonry::layout::{AsUnit, LenReq, Length, SizeDef};
use masonry::peniko::Color;
use masonry::ui_events::pointer::{PointerButton, PointerUpdate};

pub use crate::tree::{AreaId, Bar, NodeId, SplitTree, ratio_at};

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
pub struct ScreenStats {
    /// Areas the screen currently holds.
    pub areas: usize,
    pub counters: ScreenCounters,
}

/// Cumulative counters, for spotting work that should not be happening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenCounters {
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
    /// One child per area, indexed by [`AreaId`].
    pods: Vec<WidgetPod<dyn Widget>>,
    /// The border-box size each area was last given, for counting real resizes.
    sizes: Vec<Option<Size>>,
    /// Where each area goes, recomputed every layout. Reused, so a resize allocates
    /// nothing.
    rects: Vec<(AreaId, Rect)>,
    /// Where each splitter goes. Also the hit-test source for a drag.
    bars: Vec<Bar>,
    /// The splitter currently being dragged, if any.
    drag: Option<NodeId>,
    layouts: u64,
    area_resizes: u64,
    area_layouts: u64,
}

impl AreaScreen {
    /// Builds a screen over `tree`, calling `build` once per area.
    ///
    /// Every area is materialised up front, unlike the canvas's nodes: an area is
    /// on screen by definition, and there are tens of them rather than thousands.
    pub fn new(tree: SplitTree, mut build: impl FnMut(AreaId) -> NewWidget<dyn Widget>) -> Self {
        let count = tree.area_count();
        Self {
            tree,
            pods: (0..count).map(|area| build(area).to_pod()).collect(),
            sizes: vec![None; count],
            rects: Vec::with_capacity(count),
            bars: Vec::with_capacity(count.saturating_sub(1)),
            drag: None,
            layouts: 0,
            area_resizes: 0,
            area_layouts: 0,
        }
    }

    /// Current counters and area count.
    pub fn stats(&self) -> ScreenStats {
        ScreenStats {
            areas: self.tree.area_count(),
            counters: ScreenCounters {
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

    /// The widget id of each area, in area order.
    ///
    /// The way a test or a benchmark reaches inside an area to read its own
    /// counters; the screen deliberately knows nothing about what an area contains.
    pub fn area_ids(&self) -> Vec<WidgetId> {
        self.pods.iter().map(|pod| pod.id()).collect()
    }

    /// Moves a splitter so that it sits under `pos`, in screen coordinates.
    ///
    /// The scripted form of a drag: what [`Widget::on_pointer_event`] does with a
    /// real pointer, exposed so the benchmark can do it without synthesising input.
    pub fn drag_bar(this: &mut WidgetMut<'_, Self>, split: NodeId, pos: Point) {
        let Some(bar) = this.widget.bars.iter().find(|b| b.split == split).copied() else {
            return;
        };
        if this.widget.tree.set_ratio(split, ratio_at(&bar, pos, BAR_THICKNESS)) {
            this.ctx.request_layout();
        }
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
                let Some(split) = self.drag else {
                    return;
                };
                let pos = ctx.local_position(current.position);
                let Some(bar) = self.bars.iter().find(|b| b.split == split).copied() else {
                    return;
                };
                if self.tree.set_ratio(split, ratio_at(&bar, pos, BAR_THICKNESS)) {
                    ctx.request_layout();
                }
                ctx.set_handled();
            },
            PointerEvent::Up(_) | PointerEvent::Cancel(_) if self.drag.take().is_some() => {
                ctx.release_pointer();
                ctx.set_handled();
            },
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

        for i in 0..self.rects.len() {
            let (area, rect) = self.rects[i];
            let area_size = rect.size();
            if self.sizes[area] != Some(area_size) {
                self.sizes[area] = Some(area_size);
                self.area_resizes += 1;
            }
            let pod = &mut self.pods[area];
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
        for pod in &mut self.pods {
            ctx.register_child(pod);
        }
    }

    fn children_ids(&self) -> ChildrenIds {
        self.pods.iter().map(|pod| pod.id()).collect()
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

    /// A screen of one area still tiles, and has no splitter to grab.
    #[test]
    fn a_single_area_screen_has_no_splitters() {
        let harness = harness(1);
        assert!(harness.root_widget().bars().is_empty());
        assert_eq!(harness.root_widget().area_ids().len(), 1);
    }
}
