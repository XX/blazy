//! A graph node: a rounded body with a coloured header and interactive controls.
//!
//! Its slider and checkbox are stock Masonry widgets, and keep working when the canvas is
//! zoomed. At lower detail it builds fewer children. It is picked by its rounded shape,
//! both as a widget (`find_widget_under_pointer`) and by the canvas ([`GraphSource::hit`]).

use std::any::TypeId;

use blazy::canvas::{CanvasDetail, CanvasLayer, Detail, NodeSource, PortLayout, Ports};
use blazy::masonry::accesskit::{Node as AccessNode, Role};
use blazy::masonry::core::{
    AccessCtx, ActionCtx, ChildrenIds, ErasedAction, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx,
    PropertiesMut, PropertiesRef, QueryCtx, RegisterCtx, UpdateCtx, UsesProperty, Widget, WidgetId, WidgetPod,
    WidgetRef,
};
use blazy::masonry::imaging::Painter;
use blazy::masonry::kurbo::{Axis, BezPath, Point, Rect, RoundedRect, Shape, Size, Stroke};
use blazy::masonry::layout::{LenReq, Length, SizeDef};
use blazy::masonry::peniko::Color;
use blazy::masonry::properties::BorderColor;
use blazy::masonry::widgets::{Checkbox, CheckboxToggled, Slider, SliderMoved};
use blazy::node_editor::{Change, ViewToken};
use blazy::shape::ShapeHit;

use crate::model::SharedGraph;

/// Height of the coloured header strip, in canvas units.
const HEADER_HEIGHT: f64 = 22.0;
/// Corner radius of the node body.
const RADIUS: f64 = 6.0;
/// Padding around the node's controls.
const PADDING: f64 = 8.0;
/// Flattening tolerance for the far-field rounded corners, in canvas units: generous, a
/// far-field corner is a fraction of a pixel.
const FAR_TOLERANCE: f64 = 1.0;
/// How large a corner must be on screen, in pixels, before the far field rounds it
/// (a rounded rectangle is eight path segments, a plain one four).
pub(crate) const FAR_MIN_RADIUS_PX: f64 = 0.5;

/// A graph node with a slider and a checkbox: a view over
/// [`GraphModel`](crate::model::GraphModel), which every edit is written back to at once.
pub struct GraphNode {
    /// The graph this node belongs to.
    graph: SharedGraph,
    /// The canvas this node was built for, which an edit is not broadcast back to.
    canvas: Option<WidgetId>,
    /// This node's index in the graph.
    index: usize,
    /// Header tint, used to tell nodes apart at a glance when zoomed out.
    tint: Color,
    /// Interactive controls, present only at [`Detail::Full`]; below, the values are painted.
    controls: Option<Controls>,
    /// The slider value, kept for painting when there is no slider widget.
    value: f64,
    /// The checkbox state, kept for painting when there is no checkbox widget.
    checked: bool,
    /// The slider value this widget was built with (for tests).
    #[cfg_attr(not(test), expect(dead_code, reason = "read only by tests"))]
    built_value: f64,
    /// The body as a hit shape, rebuilt in `layout` when the size changes (it caches).
    hit: ShapeHit,
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

    /// The slider value this node is currently showing.
    pub fn value(&self) -> f64 {
        self.value
    }

    /// The checkbox state this node is currently showing.
    pub fn checked(&self) -> bool {
        self.checked
    }

    /// The id of this node's checkbox, for driving it from a test or a script.
    pub fn checkbox_id(&self) -> Option<WidgetId> {
        self.controls.as_ref().map(|c| c.checkbox.id())
    }

    /// Builds the widget for node `index`, reading its current state from the model.
    pub fn build(graph: &SharedGraph, canvas: Option<WidgetId>, index: usize, detail: Detail) -> NewWidget<dyn Widget> {
        let state = graph.borrow().node(index);
        let controls = (detail == Detail::Full).then(|| Controls {
            slider: WidgetPod::new(Slider::new(0.0, 1.0, state.value)),
            checkbox: WidgetPod::new(Checkbox::new(state.checked, "on")),
        });
        NewWidget::new(Self {
            graph: graph.clone(),
            canvas,
            index,
            tint: state.tint,
            controls,
            value: state.value,
            checked: state.checked,
            built_value: state.value,
            hit: body_shape(Size::ZERO),
        })
        .erased()
    }
}

impl GraphNode {
    /// Pushes this node's new state into every other view of the same graph (a node widget
    /// keeps its own copy of the model's values).
    fn broadcast(&self, ctx: &mut ActionCtx<'_>) {
        // This node first: Masonry's `Checkbox` does not toggle itself.
        ctx.mutate_self_later(|mut widget| Self::reload(&mut widget.downcast::<Self>()));

        let mut peers = Vec::new();
        let index = self.index;
        {
            let graph = self.graph.borrow();
            let views = graph.views();
            match self.canvas {
                // For every view but this one, or the slider being dragged is rebuilt.
                Some(canvas) => {
                    views.note_except(canvas, Change::Contents { index });
                    views.others(canvas, &mut peers);
                },
                None => {
                    views.note(Change::Contents { index });
                    peers.extend(views.ids());
                },
            }
        }
        for peer in peers {
            // Other canvases through the mutate pass, in the same frame.
            ctx.mutate_later(peer, move |mut widget| {
                let mut canvas = widget.downcast::<CanvasLayer>();
                CanvasLayer::update_child(&mut canvas, index, |mut node| {
                    let mut node = node.downcast::<Self>();
                    Self::reload(&mut node);
                });
            });
        }
    }

    /// Re-reads this node's state from the model: the receiving half of the push an edit
    /// starts. (Another window rebuilds the node instead, through `Change::Contents`.)
    pub fn reload(this: &mut blazy::masonry::core::WidgetMut<'_, Self>) {
        let state = this.widget.graph.borrow().node(this.widget.index);
        this.widget.value = state.value;
        this.widget.checked = state.checked;
        this.widget.tint = state.tint;
        if let Some(controls) = this.widget.controls.as_mut() {
            {
                let mut slider = this.ctx.get_mut(&mut controls.slider);
                Slider::set_value(&mut slider, state.value);
            }
            let mut checkbox = this.ctx.get_mut(&mut controls.checkbox);
            Checkbox::set_checked(&mut checkbox, state.checked);
        }
        // The painted stand-ins need a repaint of their own.
        this.ctx.request_paint_only();
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
        // A fallback: the canvas gives nodes their size.
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
        // The hit shape follows the size: the same rounded rectangle `paint` fills.
        if self.hit.bounds().size() != size {
            self.hit = body_shape(size);
        }

        // Below Full there are no controls to lay out.
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
        // The global level: how many nodes share the cost of a command.
        let global = props.get::<CanvasDetail>(ctx.property_cache()).0;
        let box_rect = ctx.content_box();
        let body = RoundedRect::from_rect(box_rect, RADIUS);

        painter.fill(body, Color::from_rgb8(0x2b, 0x2b, 0x30)).draw();

        let header = Rect::new(
            box_rect.x0,
            box_rect.y0,
            box_rect.x1,
            (box_rect.y0 + HEADER_HEIGHT).min(box_rect.y1),
        );
        painter.fill(RoundedRect::from_rect(header, RADIUS), self.tint).draw();

        // The outline colour comes from the style for the selected class (`property_set`).
        let outline = props.get::<BorderColor>(ctx.property_cache()).color;
        if outline.components[3] > 0.0 {
            painter
                .stroke(
                    body.rect().inset(-1.0).to_rounded_rect(RADIUS + 1.0),
                    &Stroke::new(2.0),
                    outline,
                )
                .draw();
        }

        // Without control widgets the state is painted where the controls would be, in
        // more detail only when few nodes are on screen.
        if self.controls.is_none() {
            let detailed = global == Detail::Full;
            let inner_width = (box_rect.width() - 2.0 * PADDING).max(0.0);
            let track = Rect::from_origin_size(
                (box_rect.x0 + PADDING, box_rect.y0 + HEADER_HEIGHT + PADDING + 8.0),
                Size::new(inner_width, 4.0),
            );
            painter.fill(track, Color::from_rgb8(0x3a, 0x3a, 0x42)).draw();
            painter
                .fill(
                    Rect::from_origin_size(track.origin(), Size::new(inner_width * self.value, 4.0)),
                    Color::from_rgb8(0x6a, 0x6a, 0x88),
                )
                .draw();
            if detailed {
                // Few nodes on screen: the knob and the checkbox too.
                let knob_x = track.x0 + inner_width * self.value;
                painter
                    .fill(
                        Rect::new(knob_x - 4.0, track.y0 - 4.0, knob_x + 4.0, track.y1 + 4.0),
                        Color::from_rgb8(0xc8, 0xc8, 0xd4),
                    )
                    .draw();

                let cb =
                    Rect::from_origin_size((box_rect.x0 + PADDING, track.y1 + PADDING + 3.0), Size::new(12.0, 12.0));
                painter.fill(cb, Color::from_rgb8(0x3a, 0x3a, 0x42)).draw();
                if self.checked {
                    painter.fill(cb.inset(-3.0), Color::from_rgb8(0xc8, 0xc8, 0xd4)).draw();
                }
            }
        }

        painter
            .stroke(body, &Stroke::new(1.0), Color::from_rgb8(0x18, 0x18, 0x1c))
            .draw();
    }

    /// Writes control changes straight back into the model.
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
            self.broadcast(ctx);
            ctx.set_handled();
        } else if let Some(toggled) = action.downcast_ref::<CheckboxToggled>() {
            self.checked = toggled.0;
            self.graph.borrow_mut().set_checked(self.index, toggled.0);
            self.broadcast(ctx);
            ctx.set_handled();
        }
    }

    /// The precise half of the hit test: the corners are rounded away.
    fn find_widget_under_pointer<'c>(&'c self, ctx: QueryCtx<'c>, pos: Point) -> Option<WidgetRef<'c, dyn Widget>> {
        self.hit.find_widget(self, ctx, pos)
    }

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        CanvasDetail::prop_changed(ctx, property_type);
        // `paint` draws the outline, and Masonry repaints only the box for `BorderColor`.
        if property_type == TypeId::of::<BorderColor>() {
            ctx.request_paint_only();
        }
    }

    /// The port dots, drawn by the node so that they sit at its level among the nodes: a
    /// node raised over this one covers them, and a corner it leaves unpainted shows them
    ///. After the controls, which reach close to the edge the dots sit on.
    fn post_paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        PORTS.paint_ports(painter, ctx.border_box(), NODE_PORTS);
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

/// The node body as a hit shape: the rounded rectangle `paint` fills.
fn body_shape(size: Size) -> ShapeHit {
    ShapeHit::fill(RoundedRect::from_rect(
        Rect::from_origin_size(Point::ORIGIN, size),
        RADIUS,
    ))
}

/// Builds and draws nodes for the canvas.
pub struct GraphSource {
    graph: SharedGraph,
    /// The canvas this source builds for, once it is in the tree.
    canvas: Option<WidgetId>,
    /// This canvas's place among the graph's views, given up when the canvas is dropped.
    view: Option<ViewToken>,
    /// One shape for every node (all the same size), kept for its flattened cache.
    body: ShapeHit,
    /// How large a corner must be on screen before the far field rounds it.
    far_min_radius_px: f64,
    /// One reusable path per tint, for the far field. See [`NodeSource::paint_far`].
    far_batches: Vec<(Color, BezPath)>,
}

impl GraphSource {
    /// Creates a source over the given graph.
    pub fn new(graph: SharedGraph) -> Self {
        Self {
            graph,
            canvas: None,
            view: None,
            body: body_shape(crate::model::NODE_SIZE),
            far_min_radius_px: FAR_MIN_RADIUS_PX,
            far_batches: Vec::new(),
        }
    }

    /// Sets how large a corner must be on screen before the far field rounds it.
    #[must_use]
    pub fn with_far_min_radius(mut self, px: f64) -> Self {
        self.far_min_radius_px = px;
        self
    }
}

/// Where a node's ports are: in line with its controls, an input beside each and the
/// output beside the first.
pub const PORTS: PortLayout = PortLayout {
    first: Some(HEADER_HEIGHT + PADDING + 10.0),
    step: 20.0 + PADDING,
    dot_radius: 4.0,
    dot_color: Color::from_rgb8(0x9a, 0x9a, 0xa8),
};

/// How many ports every node has: the canvas asks the source, the node draws them.
const NODE_PORTS: Ports = Ports {
    inputs: crate::model::INPUTS,
    outputs: crate::model::OUTPUTS,
};

impl NodeSource for GraphSource {
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget> {
        GraphNode::build(&self.graph, self.canvas, index, detail)
    }

    /// Registers this canvas as one of the graph's views.
    fn attached(&mut self, canvas: WidgetId) {
        self.canvas = Some(canvas);
        self.view = Some(self.graph.borrow().views().attach(canvas));
    }

    /// Records the canvas's own drag in the model, and names the other views to follow.
    fn moved(&mut self, index: usize, pos: Point, peers: &mut Vec<WidgetId>) {
        self.graph.borrow_mut().set_pos(index, pos);
        let graph = self.graph.borrow();
        let views = graph.views();
        match self.canvas {
            Some(canvas) => {
                views.note_except(canvas, Change::Moved { index, pos });
                views.others(canvas, peers);
            },
            None => {
                views.note(Change::Moved { index, pos });
                peers.extend(views.ids());
            },
        }
    }

    fn ports(&mut self, _index: usize) -> Ports {
        NODE_PORTS
    }

    fn paint_far(&mut self, nodes: &[(usize, Rect)], scale: f64, painter: &mut Painter<'_>) {
        // Grouped by tint and filled once per group: six commands rather than one per node.
        let graph = self.graph.borrow();
        let rounded = RADIUS * scale >= self.far_min_radius_px;
        let mut batches = std::mem::take(&mut self.far_batches);
        for (_, path) in &mut batches {
            path.truncate(0);
        }
        for &(index, rect) in nodes {
            let tint = graph.node(index).tint;
            let batch = match batches.iter().position(|(colour, _)| *colour == tint) {
                Some(at) => &mut batches[at],
                None => {
                    batches.push((tint, BezPath::new()));
                    batches.last_mut().expect("just pushed")
                },
            };
            // Each node a subpath; rounded only while the corner is visible on screen.
            if rounded {
                batch
                    .1
                    .extend(RoundedRect::from_rect(rect, RADIUS).path_elements(FAR_TOLERANCE));
            } else {
                batch.1.extend(rect.path_elements(FAR_TOLERANCE));
            }
        }
        for (tint, path) in &batches {
            if !path.is_empty() {
                painter.fill(path, *tint).draw();
            }
        }
        self.far_batches = batches;
    }

    fn hit(&mut self, _index: usize, rect: Rect, point: Point) -> bool {
        // One shape for all nodes: the point moves into its frame.
        if rect.size() != crate::model::NODE_SIZE {
            return rect.contains(point);
        }
        // Scale is irrelevant for a fill: only a stroke has a tolerance to convert.
        self.body.contains(point - rect.origin().to_vec2(), 1.0)
    }
}
