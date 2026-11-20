//! Regions inside an area, and the per-region `ui_scale`.
//!
//! An area holds regions — a header, the main view, a sidebar — each with its own
//! `ui_scale`: the interface size, not the HiDPI factor and not the content's pan and zoom.
//! `ui_scale` goes into layout, and re-lays out only its region; `view` goes into paint.
//!
//! Masonry has no inherited properties: [`UiScale`] is set on the region's root
//! (`WidgetMut::insert_prop`, which fires `Widget::property_changed`), and a container has
//! to forward it to its children ([`ScaleCarrier`]).

use std::any::TypeId;
use std::cell::Cell;
use std::rc::Rc;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PaintLayerMode, PropertiesRef,
    Property, RegisterCtx, UpdateCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Size};
use masonry::layout::{LenReq, Length, SizeDef};

/// What a region is for. Headers stack down the area's height; a sidebar takes its width
/// off the right of the body below them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegionKind {
    /// A strip of fixed logical height across the top, scaled by its `ui_scale`.
    Header,
    /// Whatever is left. The editor proper.
    Main,
    /// A panel on the right of the body, as wide as its [`SidebarState`] says, shown or
    /// hidden — Blender's sidebar (`N`). Added with [`AreaContent::with_sidebar`].
    Sidebar,
}

/// What a sidebar is: whether it is shown, and how wide it is, in logical pixels at
/// `ui_scale` 1.0. Kept outside the widget (in a [`SidebarState`]), so it survives a rebuilt
/// area.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct Sidebar {
    /// Whether the sidebar takes its width off the main region.
    pub shown: bool,
    /// Its width at `ui_scale` 1.0.
    pub width: f64,
}

impl Sidebar {
    /// A sidebar `width` wide, shown or not.
    pub fn new(shown: bool, width: f64) -> Self {
        Self {
            shown,
            width: width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH),
        }
    }
}

impl Default for Sidebar {
    /// Hidden, as Blender's starts, and as wide as Blender's default.
    fn default() -> Self {
        Self::new(false, 240.0)
    }
}

/// A sidebar's state, shared between the area's widget and whoever keeps the area's state
/// across rebuilds.
pub type SidebarState = Rc<Cell<Sidebar>>;

/// The narrowest a sidebar is dragged to, in logical pixels at scale 1.0.
pub const SIDEBAR_MIN_WIDTH: f64 = 120.0;
/// The widest, likewise.
pub const SIDEBAR_MAX_WIDTH: f64 = 900.0;
/// The grip between the main region and a shown sidebar, in logical pixels: the strip a
/// drag resizes the sidebar by, and the one place in the area that is the area's own.
pub const SIDEBAR_GRIP: f64 = 4.0;

/// The interface scale of a region, as a property on its root widget: a multiplier on
/// logical lengths, read in `measure` and `layout` (not in `paint`). Masonry's own widgets
/// do not read it.
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
    pub fn prop_changed(ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        if property_type == TypeId::of::<Self>() {
            ctx.request_layout();
        }
    }
}

/// "Hand the scale to your children, now" — a function a widget carries as a property.
///
/// Carried through layouts, a scale costs a rewrite pass per level of nesting, and Masonry
/// runs four per event. A widget carrying this ([`CarriesScale::carry_now`]) scales its
/// children in the pass [`push_ui_scale`] reaches it in.
#[derive(Clone, Copy)]
pub struct ScaleCarrier(fn(&mut WidgetMut<'_, dyn Widget>, f64));

impl ScaleCarrier {
    /// The carrier of widgets of type `W`.
    ///
    /// Give it when the widget is made: `NewWidget::new(w).with_props(ScaleCarrier::of::<W>())`.
    pub fn of<W: CarriesScale>() -> Self {
        Self(|widget, scale| {
            if let Some(mut this) = widget.try_downcast::<W>() {
                W::carry_now(&mut this, scale);
            }
        })
    }
}

impl std::fmt::Debug for ScaleCarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScaleCarrier")
    }
}

impl Property for ScaleCarrier {
    fn static_default() -> &'static Self {
        static DEFAULT: ScaleCarrier = ScaleCarrier(|_, _| {});
        &DEFAULT
    }
}

impl Default for ScaleCarrier {
    fn default() -> Self {
        *Self::static_default()
    }
}

/// A widget that can hand an interface scale to its children in the pass it was given one.
pub trait CarriesScale: Widget + masonry::core::FromDynWidget {
    /// Applies `scale` to this widget's own state and to its children, now.
    ///
    /// Call [`push_ui_scale`] on each scaled child. The widget's layout must agree with what
    /// was carried.
    fn carry_now(this: &mut WidgetMut<'_, Self>, scale: f64);
}

/// Sets [`UiScale`] on `widget` and, if it carries a [`ScaleCarrier`], hands the scale
/// on to its children in the same pass.
///
/// A subtree of carriers is scaled in this pass; a widget without one carries on through
/// its layout.
pub fn push_ui_scale(widget: &mut WidgetMut<'_, dyn Widget>, scale: f64) {
    widget.insert_prop(UiScale(scale));
    let carrier = *widget.get_prop::<ScaleCarrier>();
    (carrier.0)(widget, scale);
}

/// Cumulative counters, for spotting work that should not be happening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RegionCounters {
    /// Layout passes run on the area's region stack.
    pub layouts: u64,
    /// Regions handed a border-box size different from their last one.
    pub resizes: u64,
    /// Times a new [`UiScale`] was pushed down to a region's root.
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
    /// A sidebar's state; `None` for every other kind.
    sidebar: Option<SidebarState>,
}

/// The regions filling one area of an [`AreaScreen`](crate::AreaScreen): headers stacked
/// down the top, the main region below, a sidebar on its right.
pub struct AreaContent {
    slots: Vec<Slot>,
    /// Whether this area asks to be recorded as a scene layer of its own, for a host that
    /// caches layers. Off by default.
    isolated: bool,
    /// Regions whose root needs a new [`UiScale`], applied in the mutate pass.
    pending: Vec<usize>,
    /// Where the grip is, as of the last layout, if a sidebar is shown.
    grip: Option<masonry::kurbo::Rect>,
    /// A drag of the grip: the sidebar's slot, and the pointer's distance from its edge.
    dragging: Option<(usize, f64)>,
    layouts: u64,
    resizes: u64,
    scale_pushes: u64,
}

impl AreaContent {
    /// Builds an area from its regions, in stacking order. `base_height` is a header's
    /// height at scale 1.0; the last region, [`RegionKind::Main`], takes what is left.
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
                    sidebar: None,
                })
                .collect(),
            isolated: false,
            pending: Vec::new(),
            grip: None,
            dragging: None,
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

    /// The same area with a sidebar on the right of its body, shown and sized as `state`
    /// says. After changing `state` from outside, call [`refresh`](Self::refresh).
    #[must_use]
    pub fn with_sidebar(mut self, widget: NewWidget<dyn Widget>, state: SidebarState) -> Self {
        self.slots.push(Slot {
            kind: RegionKind::Sidebar,
            pod: widget.to_pod(),
            base_height: 0.0,
            ui_scale: 1.0,
            pushed_scale: None,
            size: None,
            sidebar: Some(state),
        });
        self
    }

    /// Lays the regions out again after their state changed from outside the widget. A
    /// hidden sidebar is stashed, not dropped.
    pub fn refresh(this: &mut WidgetMut<'_, Self>) {
        this.ctx.request_layout();
    }

    /// Asks to be recorded as a scene layer of its own, so a host can keep its pixels. The
    /// host must ask it to repaint every frame (`ShellDriver::layers`).
    #[must_use]
    pub fn with_isolated_layer(mut self, isolated: bool) -> Self {
        self.isolated = isolated;
        self
    }

    /// Builder form of [`set_ui_scale`](Self::set_ui_scale), applied by the first layout.
    #[must_use]
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

    /// The widget id of each region's root, in stacking order.
    pub fn region_ids(&self) -> Vec<WidgetId> {
        self.slots.iter().map(|slot| slot.pod.id()).collect()
    }

    /// The scale of region `index`.
    pub fn ui_scale(&self, index: usize) -> Option<f64> {
        self.slots.get(index).map(|slot| slot.ui_scale)
    }

    /// Sets the interface scale of one region, pushed down in this pass ([`push_ui_scale`]).
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
        // Now, rather than from the next layout: that would cost a rewrite pass.
        Self::apply_pending(this);
        // A header's height depends on its scale.
        this.ctx.request_layout();
    }

    /// Lays out and places region `i` at `origin`, `size` big, counting a change of size.
    fn place(&mut self, ctx: &mut LayoutCtx<'_>, i: usize, origin: Point, size: Size) {
        if self.slots[i].size != Some(size) {
            self.slots[i].size = Some(size);
            self.resizes += 1;
        }
        let pod = &mut self.slots[i].pod;
        let chosen = ctx.compute_size(pod, SizeDef::fixed(size), size.into());
        ctx.run_layout(pod, chosen);
        ctx.place_child(pod, origin);
    }

    /// The width a drag of the grip to `x` gives the sidebar in slot `i`, at scale 1.0.
    fn dragged_width(&self, i: usize, x: f64, offset: f64, area_width: f64) -> f64 {
        let scale = self.slots[i].ui_scale.max(f64::EPSILON);
        let width = (area_width - (x - offset) - SIDEBAR_GRIP) / scale;
        width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
    }

    /// Pushes queued scales onto region roots. Runs in the mutate pass.
    fn apply_pending(this: &mut WidgetMut<'_, Self>) {
        let pending = std::mem::take(&mut this.widget.pending);
        for index in pending {
            let scale = this.widget.slots[index].ui_scale;
            this.widget.slots[index].pushed_scale = Some(scale);
            this.widget.scale_pushes += 1;

            let mut root = this.ctx.get_mut(&mut this.widget.slots[index].pod);
            push_ui_scale(&mut root, scale);
        }
    }
}

impl Widget for AreaContent {
    type Action = NoAction;

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        // An area nested inside a scaled region scales with it.
        UiScale::prop_changed(ctx, property_type);
    }

    /// The grip, the one place no region covers: a drag resizes the sidebar, writing the
    /// [`SidebarState`] (not a history step, as in Blender).
    fn on_pointer_event(
        &mut self,
        ctx: &mut masonry::core::EventCtx<'_>,
        _props: &mut masonry::core::PropertiesMut<'_>,
        event: &masonry::core::PointerEvent,
    ) {
        use masonry::core::PointerEvent;
        let side = (0..self.slots.len()).find(|&i| self.slots[i].kind == RegionKind::Sidebar);
        let (Some(side), Some(grip)) = (side, self.grip) else {
            return;
        };
        match event {
            PointerEvent::Down(down) => {
                let at = ctx.local_position(down.state.position);
                if grip.contains(at) {
                    self.dragging = Some((side, at.x - grip.x0));
                    ctx.capture_pointer();
                    ctx.set_handled();
                }
            },
            PointerEvent::Move(update) => {
                if let Some((i, offset)) = self.dragging {
                    let at = ctx.local_position(update.current.position);
                    let width = self.dragged_width(i, at.x, offset, ctx.content_box().width());
                    if let Some(state) = &self.slots[i].sidebar {
                        let mut sidebar = state.get();
                        if sidebar.width != width {
                            sidebar.width = width;
                            state.set(sidebar);
                            ctx.request_layout();
                        }
                    }
                    ctx.set_handled();
                }
            },
            PointerEvent::Up(_) | PointerEvent::Cancel(_) if self.dragging.take().is_some() => {
                ctx.release_pointer();
                ctx.set_handled();
            },
            _ => {},
        }
    }

    /// The resize cursor over the grip and while it is dragged.
    fn get_cursor(&self, _ctx: &masonry::core::QueryCtx<'_>, pos: Point) -> masonry::core::CursorIcon {
        if self.dragging.is_some() || self.grip.is_some_and(|grip| grip.contains(pos)) {
            masonry::core::CursorIcon::EwResize
        } else {
            masonry::core::CursorIcon::Default
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
            // Setting a property needs the mutate pass, which runs before the next layout.
            ctx.mutate_self_later(|mut this| Self::apply_pending(&mut this.downcast::<Self>()));
        }

        // Headers take their scaled height off the top; the sidebar and the grip take their
        // width off the right of the body; the main region takes what is left. All rounded
        // to whole pixels, or every region counts as resized every frame.
        let flow: Vec<usize> = (0..self.slots.len())
            .filter(|&i| self.slots[i].kind != RegionKind::Sidebar)
            .collect();
        let side = (0..self.slots.len()).find(|&i| self.slots[i].kind == RegionKind::Sidebar);
        let mut y = 0.0;
        let mut body = None;
        for (at, &i) in flow.iter().enumerate() {
            let last = at + 1 == flow.len();
            let remaining = (size.height - y).max(0.0);
            let height = match self.slots[i].kind {
                RegionKind::Header if !last => (self.slots[i].base_height * self.slots[i].ui_scale)
                    .round()
                    .min(remaining),
                _ => remaining,
            };
            if last {
                body = Some((i, y, height));
                break;
            }
            self.place(ctx, i, Point::new(0.0, y), Size::new(size.width, height));
            y += height;
        }
        let Some((main, y, height)) = body else {
            return;
        };
        let shown = side.and_then(|i| {
            let state = self.slots[i].sidebar.as_ref()?.get();
            state.shown.then(|| {
                let width = (state.width * self.slots[i].ui_scale)
                    .round()
                    .min((size.width - SIDEBAR_GRIP).max(0.0));
                (i, width)
            })
        });
        if let Some(i) = side {
            let stashed = shown.is_none();
            ctx.set_stashed(&mut self.slots[i].pod, stashed);
        }
        match shown {
            Some((i, width)) => {
                let main_width = (size.width - width - SIDEBAR_GRIP).max(0.0);
                self.place(ctx, main, Point::new(0.0, y), Size::new(main_width, height));
                self.place(
                    ctx,
                    i,
                    Point::new(main_width + SIDEBAR_GRIP, y),
                    Size::new(width, height),
                );
                self.grip = Some(masonry::kurbo::Rect::new(
                    main_width,
                    y,
                    main_width + SIDEBAR_GRIP,
                    y + height,
                ));
            },
            None => {
                self.place(ctx, main, Point::new(0.0, y), Size::new(size.width, height));
                self.grip = None;
            },
        }
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {
        // Nothing to draw; claims its scene layer, which every paint pass resets.
        if self.isolated {
            ctx.set_paint_layer_mode(PaintLayerMode::IsolatedScene);
        }
    }

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

    /// The first rule, as a test rather than a timing: the scale is a layout input,
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

    /// Masonry's own widgets do not read [`UiScale`]. Fails the day upstream scales a
    /// subtree, and the workaround can go.
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
            "a Masonry widget's padding followed ui_scale; the architecture notes need rewriting"
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

    /// A header above, a main region and a sidebar below it, the sidebar shown at 200.
    fn with_sidebar(shown: bool) -> (TestHarness<AreaContent>, SidebarState) {
        let state: SidebarState = Rc::new(Cell::new(Sidebar::new(shown, 200.0)));
        let content = AreaContent::header_and_main(HEADER, scale_reader(), scale_reader())
            .with_sidebar(scale_reader(), state.clone());
        let mut harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new(content),
            PhysicalSize::new(AREA.0, AREA.1),
        );
        let _ = harness.redraw();
        (harness, state)
    }

    /// The sidebar is the second axis: it takes its width off the right of the body below
    /// the header, the grip between, and the main region keeps the height it had.
    #[test]
    fn a_sidebar_takes_its_width_off_the_main_region_only() {
        let (mut harness, state) = with_sidebar(true);
        let body = AREA.1 as f64 - HEADER;
        assert_eq!(
            region_size(&harness, 0),
            Size::new(AREA.0 as f64, HEADER),
            "the header is whole"
        );
        assert_eq!(
            region_size(&harness, 1),
            Size::new(AREA.0 as f64 - 200.0 - SIDEBAR_GRIP, body)
        );
        assert_eq!(region_size(&harness, 2), Size::new(200.0, body));

        // Hidden: the main region takes the whole width, and the sidebar is stashed, not
        // dropped — the same widget comes back.
        let sidebar = harness.root_widget().region_ids()[2];
        let mut hidden = state.get();
        hidden.shown = false;
        state.set(hidden);
        harness.edit_root_widget(|mut content| AreaContent::refresh(&mut content));
        let _ = harness.redraw();
        assert_eq!(region_size(&harness, 1), Size::new(AREA.0 as f64, body));
        assert!(
            harness.get_widget_with_id(sidebar).ctx().is_stashed(),
            "hidden is stashed"
        );
        state.set(Sidebar::new(true, 200.0));
        harness.edit_root_widget(|mut content| AreaContent::refresh(&mut content));
        let _ = harness.redraw();
        assert_eq!(
            harness.root_widget().region_ids()[2],
            sidebar,
            "and the same widget is back"
        );
        assert!(!harness.get_widget_with_id(sidebar).ctx().is_stashed());

        // The sidebar's own scale widens it, and changes nothing of the main region's but
        // its width.
        harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 2, 1.5));
        let _ = harness.redraw();
        assert_eq!(region_size(&harness, 2), Size::new(300.0, body));
        assert_eq!(
            region_size(&harness, 1).height,
            body,
            "the main region's height is untouched"
        );
        assert_eq!(seen_scale(&harness, 1), 1.0, "and so is its scale");
    }

    /// The grip between the regions widens the sidebar under the pointer, in whole pixels —
    /// one resize of each region per move — writes the width to the state and not only to
    /// the widget, and stops at the limits.
    #[test]
    fn the_grip_widens_the_sidebar_in_whole_pixels_into_its_state() {
        use masonry::core::CursorIcon;
        use masonry::ui_events::pointer::PointerButton;

        let (mut harness, state) = with_sidebar(true);
        // The grip is between x = 396 and 400.
        let y = 200.0;
        harness.mouse_move(Point::new(397.5, y));
        assert_eq!(harness.cursor_icon(), CursorIcon::EwResize, "the grip shows it is one");
        harness.mouse_button_press(Some(PointerButton::Primary));
        let before = harness.root_widget().counters().resizes;
        for x in [390.3, 380.7, 350.2] {
            harness.mouse_move(Point::new(x, y));
        }
        let _ = harness.redraw();
        let resizes = harness.root_widget().counters().resizes - before;
        assert!(resizes <= 6, "two regions, three moves: {resizes}");
        let width = region_size(&harness, 2).width;
        assert_eq!(width, width.round(), "whole pixels");
        assert!(
            (state.get().width - 247.3).abs() < 1e-9,
            "the state has it: {:?}",
            state.get()
        );
        harness.mouse_move(Point::new(-500.0, y));
        assert_eq!(
            state.get().width,
            SIDEBAR_MAX_WIDTH.min(AREA.0 as f64 + 500.0 - 2.5 - SIDEBAR_GRIP)
        );
        harness.mouse_move(Point::new(590.0, y));
        assert_eq!(state.get().width, SIDEBAR_MIN_WIDTH, "no narrower than the least");
        harness.mouse_button_release(Some(PointerButton::Primary));
        let after = state.get().width;
        harness.mouse_move(Point::new(300.0, y));
        assert_eq!(state.get().width, after, "let go, the pointer moves nothing");
    }
}
