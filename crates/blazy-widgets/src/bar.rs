//! A row of controls that follows the interface scale of its region.

use std::any::TypeId;

use blazy_areas::{CarriesScale, UiScale, push_ui_scale};
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PropertiesRef, RegisterCtx,
    UpdateCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Size};
use masonry::layout::{LenDef, LenReq, Length, SizeDef};

use crate::{Metrics, scale_box};

/// What a bar did about scale, summed over its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BarCounters {
    /// Times the bar noticed a scale it had not applied yet.
    ///
    /// One per change, not one per frame: a bar that re-pushed its children's properties
    /// on every layout would put the whole region through the property pass forever, and
    /// nothing in a counter of *sizes* would show it (§28.4).
    pub scalings: u64,
    /// Children whose box properties were pushed, summed over those scalings.
    ///
    /// The price §22.1 named and did not price: with no inherited properties, carrying a
    /// scale costs one property write per control, and this is that count.
    pub children_scaled: u64,
}

/// A row of controls laid out at the interface scale of the region it sits in.
///
/// This is the forwarding §22.1 describes and §46 prices: `UiScale` lives on the
/// region's root and reaches nobody below it, so a container that wants its children
/// scaled has to carry the value itself. The bar carries it through both channels —
/// [`scale_box`] for anything with a box, and [`UiScale`] pushed onto the child for
/// widgets of this crate, which read it and scale what only they can reach (text).
///
/// Children are declared with the [`Metrics`] they have **at scale 1**; see the crate
/// docs for why the baseline is the caller's and not read back from the widget.
pub struct Bar {
    children: Vec<Child>,
    /// The bar's own metrics: padding around the row and the gap between children.
    metrics: Metrics,
    /// The scale the children were last pushed at, so an unchanged scale costs nothing.
    applied: Option<f64>,
    counters: BarCounters,
}

struct Child {
    pod: WidgetPod<dyn Widget>,
    metrics: Metrics,
}

impl Bar {
    /// An empty bar with the given metrics at scale 1.
    pub fn new(metrics: Metrics) -> Self {
        Self {
            children: Vec::new(),
            metrics,
            applied: None,
            counters: BarCounters::default(),
        }
    }

    /// Adds a control, declaring what it measures at scale 1.
    #[must_use]
    pub fn with(mut self, child: NewWidget<dyn Widget>, metrics: Metrics) -> Self {
        self.children.push(Child {
            pod: child.to_pod(),
            metrics,
        });
        self
    }

    /// What this bar did about scale.
    pub fn counters(&self) -> BarCounters {
        self.counters
    }

    /// The scale its children were last pushed at, if any.
    pub fn applied_scale(&self) -> Option<f64> {
        self.applied
    }

    /// Pushes the scale onto every child, once per change.
    ///
    /// In `layout` rather than in `property_changed`, because that is where the value is:
    /// `property_changed` is told *which* property moved and not what it became, and a
    /// container that guessed would push a stale number (§22). In `layout` rather than in
    /// `measure` for a blunter reason — a region is laid out at a size its parent fixed,
    /// so `measure` is never called on it at all, and a bar that carried the scale there
    /// carried it never (§46).
    ///
    /// The mutate pass runs before the next layout pass of the same rewrite loop, so the
    /// children are scaled and laid out in the frame the scale changed in.
    fn carry(&mut self, ctx: &mut LayoutCtx<'_>, scale: f64) {
        if self.applied == Some(scale) {
            return;
        }
        self.applied = Some(scale);
        self.counters.scalings += 1;
        self.counters.children_scaled += self.children.len() as u64;
        for child in &mut self.children {
            let metrics = child.metrics;
            ctx.mutate_child_later(&mut child.pod, move |mut widget| {
                scale_child(&mut widget, metrics, scale)
            });
        }
    }
}

/// The fast path: the whole row in the pass the scale arrived in (`ScaleCarrier`).
///
/// The same work the bar does from its layout, recorded in the same `applied`, so
/// that the layout that follows finds nothing left to do — one owner for the state, two
/// ways to reach it (§46.3).
impl CarriesScale for Bar {
    fn carry_now(this: &mut WidgetMut<'_, Self>, scale: f64) {
        if this.widget.applied == Some(scale) {
            return;
        }
        this.widget.applied = Some(scale);
        this.widget.counters.scalings += 1;
        this.widget.counters.children_scaled += this.widget.children.len() as u64;
        for index in 0..this.widget.children.len() {
            let metrics = this.widget.children[index].metrics;
            let mut child = this.ctx.get_mut(&mut this.widget.children[index].pod);
            scale_child(&mut child, metrics, scale);
        }
        this.ctx.request_layout();
    }
}

/// Hands `scale` to one control of a bar: the value, the caption, the box.
fn scale_child(widget: &mut WidgetMut<'_, dyn Widget>, metrics: Metrics, scale: f64) {
    // The property first, and onto *every* child: it is the general mechanism, it is
    // what a widget of ours deeper down will read, and it is the value the child's own
    // layout will compare against. Handing the scale by call to some children and by
    // property to others is two owners for one piece of state — the caption's own layout
    // then read the property it never got and put the text back (§46). Pushed rather than
    // inserted, so a child that carries a scale of its own carries it on in this pass.
    push_ui_scale(widget, scale);
    // And then, for a caption, the same scale *now* rather than through another rewrite
    // pass: told by property alone it would have to be told, lay itself out and tell its
    // own inner label, which is a pass per level — and four is all an event has.
    if let Some(mut label) = widget.try_downcast::<crate::Label>() {
        crate::Label::set_scale(&mut label, scale);
    } else if let Some(mut button) = widget.try_downcast::<masonry::widgets::Button>() {
        // One stock container is known by name here, and only one: a button with a
        // caption is the commonest control of a dense UI, and its caption is a child it
        // does not forward anything to. Reaching it is exactly what an application should
        // not have to write — the box channel scales the button and leaves the words
        // inside it at their old size, which looks like a bug in the scale and is not
        // (§46).
        let mut child = masonry::widgets::Button::child_mut(&mut button);
        // The property here too, and for the same reason as above: a caption given the
        // call but not the value compares its own layout against a scale of 1 and puts
        // the text back, one frame later, for ever.
        push_ui_scale(&mut child, scale);
        if let Some(mut label) = child.try_downcast::<crate::Label>() {
            crate::Label::set_scale(&mut label, scale);
        }
    }
    scale_box(widget, metrics, scale);
}

impl Widget for Bar {
    type Action = NoAction;

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        UiScale::prop_changed(ctx, property_type);
    }

    fn measure(
        &mut self,
        ctx: &mut MeasureCtx<'_>,
        props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        cross_length: Option<Length>,
    ) -> Length {
        let scale = props.get::<UiScale>(ctx.property_cache()).0;
        let metrics = self.metrics.at(scale);

        // The bar's own box is not known while measuring, and a child of a bar is sized
        // by its content rather than by a share of the row, so there is nothing relative
        // to resolve against.
        let context = masonry::layout::LayoutSize::default();
        match (axis, len_req) {
            (Axis::Horizontal, LenReq::MinContent | LenReq::MaxContent) => {
                let mut width = 2.0 * metrics.padding;
                for (index, child) in self.children.iter_mut().enumerate() {
                    if index > 0 {
                        width += metrics.gap;
                    }
                    width += ctx
                        .compute_length(&mut child.pod, LenDef::MaxContent, context, axis, cross_length)
                        .get();
                }
                Length::px(width)
            },
            (Axis::Vertical, LenReq::MinContent | LenReq::MaxContent) => {
                let mut height: f64 = 0.0;
                for child in &mut self.children {
                    let length = ctx
                        .compute_length(&mut child.pod, LenDef::MaxContent, context, axis, cross_length)
                        .get();
                    height = height.max(length);
                }
                Length::px(height + 2.0 * metrics.padding)
            },
            (_, LenReq::FitContent(space)) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, props: &PropertiesRef<'_>, size: Size) {
        let scale = props.get::<UiScale>(ctx.property_cache()).0;
        self.carry(ctx, scale);
        let metrics = self.metrics.at(scale);
        let inner_height = (size.height - 2.0 * metrics.padding).max(0.0);
        let mut x = metrics.padding;
        for child in &mut self.children {
            let available = Size::new((size.width - x).max(0.0), inner_height);
            let chosen = ctx.compute_size(&mut child.pod, SizeDef::fit(available), size.into());
            let chosen = Size::new(chosen.width.min(available.width), inner_height);
            ctx.run_layout(&mut child.pod, chosen);
            ctx.place_child(&mut child.pod, Point::new(x, metrics.padding));
            x += chosen.width + metrics.gap;
        }
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        for child in &mut self.children {
            ctx.register_child(&mut child.pod);
        }
    }

    fn children_ids(&self) -> ChildrenIds {
        let ids: Vec<WidgetId> = self.children.iter().map(|child| child.pod.id()).collect();
        ChildrenIds::from_slice(&ids)
    }

    fn accessibility_role(&self) -> Role {
        Role::Toolbar
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}
