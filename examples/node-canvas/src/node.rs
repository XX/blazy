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
    slider: WidgetPod<Slider>,
    checkbox: WidgetPod<Checkbox>,
    /// Whether the contents are currently stashed.
    stashed: bool,
    /// The slider value this widget was built with.
    ///
    /// Kept so tests can assert that a rebuilt node picked up the model's current
    /// state rather than a stale default.
    #[cfg_attr(not(test), expect(dead_code, reason = "read only by tests"))]
    built_value: f64,
}

impl GraphNode {
    /// The slider value this node was built with.
    #[cfg(test)]
    pub fn built_value(&self) -> f64 {
        self.built_value
    }

    /// The id of this node's checkbox, for driving it from tests.
    #[cfg(test)]
    pub fn checkbox_id(&self) -> WidgetId {
        self.checkbox.id()
    }

    /// Builds the widget for node `index`, reading its current state from the model.
    pub fn build(graph: &SharedGraph, index: usize) -> NewWidget<dyn Widget> {
        let state = graph.borrow().node(index);
        NewWidget::new(Self {
            graph: graph.clone(),
            index,
            tint: state.tint,
            slider: WidgetPod::new(Slider::new(0.0, 1.0, state.value)),
            checkbox: WidgetPod::new(Checkbox::new(state.checked, "on")),
            stashed: false,
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
            self.graph.borrow_mut().set_value(self.index, moved.value);
            ctx.set_handled();
        } else if let Some(toggled) = action.downcast_ref::<CheckboxToggled>() {
            self.graph.borrow_mut().set_checked(self.index, toggled.0);
            ctx.set_handled();
        }
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
    fn build(&mut self, index: usize) -> NewWidget<dyn Widget> {
        GraphNode::build(&self.graph, index)
    }

    fn paint_far(&mut self, index: usize, rect: Rect, painter: &mut Painter<'_>) {
        // The far field: no widget, no layout, no hit route — one filled rounded
        // rect per node, straight into the canvas's own scene.
        let tint = self.graph.borrow().node(index).tint;
        painter.fill(RoundedRect::from_rect(rect, RADIUS), tint).draw();
    }
}
