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

/// What a region is for.
///
/// Three of the five §8 lists. Headers stack down the area's height; a sidebar takes its
/// width off the right of the body below them — the second axis, which is not "more of
/// the same arithmetic" (§61). A toolbar would be the sidebar's arithmetic on the left.
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
/// `ui_scale` 1.0.
///
/// Kept out of the widget (§61): an area is built again when it is detached into another
/// window or a workspace is read (§41.2, §44), and a sidebar that lived in its widget
/// would come back hidden and at the default width. The owner of the area's other state —
/// its session — holds the [`SidebarState`], and the widget reads it.
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

/// "Hand the scale to your children, now" — a function a widget carries as a property.
///
/// Why a property and why a function, both measured (`issues/rewrite pass budget.md`):
/// carrying a scale by [`UiScale`] alone costs one rewrite pass per level of nesting —
/// each level has to be told, lay itself out, and tell the next through `mutate_later`,
/// which runs in the *next* pass — and Masonry runs four passes per event. Two containers
/// between a region's root and its controls already spill into the next frame, which is
/// drawn half-scaled.
///
/// The way around it is to carry the whole subtree in the pass that has the `WidgetMut`.
/// Nobody can walk another widget's children — `get_mut` wants the `WidgetPod`, and only
/// the parent has it — but whoever holds a child's `WidgetMut` can read the child's
/// properties, and a property can be a function. So a widget that can carry its children
/// carries this, set to its own [`CarriesScale::carry_now`], and [`push_ui_scale`] calls
/// it. A widget without one is carried the slow way, through its own layout, which is
/// correct and costs a pass.
#[derive(Clone, Copy)]
pub struct ScaleCarrier(fn(&mut WidgetMut<'_, dyn Widget>, f64));

impl ScaleCarrier {
    /// The carrier of widgets of type `W`.
    ///
    /// Give it to the widget when it is made — `NewWidget::new(w).with_props(ScaleCarrier::of::<W>())`
    /// — which is the only moment it can be given without a pass of its own.
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

/// A widget that can hand an interface scale to its children in the pass it was given
/// one, rather than through its next layout.
///
/// See [`ScaleCarrier`] for why that is the difference between a scale change that lands
/// in one frame and one that does not.
pub trait CarriesScale: Widget + masonry::core::FromDynWidget {
    /// Applies `scale` to this widget's own state and to its children, now.
    ///
    /// For each child that is itself scaled, call [`push_ui_scale`] on its `WidgetMut`, so
    /// that a carrier further down carries on. The widget's own layout must agree with
    /// what this did — compare against what was carried, not against what was read — or
    /// the two paths will fight over the same state (§46.3).
    fn carry_now(this: &mut WidgetMut<'_, Self>, scale: f64);
}

/// Sets [`UiScale`] on `widget` and, if it carries a [`ScaleCarrier`], hands the scale
/// on to its children in the same pass.
///
/// The one call a holder of a `WidgetMut` makes to scale a subtree. A subtree of
/// carriers is scaled entirely in the pass the call is made in, whatever its depth; a
/// widget without a carrier stops the walk there and carries on through its layout.
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
    /// A sidebar's state; `None` for every other kind.
    sidebar: Option<SidebarState>,
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
    /// Whether this area asks to be recorded as a scene layer of its own.
    ///
    /// The seam per-area caching is built on (§36): a host that can tell one area's
    /// pixels from another's can keep the ones that did not change. Off by default,
    /// because a layer boundary is only worth its cost to a host that caches — and
    /// because it lives exactly one paint (§26.1), so an area that wants to stay a layer
    /// has to be asked to repaint every frame, which the host does.
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
    /// Builds an area from its regions, in stacking order.
    ///
    /// `base_height` is a header's height in logical pixels at scale 1.0, and is
    /// ignored for anything else. The last region takes whatever height is left, so
    /// it is the one that should be [`RegionKind::Main`]; a `Main` that is not last
    /// would leave nothing for the regions after it.
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
    /// says.
    ///
    /// The state is the caller's to keep: the widget reads it in every layout and writes
    /// the width a drag of the grip gives it. After changing it from outside — `N` — call
    /// [`refresh`](Self::refresh).
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

    /// Lays the regions out again after their state changed from outside the widget.
    ///
    /// Showing or hiding a sidebar builds nothing: the sidebar's widget stays, stashed while
    /// hidden, and the main region keeps its widget and only changes width.
    pub fn refresh(this: &mut WidgetMut<'_, Self>) {
        this.ctx.request_layout();
    }

    /// Asks to be recorded as a scene layer of its own.
    ///
    /// What it buys is the possibility of caching: a host that sees this area as its own
    /// layer can keep its pixels across the frames in which it did not change (§36).
    /// What it costs is a repaint request per frame, which is the host's job and not the
    /// screen's — `blazy_shell::window::ShellDriver::layers` names the layer owners once
    /// per frame and the shell asks each of them to repaint. Without that the layer
    /// disappears on the first frame the area is idle, which is exactly the frame worth
    /// caching (§26.1, §36.1).
    #[must_use]
    pub fn with_isolated_layer(mut self, isolated: bool) -> Self {
        self.isolated = isolated;
        self
    }

    /// Builder form of [`set_ui_scale`](Self::set_ui_scale), for a scale known before
    /// the widget is in a tree.
    ///
    /// The queued value is pushed down by the first layout pass, so a region built
    /// this way is never briefly shown at the wrong size.
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

    /// Sets the interface scale of one region.
    ///
    /// Pushes [`UiScale`] onto the region's root in this pass, through [`push_ui_scale`],
    /// so a root that carries a [`ScaleCarrier`] scales its whole subtree before the
    /// layout that follows — in the frame the change was made, whatever its depth.
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
        // Pushed now rather than from the next layout: this is a `WidgetMut`, so the root
        // can be reached in this pass, and waiting for layout to schedule a mutate costs
        // one of the four passes an event has (`issues/rewrite pass budget.md`). The
        // queue stays for the builder, whose widget is not in a tree yet.
        Self::apply_pending(this);
        // A header's own height is a function of its scale, so the stack has to be
        // re-laid-out whatever the region's root decides to do about the property.
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

    /// The grip: a press on it takes the pointer, a move resizes the sidebar, a release
    /// lets go. The grip is the one place in the area no region covers, so these reach this
    /// widget as its own events. The width goes into the [`SidebarState`], not into this
    /// widget, and is not a step in any history — Blender's sidebar width is not either.
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
            // Setting a property needs a `WidgetMut`, which layout does not have. The
            // mutate pass runs before the next layout pass in the same rewrite loop,
            // so a scale change lands and is laid out in the same frame.
            ctx.mutate_self_later(|mut this| Self::apply_pending(&mut this.downcast::<Self>()));
        }

        // A header takes its scaled height off the top; anything else takes what is
        // left, which is why the last region is the one that gets to be `Main`. The
        // height is rounded to whole pixels for the reason `split_rect` is: a
        // fractional boundary would make every region count as resized every frame.
        //
        // A sidebar is the second axis (§61): below the headers, it takes its width off the
        // right of the body, the grip between it and the main region, and the main region
        // takes what is left of the width — rounded too, so a drag of the grip resizes the
        // two regions once per move and never by a fraction.
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
        // Nothing of its own to draw — an area is its regions. The one thing it does
        // here is claim a scene layer, and it has to claim it again in every paint,
        // because the mode is reset for every widget at the start of every paint pass
        // (§26.1, measured in §36.1).
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
