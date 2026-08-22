//! A region header that actually honours [`UiScale`].
//!
//! The point of the Phase 0.6 spike is that `ui_scale` is a *layout* input, and a
//! widget that ignores it proves nothing either way. This one is deliberately made of
//! the parts that scaling is supposed to affect — control sizes and the gaps between
//! them — computed from the property in `measure` and `layout` rather than baked in.
//!
//! It also records the scale it last laid out at. Whether a region's root noticed a
//! scale change cannot be asked from the mutate pass that pushed it (`blazy-areas`
//! says why); the widget that reads the property is the one that knows, so the answer
//! lives here and the benchmark reads it back.

use std::any::TypeId;

use blazy_areas::UiScale;
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NoAction, PaintCtx, PropertiesRef, RegisterCtx, UpdateCtx, Widget,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Rect, RoundedRect, Size};
use masonry::layout::{LenReq, Length};
use masonry::peniko::Color;

/// Number of mock controls in the header.
const CONTROLS: usize = 5;
/// Size of one control at scale 1.0, in logical pixels.
const CONTROL: f64 = 16.0;
/// Gap between controls at scale 1.0.
const GAP: f64 = 6.0;

/// A strip of mock controls whose size follows the region's [`UiScale`].
pub struct ScaledHeader {
    tint: Color,
    /// The scale the last layout ran at.
    seen_scale: f64,
    /// Layout passes run.
    layouts: u64,
}

impl ScaledHeader {
    pub fn new(tint: Color) -> Self {
        Self {
            tint,
            seen_scale: 1.0,
            layouts: 0,
        }
    }

    /// The scale this header last laid itself out at.
    ///
    /// The benchmark compares it against the scale it asked for: a mismatch means the
    /// property did not reach layout, which is exactly the failure §9 is about.
    pub fn seen_scale(&self) -> f64 {
        self.seen_scale
    }

    /// Layout passes run on this header.
    pub fn layouts(&self) -> u64 {
        self.layouts
    }

    /// Width the controls need at `scale`.
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

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, props: &PropertiesRef<'_>, _size: Size) {
        self.layouts += 1;
        self.seen_scale = props.get::<UiScale>(ctx.property_cache()).0;
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        let scale = props.get::<UiScale>(ctx.property_cache()).0;
        let bounds = ctx.border_box();
        painter.fill(bounds, Color::from_rgb8(0x22, 0x22, 0x28)).draw();

        let control = CONTROL * scale;
        let gap = GAP * scale;
        let y = (bounds.height() - control) / 2.0;
        for i in 0..CONTROLS {
            let x = gap + i as f64 * (control + gap);
            if x + control > bounds.width() {
                break;
            }
            let rect = Rect::new(x, y, x + control, y + control);
            painter
                .fill(RoundedRect::from_rect(rect, 3.0 * scale), self.tint)
                .draw();
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
