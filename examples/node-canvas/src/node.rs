// Copyright 2026 the blazy Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A graph node: a rounded body with a coloured header and interactive controls.
//!
//! The node is a container, not a painter callback. That is the whole point of
//! claim 3: its slider and checkbox are stock Masonry widgets, unmodified, and they
//! keep working when the canvas is zoomed because Masonry inverts `window_transform`
//! when routing pointer events.
//!
//! The node also implements level of detail. The interesting part is not that it
//! draws less when zoomed out, but that at [`Detail::Box`] it *stashes* its
//! contents: a stashed widget is not laid out, not painted and not hit-tested.

use std::any::TypeId;

use blazy_canvas::{CanvasDetail, Detail};
use masonry::accesskit::{Node as AccessNode, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NoAction, PaintCtx, PropertiesRef, RegisterCtx, UpdateCtx,
    UsesProperty, Widget, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Rect, RoundedRect, Size, Stroke};
use masonry::layout::{LenReq, Length, SizeDef};
use masonry::peniko::Color;
use masonry::widgets::{Checkbox, Slider};

/// Height of the coloured header strip, in canvas units.
const HEADER_HEIGHT: f64 = 22.0;
/// Corner radius of the node body.
const RADIUS: f64 = 6.0;
/// Padding around the node's controls.
const PADDING: f64 = 8.0;

/// A graph node with a slider and a checkbox.
pub struct GraphNode {
    /// Header tint, used to tell nodes apart at a glance when zoomed out.
    tint: Color,
    slider: WidgetPod<Slider>,
    checkbox: WidgetPod<Checkbox>,
    /// Whether the contents are currently stashed.
    stashed: bool,
}

impl GraphNode {
    /// Creates a node with the given header tint and initial control values.
    pub fn new(tint: Color, value: f64, checked: bool) -> Self {
        Self {
            tint,
            slider: WidgetPod::new(Slider::new(0.0, 1.0, value)),
            checkbox: WidgetPod::new(Checkbox::new(checked, "on")),
            stashed: false,
        }
    }
}

// Declares that this widget reads the property, so Masonry validates the plumbing.
impl UsesProperty<CanvasDetail> for GraphNode {}

impl Widget for GraphNode {
    type Action = NoAction;

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        // The canvas always gives nodes a fixed size from the graph model, so this
        // is only a fallback. It deliberately does not measure the children.
        let fallback = match axis {
            Axis::Horizontal => 160.0,
            Axis::Vertical => 96.0,
        };
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(fallback),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, props: &PropertiesRef<'_>, size: Size) {
        let detail = props.get::<CanvasDetail>(ctx.property_cache()).0;

        // At Box detail the contents are stashed: not laid out, not painted, not
        // hit-tested. This is where LOD actually pays for itself — skipping paint
        // saves draw commands, but skipping layout saves the expensive half.
        let want_stashed = detail == Detail::Box;
        if want_stashed != self.stashed {
            self.stashed = want_stashed;
            ctx.set_stashed(&mut self.slider, want_stashed);
            ctx.set_stashed(&mut self.checkbox, want_stashed);
        }
        if want_stashed {
            return;
        }

        let inner_width = (size.width - 2.0 * PADDING).max(0.0);
        let mut y = HEADER_HEIGHT + PADDING;

        let slider_size = Size::new(inner_width, 20.0);
        let s = ctx.compute_size(&mut self.slider, SizeDef::fixed(slider_size), size.into());
        ctx.run_layout(&mut self.slider, s);
        ctx.place_child(&mut self.slider, Point::new(PADDING, y));
        y += s.height + PADDING;

        // The checkbox is only laid out at Full detail. At Simplified it stays in
        // the tree but is stashed, so the cost of a half-zoomed-out graph is the
        // header and the slider only.
        let want_controls = detail == Detail::Full;
        ctx.set_stashed(&mut self.checkbox, !want_controls);
        if want_controls {
            let cb_size = Size::new(inner_width, 20.0);
            let c = ctx.compute_size(&mut self.checkbox, SizeDef::fit(cb_size), size.into());
            ctx.run_layout(&mut self.checkbox, c);
            ctx.place_child(&mut self.checkbox, Point::new(PADDING, y));
        }
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        let detail = props.get::<CanvasDetail>(ctx.property_cache()).0;
        let box_rect = ctx.content_box();
        let body = RoundedRect::from_rect(box_rect, RADIUS);

        if detail == Detail::Box {
            // One filled shape for the whole node. At this zoom a node is a few
            // pixels across, so anything more is wasted.
            painter.fill(body, self.tint).draw();
            return;
        }

        painter.fill(body, Color::from_rgb8(0x2b, 0x2b, 0x30)).draw();

        let header = Rect::new(
            box_rect.x0,
            box_rect.y0,
            box_rect.x1,
            (box_rect.y0 + HEADER_HEIGHT).min(box_rect.y1),
        );
        painter.fill(RoundedRect::from_rect(header, RADIUS), self.tint).draw();

        painter
            .stroke(body, &Stroke::new(1.0), Color::from_rgb8(0x18, 0x18, 0x1c))
            .draw();
    }

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        CanvasDetail::prop_changed(ctx, property_type);
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.slider);
        ctx.register_child(&mut self.checkbox);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.slider.id(), self.checkbox.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut AccessNode) {}
}
