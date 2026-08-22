//! Regions inside an area, and the per-region `ui_scale` of `rnd/architecture.md` §9.
//!
//! An area is not one surface. Blender's editors carry a header, sometimes a toolbar
//! and a sidebar, around the main view, and §9 gives each of those its own
//! `ui_scale`: the user's idea of how big the interface should be, which is *not* the
//! same knob as the window's HiDPI factor and *not* the same knob as panning and
//! zooming the content.
//!
//! The distinction §9 insists on, and the one this module exists to keep:
//!
//! * `ui_scale` goes into **layout** — type size, padding, control sizes. Changing it is supposed to cost a re-layout
//!   of the region, and only of the region.
//! * `view` (pan/zoom of the content) goes into **paint** and nowhere else. Changing it must cost no layout at all.
//!
//! Mixing them means re-running layout on every frame of a zoom, which is the mistake
//! that kills node editors. Phase 0 established that the canvas keeps `view` out of
//! layout; this module is where `ui_scale` gets put *into* it without dragging `view`
//! along.
//!
//! # How the value travels
//!
//! Masonry has no inherited properties. [`UiScale`] is set on the region's root
//! widget with `WidgetMut::insert_prop`, which fires `Widget::property_changed`, and
//! a widget that cares asks for a re-layout there. A container that wants its
//! children scaled has to forward the value; nothing does it automatically. That is
//! a finding, not a design choice — see `rnd/architecture.md` §22.

use std::any::TypeId;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PropertiesRef, Property, RegisterCtx,
    UpdateCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Size};
use masonry::layout::{LenReq, Length, SizeDef};

/// What a region is for.
///
/// Two of the five §8 lists. A header and a main view are enough to ask every
/// question this spike asks; a toolbar and a sidebar would only be more of the same
/// arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionKind {
    /// A strip of fixed logical height across the top, scaled by its `ui_scale`.
    Header,
    /// Whatever is left. The editor proper.
    Main,
}

/// The interface scale of a region, as a property on its root widget.
///
/// A multiplier on logical lengths, not on the render target: 1.0 is the size the
/// application was designed at. Read it in `measure` and `layout` and multiply the
/// lengths you would otherwise have used; do **not** read it in `paint` to scale a
/// transform, which is what `view` is for.
///
/// Widgets that ignore it simply do not scale, which is why the built-in Masonry
/// widgets do not: nothing in `masonry::widgets` reads it, and their sizes come from
/// the theme's `DefaultProperties`, which is one per application rather than one per
/// region (§22).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiScale(pub f64);

impl Property for UiScale {
    fn static_default() -> &'static Self {
        static DEFAULT: UiScale = UiScale(1.0);
        &DEFAULT
    }
}

impl Default for UiScale {
    fn default() -> Self {
        *Self::static_default()
    }
}

impl UiScale {
    /// Call from `Widget::property_changed` to re-lay-out when the scale moves.
    ///
    /// A scale change is a layout change by definition — that is the whole content of
    /// §9's first rule — so a widget that reads [`UiScale`] must ask for layout, not
    /// merely for paint.
    pub fn prop_changed(ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        if property_type == TypeId::of::<Self>() {
            ctx.request_layout();
        }
    }
}

/// Cumulative counters, for spotting work that should not be happening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionCounters {
    /// Layout passes run on the area's region stack.
    pub layouts: u64,
    /// Regions handed a border-box size different from their last one.
    pub resizes: u64,
    /// Times a new [`UiScale`] was pushed down to a region's root.
    ///
    /// Pushed in the mutate pass, because setting a property needs a `WidgetMut`.
    ///
    /// Whether the root *acted* on it is deliberately not counted here: from the
    /// mutate pass there is no honest way to ask, and guessing would be worse than
    /// not answering. The region's own content knows, and that is where the
    /// measurement belongs — see `area-screen`'s header widget.
    pub scale_pushes: u64,
}

/// One region's slot.
struct Slot {
    kind: RegionKind,
    pod: WidgetPod<dyn Widget>,
    /// Height of a [`RegionKind::Header`] at scale 1.0. Ignored for `Main`.
    base_height: f64,
    ui_scale: f64,
    /// The scale the root was last told about, if it has been told.
    pushed_scale: Option<f64>,
    /// Border-box size the region was last given.
    size: Option<Size>,
}

/// The stack of regions filling one area.
///
/// Goes inside an [`AreaScreen`](crate::AreaScreen) area: the screen decides how big
/// an area is, this decides how the area is divided between its regions.
///
/// Regions are stacked along the vertical axis, header first. §8's model has regions
/// on any edge; a spike needs one axis to ask its questions and gains nothing from
/// two.
pub struct AreaContent {
    slots: Vec<Slot>,
    /// Regions whose root needs a new [`UiScale`], applied in the mutate pass.
    pending: Vec<usize>,
    layouts: u64,
    resizes: u64,
    scale_pushes: u64,
}

impl AreaContent {
    /// Builds an area from its regions, in stacking order.
    ///
    /// `base_height` is a header's height in logical pixels at scale 1.0.
    pub fn new(regions: Vec<(RegionKind, f64, NewWidget<dyn Widget>)>) -> Self {
        Self {
            slots: regions
                .into_iter()
                .map(|(kind, base_height, widget)| Slot {
                    kind,
                    pod: widget.to_pod(),
                    base_height,
                    ui_scale: 1.0,
                    pushed_scale: None,
                    size: None,
                })
                .collect(),
            pending: Vec::new(),
            layouts: 0,
            resizes: 0,
            scale_pushes: 0,
        }
    }

    /// A header of `base_height` logical pixels above a main region.
    pub fn header_and_main(base_height: f64, header: NewWidget<dyn Widget>, main: NewWidget<dyn Widget>) -> Self {
        Self::new(vec![
            (RegionKind::Header, base_height, header),
            (RegionKind::Main, 0.0, main),
        ])
    }

    /// Builder form of [`set_ui_scale`](Self::set_ui_scale), for a scale known before
    /// the widget is in a tree.
    ///
    /// The queued value is pushed down by the first layout pass, so a region built
    /// this way is never briefly shown at the wrong size.
    pub fn with_ui_scale(mut self, index: usize, scale: f64) -> Self {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.ui_scale = scale.clamp(0.1, 8.0);
            if !self.pending.contains(&index) {
                self.pending.push(index);
            }
        }
        self
    }

    /// Current counters.
    pub fn counters(&self) -> RegionCounters {
        RegionCounters {
            layouts: self.layouts,
            resizes: self.resizes,
            scale_pushes: self.scale_pushes,
        }
    }

    /// How many regions this area has.
    pub fn region_count(&self) -> usize {
        self.slots.len()
    }

    /// The widget id of each region's root, in stacking order.
    pub fn region_ids(&self) -> Vec<WidgetId> {
        self.slots.iter().map(|slot| slot.pod.id()).collect()
    }

    /// The scale of region `index`.
    pub fn ui_scale(&self, index: usize) -> Option<f64> {
        self.slots.get(index).map(|slot| slot.ui_scale)
    }

    /// Sets the interface scale of one region.
    ///
    /// Takes effect in two steps: this marks the region, the next mutate pass pushes
    /// [`UiScale`] onto its root, and the root's `property_changed` asks for the
    /// re-layout. The mutate pass runs before layout in the same rewrite loop, so a
    /// scale change lands in the frame it was made.
    pub fn set_ui_scale(this: &mut WidgetMut<'_, Self>, index: usize, scale: f64) {
        let scale = scale.clamp(0.1, 8.0);
        let Some(slot) = this.widget.slots.get_mut(index) else {
            return;
        };
        if slot.ui_scale == scale {
            return;
        }
        slot.ui_scale = scale;
        if !this.widget.pending.contains(&index) {
            this.widget.pending.push(index);
        }
        // A header's own height is a function of its scale, so the stack has to be
        // re-laid-out whatever the region's root decides to do about the property.
        this.ctx.request_layout();
    }

    /// Pushes queued scales onto region roots. Runs in the mutate pass.
    fn apply_pending(this: &mut WidgetMut<'_, Self>) {
        let pending = std::mem::take(&mut this.widget.pending);
        for index in pending {
            let scale = this.widget.slots[index].ui_scale;
            this.widget.slots[index].pushed_scale = Some(scale);
            this.widget.scale_pushes += 1;

            let mut root = this.ctx.get_mut(&mut this.widget.slots[index].pod);
            root.insert_prop(UiScale(scale));
        }
    }
}

impl Widget for AreaContent {
    type Action = NoAction;

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        // An area nested inside a scaled region scales with it.
        UiScale::prop_changed(ctx, property_type);
    }

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::MinContent => Length::ZERO,
            LenReq::MaxContent => match axis {
                Axis::Horizontal => Length::px(800.0),
                Axis::Vertical => Length::px(600.0),
            },
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        self.layouts += 1;

        if !self.pending.is_empty() {
            // Setting a property needs a `WidgetMut`, which layout does not have. The
            // mutate pass runs before the next layout pass in the same rewrite loop,
            // so a scale change lands and is laid out in the same frame.
            ctx.mutate_self_later(|mut this| Self::apply_pending(&mut this.downcast::<Self>()));
        }

        // Headers take their scaled height off the top; the last region gets the
        // rest. Rounded to whole pixels for the reason `split_rect` is: a fractional
        // boundary would make every region count as resized on every frame.
        let mut y = 0.0;
        for i in 0..self.slots.len() {
            let last = i + 1 == self.slots.len();
            let height = match self.slots[i].kind {
                _ if last => (size.height - y).max(0.0),
                RegionKind::Header => (self.slots[i].base_height * self.slots[i].ui_scale)
                    .round()
                    .min((size.height - y).max(0.0)),
                RegionKind::Main => (size.height - y).max(0.0),
            };
            let region_size = Size::new(size.width, height);

            if self.slots[i].size != Some(region_size) {
                self.slots[i].size = Some(region_size);
                self.resizes += 1;
            }

            let pod = &mut self.slots[i].pod;
            let chosen = ctx.compute_size(pod, SizeDef::fixed(region_size), region_size.into());
            ctx.run_layout(pod, chosen);
            ctx.place_child(pod, Point::new(0.0, y));
            y += height;
        }
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        for slot in &mut self.slots {
            ctx.register_child(&mut slot.pod);
        }
    }

    fn children_ids(&self) -> ChildrenIds {
        self.slots.iter().map(|slot| slot.pod.id()).collect()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

#[cfg(test)]
mod tests {
    use masonry::core::NewWidget;
    use masonry::dpi::PhysicalSize;
    use masonry::testing::{ModularWidget, TestHarness};
    use masonry::theme::default_property_set;

    use super::*;

    const AREA: (u32, u32) = (600, 400);
    const HEADER: f64 = 24.0;

    /// A region root that reads [`UiScale`] and remembers what it saw.
    ///
    /// Stands in for a real header. The whole question is whether the value reaches
    /// *layout*, so the widget records it there and nowhere else.
    fn scale_reader() -> NewWidget<dyn Widget> {
        NewWidget::new(
            ModularWidget::new(f64::NAN)
                .property_change_fn(|_, ctx, property_type| UiScale::prop_changed(ctx, property_type))
                .measure_fn(|_, _, _, _, len_req, _| match len_req {
                    LenReq::FitContent(space) => space,
                    _ => Length::ZERO,
                })
                .layout_fn(|seen, ctx, props, _| {
                    *seen = props.get::<UiScale>(ctx.property_cache()).0;
                }),
        )
        .erased()
    }

    fn harness() -> TestHarness<AreaContent> {
        let content = AreaContent::header_and_main(HEADER, scale_reader(), scale_reader());
        let mut harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new(content),
            PhysicalSize::new(AREA.0, AREA.1),
        );
        let _ = harness.redraw();
        harness
    }

    fn region_size(harness: &TestHarness<AreaContent>, index: usize) -> Size {
        let id = harness.root_widget().region_ids()[index];
        harness.get_widget_with_id(id).ctx().border_box().size()
    }

    fn seen_scale(harness: &TestHarness<AreaContent>, index: usize) -> f64 {
        let id = harness.root_widget().region_ids()[index];
        harness
            .get_widget_with_id(id)
            .downcast::<ModularWidget<f64>>()
            .expect("regions here are scale readers")
            .state
    }

    #[test]
    fn regions_stack_and_fill_the_area() {
        let harness = harness();
        let header = region_size(&harness, 0);
        let main = region_size(&harness, 1);

        assert_eq!(header, Size::new(AREA.0 as f64, HEADER));
        assert_eq!(main, Size::new(AREA.0 as f64, AREA.1 as f64 - HEADER));
    }

    /// §9's first rule, as a test rather than a timing: the scale is a layout input,
    /// so it has to arrive at the region's layout and change what comes out of it.
    #[test]
    fn ui_scale_reaches_the_regions_layout() {
        let mut harness = harness();
        assert_eq!(seen_scale(&harness, 0), 1.0);

        harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 0, 1.5));
        let _ = harness.redraw();

        assert_eq!(seen_scale(&harness, 0), 1.5, "the header did not see the new scale");
        assert_eq!(
            region_size(&harness, 0),
            Size::new(AREA.0 as f64, HEADER * 1.5),
            "a scaled header takes more room"
        );
    }

    /// The regions are independent knobs: scaling one must leave the other's scale
    /// alone, even though its size necessarily changes to make room.
    #[test]
    fn scaling_one_region_does_not_scale_another() {
        let mut harness = harness();
        harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 0, 2.0));
        let _ = harness.redraw();

        assert_eq!(seen_scale(&harness, 0), 2.0);
        assert_eq!(seen_scale(&harness, 1), 1.0, "the main region has its own scale");
        assert_eq!(region_size(&harness, 1).height, AREA.1 as f64 - HEADER * 2.0);
    }

    #[test]
    fn an_unchanged_scale_is_not_a_change() {
        let mut harness = harness();
        let before = harness.root_widget().counters();
        harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 0, 1.0));
        let _ = harness.redraw();
        assert_eq!(harness.root_widget().counters().scale_pushes, before.scale_pushes);
    }

    #[test]
    fn an_idle_area_resizes_no_region() {
        let mut harness = harness();
        let before = harness.root_widget().counters().resizes;
        for _ in 0..5 {
            let _ = harness.redraw();
        }
        assert_eq!(harness.root_widget().counters().resizes, before);
    }

    /// A scale change costs a resize of the header and of whatever it took the room
    /// from — and of nothing else, which is what makes it a region-local operation.
    #[test]
    fn a_scale_change_resizes_two_regions() {
        let mut harness = harness();
        let before = harness.root_widget().counters().resizes;
        harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 0, 1.5));
        let _ = harness.redraw();
        assert_eq!(harness.root_widget().counters().resizes - before, 2);
    }

    /// The finding §22 rests on, as a measurement rather than a reading of the
    /// source: Masonry's own widgets do not scale, because nothing in them reads
    /// [`UiScale`] and their sizes come from the theme's `DefaultProperties`, which
    /// is one per application rather than one per region.
    ///
    /// Written as a test so that the day upstream grows a per-subtree scale, this
    /// fails and tells us the workaround can go.
    #[test]
    fn masonry_widgets_do_not_follow_ui_scale() {
        use masonry::widgets::Button;

        // The button goes in the main region, whose size does not depend on its own
        // scale — so anything that changes here changed because of the property and
        // not because the region got a different box.
        let content = AreaContent::header_and_main(
            HEADER,
            scale_reader(),
            NewWidget::new(Button::with_text("scale me")).erased(),
        );
        let mut harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new(content),
            PhysicalSize::new(AREA.0, AREA.1),
        );
        let _ = harness.redraw();

        // Padding and border are what "scale the interface" is supposed to move, and
        // they are exactly the gap between the two boxes.
        let insets = |h: &TestHarness<AreaContent>| {
            let id = h.root_widget().region_ids()[1];
            let ctx = h.get_widget_with_id(id);
            let (border, content) = (ctx.ctx().border_box().size(), ctx.ctx().content_box().size());
            Size::new(border.width - content.width, border.height - content.height)
        };
        let before_box = {
            let id = harness.root_widget().region_ids()[1];
            harness.get_widget_with_id(id).ctx().border_box().size()
        };
        let before = insets(&harness);

        harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 1, 2.0));
        let _ = harness.redraw();

        let after_box = {
            let id = harness.root_widget().region_ids()[1];
            harness.get_widget_with_id(id).ctx().border_box().size()
        };
        let after = insets(&harness);
        assert_eq!(
            before_box, after_box,
            "the main region's box must not depend on its own scale"
        );
        assert_eq!(
            before, after,
            "a Masonry widget's padding followed ui_scale; §22 needs rewriting"
        );
        assert!(before.height > 0.0, "the check is vacuous if the button has no padding");
    }

    #[test]
    fn the_scale_is_clamped_to_something_usable() {
        let mut harness = harness();
        harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 0, 1000.0));
        let _ = harness.redraw();
        assert_eq!(harness.root_widget().ui_scale(0), Some(8.0));
    }
}
