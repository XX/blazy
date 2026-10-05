//! A region header that actually honours [`UiScale`].
//!
//! The point of the Phase 0.6 spike is that `ui_scale` is a *layout* input, and a
//! widget that ignores it proves nothing either way. This one is made of the parts
//! scaling is supposed to affect, and since §46 they are **real controls** rather than
//! five painted rectangles: a caption from `blazy-widgets`, which owns the text channel,
//! and stock Masonry buttons, which follow the box properties the bar pushes onto them.
//! That is the whole claim of §46 standing in the window rather than only in a test.
//!
//! It also records the scale it last laid out at. Whether a region's root noticed a
//! scale change cannot be asked from the mutate pass that pushed it (`blazy-areas`
//! says why); the widget that reads the property is the one that knows, so the answer
//! lives here and the benchmark reads it back.

use std::any::TypeId;

use blazy::areas::UiScale;
use blazy::masonry::accesskit::{Node, Role};
use blazy::masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PropertiesRef, RegisterCtx,
    UpdateCtx, Widget, WidgetPod,
};
use blazy::masonry::imaging::Painter;
use blazy::masonry::kurbo::{Axis, Point, Rect, Size};
use blazy::masonry::layout::{LenReq, Length, SizeDef};
use blazy::masonry::peniko::Color;
use blazy::masonry::widgets::Button;
use blazy::widgets::{Bar, Label, Metrics};

/// Number of buttons beside the caption.
const CONTROLS: usize = 3;
/// Size of one control at scale 1.0, in logical pixels.
const CONTROL: f64 = 16.0;
/// Gap between controls at scale 1.0.
const GAP: f64 = 6.0;

/// A caption and a few buttons, all sized at the region's [`UiScale`].
pub struct ScaledHeader {
    tint: Color,
    /// The scale the last layout ran at.
    seen_scale: f64,
    /// The row of controls, which is what carries the scale to each of them (§46).
    bar: WidgetPod<Bar>,
    /// The scale last handed to the bar.
    ///
    /// A container between the region's root and the controls has to pass the scale on
    /// itself — there is no inheritance (§22.1). Forgetting it is invisible: the header
    /// lays itself out at the new scale and everything inside it stays at 1 (§46).
    carried: Option<f64>,
}

impl ScaledHeader {
    pub fn new(tint: Color) -> Self {
        Self::named(tint, "area")
    }

    /// A header with the caption and nothing else.
    ///
    /// For a measurement that magnifies the header and compares the two images: the bar
    /// lays out as many controls as fit, so a header with buttons shows *different*
    /// content at two sizes, and a sharpness metric comparing one content with another
    /// measures nothing (§23, §46).
    pub fn caption(tint: Color, name: impl Into<std::sync::Arc<str>>) -> Self {
        Self::build(tint, name, 0)
    }

    /// A header captioned with the area's name.
    pub fn named(tint: Color, name: impl Into<std::sync::Arc<str>>) -> Self {
        Self::build(tint, name, CONTROLS)
    }

    fn build(tint: Color, name: impl Into<std::sync::Arc<str>>, controls: usize) -> Self {
        let metrics = Metrics {
            padding: 3.0,
            gap: GAP,
            ..Metrics::default()
        };
        let mut bar = Bar::new(metrics).with(NewWidget::new(Label::new(name)).erased(), metrics);
        for i in 0..controls {
            // Stock Masonry buttons on purpose: §46's measurement is that they need no
            // replacing, and a header that used widgets of ours everywhere would prove
            // nothing about that.
            // A stock button with a caption of ours inside: the button needs no
            // replacing — its box follows the properties the bar pushes — but the words
            // in it are a child it forwards nothing to, so the caption is one of ours
            // (§46).
            let caption = match i {
                0 => "split",
                1 => "join",
                _ => "max",
            };
            let button = Button::new(NewWidget::new(Label::new(caption)));
            bar = bar.with(NewWidget::new(button).erased(), metrics);
        }
        Self {
            tint,
            seen_scale: 1.0,
            bar: WidgetPod::new(bar),
            carried: None,
        }
    }

    /// The scale this header last laid itself out at.
    ///
    /// The benchmark compares it against the scale it asked for: a mismatch means the
    /// property did not reach layout, which is exactly the failure §9 is about.
    pub fn seen_scale(&self) -> f64 {
        self.seen_scale
    }

    /// Width the controls need at `scale`.
    ///
    /// The bar measures itself from its children, and this is the floor under it: the
    /// header has a size of its own even before anything is in it, and the benchmark
    /// compares scales rather than pixels.
    fn content_width(scale: f64) -> f64 {
        CONTROLS as f64 * CONTROL * scale + (CONTROLS as f64 + 1.0) * GAP * scale
    }
}

impl Widget for ScaledHeader {
    type Action = NoAction;

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        // A scale change is a layout change. Asking only for a repaint here is the
        // mistake that makes `ui_scale` look free and behave wrong.
        UiScale::prop_changed(ctx, property_type);
    }

    fn measure(
        &mut self,
        ctx: &mut MeasureCtx<'_>,
        props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        let scale = props.get::<UiScale>(ctx.property_cache()).0;
        match (axis, len_req) {
            (Axis::Horizontal, LenReq::MinContent | LenReq::MaxContent) => Length::px(Self::content_width(scale)),
            (Axis::Vertical, LenReq::MinContent | LenReq::MaxContent) => Length::px((CONTROL + 2.0 * GAP) * scale),
            (_, LenReq::FitContent(space)) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, props: &PropertiesRef<'_>, size: Size) {
        self.seen_scale = props.get::<UiScale>(ctx.property_cache()).0;
        blazy::widgets::carry_ui_scale(ctx, &mut self.bar, &mut self.carried, self.seen_scale);
        let chosen = ctx.compute_size(&mut self.bar, SizeDef::fixed(size), size.into());
        ctx.run_layout(&mut self.bar, chosen);
        ctx.place_child(&mut self.bar, Point::ORIGIN);
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        // The strip behind the controls, tinted so the areas are told apart by eye. The
        // controls are children and paint themselves.
        let bounds = ctx.border_box();
        painter.fill(bounds, Color::from_rgb8(0x22, 0x22, 0x28)).draw();
        painter
            .fill(Rect::new(bounds.x0, bounds.y0, bounds.x1, bounds.y0 + 3.0), self.tint)
            .draw();
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.bar);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.bar.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}
