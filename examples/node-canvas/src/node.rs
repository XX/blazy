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

use blazy_canvas::{CanvasDetail, Detail, NodeSource};
use masonry::accesskit::{Node as AccessNode, Role};
use masonry::core::{
    AccessCtx, ActionCtx, ChildrenIds, ErasedAction, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx,
    PropertiesMut, PropertiesRef, RegisterCtx, UpdateCtx, UsesProperty, Widget, WidgetId, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Rect, RoundedRect, Size, Stroke};
use masonry::layout::{LenReq, Length, SizeDef};
use masonry::peniko::Color;
use masonry::widgets::{Checkbox, CheckboxToggled, Slider, SliderMoved};

use crate::model::SharedGraph;

/// Height of the coloured header strip, in canvas units.
const HEADER_HEIGHT: f64 = 22.0;
/// Corner radius of the node body.
const RADIUS: f64 = 6.0;
/// Padding around the node's controls.
const PADDING: f64 = 8.0;

/// A graph node with a slider and a checkbox.
///
/// The node is a *view* over [`GraphModel`](crate::model::GraphModel): it is built
/// when the node scrolls into view and dropped when it scrolls out, so anything the
/// user changes has to be written back to the model immediately. That write-back is
/// [`on_action`](Widget::on_action).
pub struct GraphNode {
    /// The graph this node belongs to.
    graph: SharedGraph,
    /// This node's index in the graph.
    index: usize,
    /// Header tint, used to tell nodes apart at a glance when zoomed out.
    tint: Color,
    /// Interactive controls, present only at [`Detail::Full`].
    ///
    /// At `Simplified` a node is roughly 50 px wide: the slider would be 3 px tall and
    /// unusable. Building it anyway would cost three extra widgets per node in every
    /// pass, for something the user cannot touch — so the value is painted instead.
    controls: Option<Controls>,
    /// The slider value, kept for painting when there is no slider widget.
    value: f64,
    /// The checkbox state, kept for painting when there is no checkbox widget.
    checked: bool,
    /// The slider value this widget was built with.
    ///
    /// Kept so tests can assert that a rebuilt node picked up the model's current
    /// state rather than a stale default.
    #[cfg_attr(not(test), expect(dead_code, reason = "read only by tests"))]
    built_value: f64,
}

/// The interactive half of a node, built only at [`Detail::Full`].
struct Controls {
    slider: WidgetPod<Slider>,
    checkbox: WidgetPod<Checkbox>,
}

impl GraphNode {
    /// The slider value this node was built with.
    #[cfg(test)]
    pub fn built_value(&self) -> f64 {
        self.built_value
    }

    /// The id of this node's checkbox, for driving it from tests.
    #[cfg(test)]
    pub fn checkbox_id(&self) -> Option<WidgetId> {
        self.controls.as_ref().map(|c| c.checkbox.id())
    }

    /// Builds the widget for node `index`, reading its current state from the model.
    pub fn build(graph: &SharedGraph, index: usize, detail: Detail) -> NewWidget<dyn Widget> {
        let state = graph.borrow().node(index);
        let controls = (detail == Detail::Full).then(|| Controls {
            slider: WidgetPod::new(Slider::new(0.0, 1.0, state.value)),
            checkbox: WidgetPod::new(Checkbox::new(state.checked, "on")),
        });
        NewWidget::new(Self {
            graph: graph.clone(),
            index,
            tint: state.tint,
            controls,
            value: state.value,
            checked: state.checked,
            built_value: state.value,
        })
        .erased()
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

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        // Whether this node has controls was decided when it was built, not here.
        // Below Full it has none at all, so there is nothing to stash and nothing to
        // lay out — which is the entire saving.
        let Some(controls) = self.controls.as_mut() else {
            return;
        };

        let inner_width = (size.width - 2.0 * PADDING).max(0.0);
        let mut y = HEADER_HEIGHT + PADDING;

        let slider_size = Size::new(inner_width, 20.0);
        ctx.run_layout(&mut controls.slider, slider_size);
        ctx.place_child(&mut controls.slider, Point::new(PADDING, y));
        y += slider_size.height + PADDING;

        let cb_size = Size::new(inner_width, 20.0);
        let c = ctx.compute_size(&mut controls.checkbox, SizeDef::fit(cb_size), size.into());
        ctx.run_layout(&mut controls.checkbox, c);
        ctx.place_child(&mut controls.checkbox, Point::new(PADDING, y));
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

        // Without controls the node still has to show its state, so the slider and
        // checkbox become two rectangles. Two draw commands replace three widgets.
        if self.controls.is_none() {
            let inner_width = (box_rect.width() - 2.0 * PADDING).max(0.0);
            let bar = Rect::from_origin_size(
                (box_rect.x0 + PADDING, box_rect.y0 + HEADER_HEIGHT + PADDING),
                Size::new(inner_width, 6.0),
            );
            painter.fill(bar, Color::from_rgb8(0x3a, 0x3a, 0x42)).draw();
            painter
                .fill(
                    Rect::from_origin_size(bar.origin(), Size::new(inner_width * self.value, 6.0)),
                    Color::from_rgb8(0x9a, 0x9a, 0xb0),
                )
                .draw();

            if self.checked {
                let mark = Rect::from_origin_size((box_rect.x0 + PADDING, bar.y1 + PADDING), Size::new(8.0, 8.0));
                painter.fill(mark, Color::from_rgb8(0x9a, 0x9a, 0xb0)).draw();
            }
        }

        painter
            .stroke(body, &Stroke::new(1.0), Color::from_rgb8(0x18, 0x18, 0x1c))
            .draw();
    }

    /// Writes control changes straight back into the model.
    ///
    /// Without this, virtualisation would silently discard the user's edits the
    /// moment a node scrolled off screen.
    fn on_action(
        &mut self,
        ctx: &mut ActionCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        action: &ErasedAction,
        _source: WidgetId,
    ) {
        if let Some(moved) = action.downcast_ref::<SliderMoved>() {
            self.value = moved.value;
            self.graph.borrow_mut().set_value(self.index, moved.value);
            ctx.set_handled();
        } else if let Some(toggled) = action.downcast_ref::<CheckboxToggled>() {
            self.checked = toggled.0;
            self.graph.borrow_mut().set_checked(self.index, toggled.0);
            ctx.set_handled();
        }
    }

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        CanvasDetail::prop_changed(ctx, property_type);
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        if let Some(controls) = self.controls.as_mut() {
            ctx.register_child(&mut controls.slider);
            ctx.register_child(&mut controls.checkbox);
        }
    }

    fn children_ids(&self) -> ChildrenIds {
        match self.controls.as_ref() {
            Some(c) => ChildrenIds::from_slice(&[c.slider.id(), c.checkbox.id()]),
            None => ChildrenIds::new(),
        }
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut AccessNode) {}
}

/// Builds and draws nodes for the canvas.
///
/// A struct rather than a closure because the canvas needs two things from it: a
/// widget when the node is big enough to interact with, and a rectangle when it is
/// not. See [`NodeSource::paint_far`].
pub struct GraphSource {
    graph: SharedGraph,
}

impl GraphSource {
    /// Creates a source over the given graph.
    pub fn new(graph: SharedGraph) -> Self {
        Self { graph }
    }
}

impl NodeSource for GraphSource {
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget> {
        GraphNode::build(&self.graph, index, detail)
    }

    fn paint_far(&mut self, index: usize, rect: Rect, painter: &mut Painter<'_>) {
        // The far field: no widget, no layout, no hit route — one filled rounded
        // rect per node, straight into the canvas's own scene.
        let tint = self.graph.borrow().node(index).tint;
        painter.fill(RoundedRect::from_rect(rect, RADIUS), tint).draw();
    }
}
