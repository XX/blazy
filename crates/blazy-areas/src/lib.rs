//! Blender-style screen areas for Masonry: one widget tree tiled by a split tree.
//!
//! * **Areas do not add up:** splitting divides the same viewport, so the widget count stays flat as areas multiply.
//! * **A splitter drag disturbs its two neighbours**, not the screen (the root splitter moves half of it, which is
//!   tiling).
//! * **An idle screen is idle:** computing rects is not laying areas out.
//! * **`ui_scale` is a local layout input; `view` is not a layout input at all.**
//!
//! [`SplitTree`] is pure geometry; [`AreaScreen`] places one child per area at the rect the
//! tree computed. [`AreaContent`] fills an area with regions (header, main view, sidebar),
//! each with its own [`UiScale`]. [`Workspace`] writes the tree, and what fills each area, to
//! a file.
//!
//! Split, join, swap, maximize and restore keep the widget of every area that survives: an
//! [`AreaId`] is never renumbered. Join merges siblings only — a binary tree's limit, where
//! Blender merges any two areas sharing a border.
//!
//! Masonry's own widgets do not read [`UiScale`] (their sizes come from the theme);
//! `blazy-widgets` carries it into a subtree of them.

#![warn(missing_docs, unreachable_pub)]

mod region;
mod tree;
mod workspace;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, CursorIcon, EventCtx, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PointerEvent,
    PropertiesMut, PropertiesRef, QueryCtx, RegisterCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Rect, Size};
use masonry::layout::{AsUnit, LenReq, Length, SizeDef};
use masonry::peniko::Color;
use masonry::ui_events::pointer::{PointerButton, PointerUpdate};

pub use crate::region::{
    AreaContent, CarriesScale, RegionCounters, RegionKind, SIDEBAR_GRIP, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
    ScaleCarrier, Sidebar, SidebarState, UiScale, push_ui_scale,
};
pub use crate::tree::{AreaId, Bar, MAX_AREAS, NodeId, SplitTree, ratio_at};
pub use crate::workspace::{Workspace, WorkspaceError};

/// Thickness of a splitter, in logical pixels.
const BAR_THICKNESS: f64 = 4.0;

/// How far either side of a splitter still counts as grabbing it (the bar is thin).
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
    /// Area widgets built, one per area that appeared: only a split and the first build
    /// raise it.
    pub builds: u64,
    /// Layout passes run on the screen itself.
    pub layouts: u64,
    /// Areas handed a border-box size different from their last one, summed over all
    /// passes: what a splitter drag costs.
    pub area_resizes: u64,
    /// Areas Masonry considered dirty on entry to the screen's layout; meaningful only
    /// without debug assertions.
    pub area_layouts: u64,
}

/// Makes what an area is: its widget and what the application keeps beside it, at once.
type AreaBuilder<P> = dyn FnMut(AreaId) -> (NewWidget<dyn Widget>, P);

/// A window tiled into areas, each laid out at exactly the rect the split tree computed.
pub struct AreaScreen<P = ()> {
    tree: SplitTree,
    /// Builds the widget for an area that appears (kept for splits).
    build: Box<AreaBuilder<P>>,
    /// One child per area, indexed by [`AreaId`]; `None` where an area was joined away.
    pods: Vec<Option<WidgetPod<dyn Widget>>>,
    /// What the application keeps beside each area's widget, by the same key and with
    /// the same tombstones.
    ///
    /// The place for state that has to outlive the widget — a view, a selection, an undo
    /// history — because a widget does not survive being rebuilt in another window and
    /// that state must (the detach task, decision 1a). Keyed by [`AreaId`], so it follows
    /// the area through a swap and goes with it through a join without anything else
    /// being written: the id is what everything here is keyed by.
    payloads: Vec<Option<P>>,
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
    /// the same reason the canvas publishes what it picked. An operation on
    /// "the area under the pointer" needs this and nothing else.
    hovered: Option<AreaId>,
    layouts: u64,
    builds: u64,
    area_resizes: u64,
    area_layouts: u64,
}

impl AreaScreen<()> {
    /// Builds a screen over `tree`, calling `build` once per area.
    ///
    /// Every area is materialised up front, unlike the canvas's nodes: an area is
    /// on screen by definition, and there are tens of them rather than thousands.
    pub fn new(tree: SplitTree, mut build: impl FnMut(AreaId) -> NewWidget<dyn Widget> + 'static) -> Self {
        Self::with_payloads(tree, move |area| (build(area), ()))
    }
}

impl<P: 'static> AreaScreen<P> {
    /// Builds a screen whose areas carry something of the application's beside them, made
    /// together with each widget.
    pub fn with_payloads(
        tree: SplitTree,
        mut build: impl FnMut(AreaId) -> (NewWidget<dyn Widget>, P) + 'static,
    ) -> Self {
        // By area id: ids need not be dense.
        let slots = tree.areas().map(|area| area + 1).max().unwrap_or(0);
        let mut pods: Vec<Option<WidgetPod<dyn Widget>>> = (0..slots).map(|_| None).collect();
        let mut payloads: Vec<Option<P>> = (0..slots).map(|_| None).collect();
        let mut builds = 0;
        for area in tree.areas() {
            let (widget, payload) = build(area);
            pods[area] = Some(widget.to_pod());
            payloads[area] = Some(payload);
            builds += 1;
        }
        let count = tree.area_count();
        Self {
            tree,
            build: Box::new(build),
            pods,
            payloads,
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

    /// The split tree the screen is placing areas from; change it through the screen's
    /// operations.
    pub fn tree(&self) -> &SplitTree {
        &self.tree
    }

    /// The widget id of each area, by area id, skipping the ids that hold no area.
    pub fn area_ids(&self) -> Vec<WidgetId> {
        self.pods.iter().flatten().map(|pod| pod.id()).collect()
    }

    /// The widget id of one area, if it has one.
    pub fn area_widget(&self, area: AreaId) -> Option<WidgetId> {
        self.pods.get(area).and_then(|pod| pod.as_ref()).map(|pod| pod.id())
    }

    /// Moves a splitter so that it sits under `pos`, in screen coordinates: a scripted drag.
    pub fn drag_bar(this: &mut WidgetMut<'_, Self>, split: NodeId, pos: Point) {
        if this.widget.move_bar(split, pos) {
            this.ctx.request_layout();
        }
    }

    /// Puts splitter `split` under `pos`. Returns whether anything moved.
    fn move_bar(&mut self, split: NodeId, pos: Point) -> bool {
        let Some(bar) = self.bars.iter().find(|b| b.split == split).copied() else {
            return false;
        };
        self.tree.set_ratio(split, ratio_at(&bar, pos, BAR_THICKNESS))
    }

    // --- MARK: OPERATIONS

    /// Splits `area` in two, building a widget for the area that appears; the existing area
    /// keeps its widget and the first `ratio`. Returns the new id, or `None` if `area` is not
    /// on the screen or the screen holds [`MAX_AREAS`].
    pub fn split(this: &mut WidgetMut<'_, Self>, area: AreaId, axis: Axis, ratio: f64) -> Option<AreaId> {
        let fresh = this.widget.tree.split(area, axis, ratio)?;
        let (widget, payload) = (this.widget.build)(fresh);
        if fresh >= this.widget.pods.len() {
            this.widget.pods.resize_with(fresh + 1, || None);
            this.widget.payloads.resize_with(fresh + 1, || None);
            this.widget.sizes.resize(fresh + 1, None);
        }
        this.widget.pods[fresh] = Some(widget.to_pod());
        this.widget.payloads[fresh] = Some(payload);
        // A reused id may carry its previous occupant's size.
        this.widget.sizes[fresh] = None;
        this.widget.builds += 1;
        this.ctx.children_changed();
        this.ctx.request_layout();
        Some(fresh)
    }

    /// Merges two sibling areas, keeping `keep` (its widget too) and dropping `dropped`.
    /// Returns whether anything happened.
    pub fn join(this: &mut WidgetMut<'_, Self>, keep: AreaId, dropped: AreaId) -> bool {
        if !this.widget.tree.join(keep, dropped) {
            return false;
        }
        if let Some(pod) = this.widget.pods.get_mut(dropped).and_then(Option::take) {
            this.ctx.remove_child(pod);
        }
        // And what the application kept beside it.
        this.widget.payloads[dropped] = None;
        this.widget.sizes[dropped] = None;
        this.ctx.children_changed();
        this.ctx.request_layout();
        true
    }

    /// Takes an area off this screen and returns what it carried, for detach: the widget is
    /// dropped (a tree cannot move between windows), the payload is the caller's to rebuild
    /// elsewhere. `None` for the last area. The space goes to its sibling, of any shape.
    pub fn detach(this: &mut WidgetMut<'_, Self>, area: AreaId) -> Option<P> {
        if !this.widget.tree.remove(area) {
            return None;
        }
        let payload = this.widget.payloads.get_mut(area).and_then(Option::take);
        if let Some(pod) = this.widget.pods.get_mut(area).and_then(Option::take) {
            this.ctx.remove_child(pod);
        }
        this.widget.sizes[area] = None;
        this.ctx.children_changed();
        this.ctx.request_layout();
        payload
    }

    /// Exchanges the places of two areas, widgets untouched. Returns whether anything
    /// happened.
    pub fn swap(this: &mut WidgetMut<'_, Self>, a: AreaId, b: AreaId) -> bool {
        if !this.widget.tree.swap(a, b) {
            return false;
        }
        this.ctx.request_layout();
        true
    }

    /// Shows one area alone, the others stashed. Returns whether anything happened.
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

    /// Replaces the tree (a workspace load), keeping every widget whose area is in both.
    pub fn set_tree(this: &mut WidgetMut<'_, Self>, tree: SplitTree) {
        let slots = tree
            .areas()
            .map(|area| area + 1)
            .max()
            .unwrap_or(0)
            .max(this.widget.pods.len());
        this.widget.pods.resize_with(slots, || None);
        this.widget.payloads.resize_with(slots, || None);
        this.widget.sizes.resize(slots, None);

        for area in 0..slots {
            match (tree.holds(area), this.widget.pods[area].is_some()) {
                (false, true) => {
                    if let Some(pod) = this.widget.pods[area].take() {
                        this.ctx.remove_child(pod);
                    }
                    this.widget.payloads[area] = None;
                    this.widget.sizes[area] = None;
                },
                (true, false) => {
                    let (widget, payload) = (this.widget.build)(area);
                    this.widget.pods[area] = Some(widget.to_pod());
                    this.widget.payloads[area] = Some(payload);
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

    /// What the application keeps beside an area, if anything.
    pub fn payload(&self, area: AreaId) -> Option<&P> {
        self.payloads.get(area).and_then(Option::as_ref)
    }

    /// Takes it away, leaving the area without one.
    pub fn take_payload(this: &mut WidgetMut<'_, Self>, area: AreaId) -> Option<P> {
        this.widget.payloads.get_mut(area).and_then(Option::take)
    }

    /// Puts one there, replacing whatever was.
    pub fn set_payload(this: &mut WidgetMut<'_, Self>, area: AreaId, payload: P) {
        if area >= this.widget.payloads.len() {
            this.widget.payloads.resize_with(area + 1, || None);
        }
        this.widget.payloads[area] = Some(payload);
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

    /// The area the pointer is over, as of the last pointer event; `None` over a splitter.
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

impl<P: 'static> Widget for AreaScreen<P> {
    type Action = NoAction;

    fn on_pointer_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &PointerEvent) {
        match event {
            PointerEvent::Down(e) if !ctx.is_handled() => {
                if e.button != Some(PointerButton::Primary) {
                    return;
                }
                // An unhandled press is on a bar or on an area that ignored it.
                let pos = ctx.local_position(e.state.position);
                if let Some(split) = self.bar_at(pos) {
                    self.drag = Some(split);
                    ctx.capture_pointer();
                    ctx.set_handled();
                }
            },
            PointerEvent::Move(PointerUpdate { current, .. }) => {
                let pos = ctx.local_position(current.position);
                // Recorded whether or not anything handled the event.
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

    /// Over a splitter, and while one is dragged, the resize cursor for its axis.
    fn get_cursor(&self, _ctx: &QueryCtx<'_>, pos: Point) -> CursorIcon {
        let split = self.drag.or_else(|| self.bar_at(pos));
        match split.and_then(|split| self.bars.iter().find(|bar| bar.split == split)) {
            Some(bar) if bar.axis == Axis::Horizontal => CursorIcon::EwResize,
            Some(_) => CursorIcon::NsResize,
            None => CursorIcon::Default,
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
        // A screen fills its window; areas never say how big they would like to be.
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

        // Cheap; what costs is the rects that changed.
        self.tree.layout(
            Rect::from_origin_size(Point::ORIGIN, size),
            BAR_THICKNESS,
            &mut self.rects,
            &mut self.bars,
        );

        // An area the tree did not place (maximize) is stashed.
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
    use std::rc::Rc;

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

    /// A screen whose areas carry something of the application's: a counted handle, so
    /// the test can see when it is dropped.
    fn payload_harness(areas: usize) -> (TestHarness<AreaScreen<Rc<()>>>, Rc<()>) {
        let token = Rc::new(());
        let handed = token.clone();
        let screen = AreaScreen::with_payloads(SplitTree::balanced(areas), move |_| (leaf(), handed.clone()));
        let mut harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new(screen),
            PhysicalSize::new(SCREEN.0, SCREEN.1),
        );
        let _ = harness.redraw();
        (harness, token)
    }

    /// Over a splitter the cursor says which way it moves — left and right between areas
    /// side by side, up and down between areas one above the other — and stays so for the
    /// whole drag; over an area it is the area's.
    #[test]
    fn a_splitter_under_the_pointer_shows_which_way_it_moves() {
        use masonry::core::CursorIcon;

        let mut harness = harness(3);
        let bars: Vec<Bar> = harness.root_widget().bars().to_vec();
        let side = bars
            .iter()
            .find(|bar| bar.axis == Axis::Horizontal)
            .expect("a side-by-side split");
        let stacked = bars
            .iter()
            .find(|bar| bar.axis == Axis::Vertical)
            .expect("a stacked split");

        harness.mouse_move(side.rect.center());
        assert_eq!(
            harness.cursor_icon(),
            CursorIcon::EwResize,
            "between areas side by side"
        );
        harness.mouse_move(stacked.rect.center());
        assert_eq!(
            harness.cursor_icon(),
            CursorIcon::NsResize,
            "between areas one above the other"
        );

        harness.mouse_move(side.rect.center());
        harness.mouse_button_press(Some(PointerButton::Primary));
        harness.mouse_move(side.rect.center() + masonry::kurbo::Vec2::new(-40.0, 0.0));
        assert_eq!(harness.cursor_icon(), CursorIcon::EwResize, "the whole drag");
        harness.mouse_button_release(Some(PointerButton::Primary));

        harness.mouse_move(Point::new(5.0, 5.0));
        assert_eq!(harness.cursor_icon(), CursorIcon::Default, "over an area");
    }

    /// Decision 1a: what an area carries beside its widget lives and dies with the area.
    #[test]
    fn a_joined_area_takes_its_payload_with_it() {
        let (mut harness, token) = payload_harness(4);
        // One per area, one the test holds, and one the builder keeps so that a split
        // can make another area later.
        assert_eq!(Rc::strong_count(&token), 6);

        harness.edit_root_widget(|mut screen| {
            let sibling = screen
                .widget
                .tree()
                .joinable(0)
                .expect("a balanced screen has siblings");
            assert!(AreaScreen::join(&mut screen, 0, sibling));
            assert!(screen.widget.payload(sibling).is_none(), "the area that went took it");
            assert!(screen.widget.payload(0).is_some(), "the one that stayed kept it");
        });
        let _ = harness.redraw();

        assert_eq!(Rc::strong_count(&token), 5, "and the handle the area held is gone");
    }

    /// And a payload can be taken out and put back, which is all detach needs from it.
    #[test]
    fn a_payload_can_be_taken_and_given() {
        let (mut harness, _token) = payload_harness(2);
        harness.edit_root_widget(|mut screen| {
            let taken = AreaScreen::take_payload(&mut screen, 1).expect("area 1 has one");
            assert!(screen.widget.payload(1).is_none(), "and now it does not");
            AreaScreen::set_payload(&mut screen, 1, taken);
            assert!(screen.widget.payload(1).is_some(), "until it is given back");
        });
    }

    /// Detach: the area goes, its state comes back, and nothing else is rebuilt.
    #[test]
    fn detaching_an_area_hands_back_its_payload_and_rebuilds_nothing() {
        let (mut harness, token) = payload_harness(4);
        let before = harness.root_widget().stats().counters.builds;

        let taken = harness.edit_root_widget(|mut screen| AreaScreen::detach(&mut screen, 1));
        let _ = harness.redraw();

        assert!(taken.is_some(), "the area's state came out with it");
        assert_eq!(harness.root_widget().stats().areas, 3, "and the area is off the screen");
        assert_eq!(
            harness.root_widget().stats().counters.builds,
            before,
            "the areas that stayed kept their widgets"
        );
        assert_eq!(Rc::strong_count(&token), 6, "nothing was cloned and nothing dropped");
        drop(taken);
        assert_eq!(Rc::strong_count(&token), 5);
    }

    /// Decision 5: the last area of a screen does not detach.
    #[test]
    fn the_last_area_does_not_detach() {
        let (mut harness, _token) = payload_harness(1);
        let taken = harness.edit_root_widget(|mut screen| AreaScreen::detach(&mut screen, 0));
        assert!(taken.is_none(), "nothing came out");
        assert_eq!(harness.root_widget().stats().areas, 1, "and nothing went");
        assert!(
            harness.root_widget().payload(0).is_some(),
            "the area still has its state"
        );
    }

    /// The space goes to the sibling subtree, which is what makes detach work where join
    /// refuses.
    #[test]
    fn detach_works_where_join_has_no_partner() {
        let (mut harness, _token) = payload_harness(4);
        harness.edit_root_widget(|mut screen| {
            AreaScreen::split(&mut screen, 1, Axis::Horizontal, 0.5);
        });
        let _ = harness.redraw();

        let areas = harness.root_widget().stats().areas;
        harness.edit_root_widget(|mut screen| {
            let joinable = screen.widget.tree().joinable(0);
            let taken = AreaScreen::detach(&mut screen, 0);
            assert!(taken.is_some(), "detach took it anyway: {joinable:?}");
        });
        let _ = harness.redraw();
        assert_eq!(harness.root_widget().stats().areas, areas - 1);
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
    /// materialised nodes.
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
