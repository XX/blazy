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
//!
//! It is drawn as a rounded rectangle, so it is picked as one too, twice over: the
//! widget overrides `find_widget_under_pointer` with a [`ShapeHit`], and
//! [`GraphSource::hit`] answers the same question for the canvas, which needs it for
//! nodes that have no widget at all. Both go through the same shape — a corner the
//! eye sees as empty has to be empty to the pointer as well.

use std::any::TypeId;

use blazy_canvas::{CanvasDetail, CanvasLayer, Detail, NodeSource};
use blazy_shape::ShapeHit;
use masonry::accesskit::{Node as AccessNode, Role};
use masonry::core::{
    AccessCtx, ActionCtx, ChildrenIds, ErasedAction, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx,
    PropertiesMut, PropertiesRef, QueryCtx, RegisterCtx, UpdateCtx, UsesProperty, Widget, WidgetId, WidgetPod,
    WidgetRef,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, BezPath, Point, Rect, RoundedRect, Shape, Size, Stroke};
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
/// Flattening tolerance for the far-field rounded corners, in canvas units.
///
/// Generous on purpose: in the far field a whole node is a few pixels across, so its
/// corner arc is a fraction of one, and the elements this saves are elements the
/// batch would carry in every frame.
const FAR_TOLERANCE: f64 = 1.0;

/// A graph node with a slider and a checkbox.
///
/// The node is a *view* over [`GraphModel`](crate::model::GraphModel): it is built
/// when the node scrolls into view and dropped when it scrolls out, so anything the
/// user changes has to be written back to the model immediately. That write-back is
/// [`on_action`](Widget::on_action).
pub struct GraphNode {
    /// The graph this node belongs to.
    graph: SharedGraph,
    /// The canvas this node was built for.
    ///
    /// Needed to tell the *other* views of the same graph apart from this one when an
    /// edit is broadcast: re-applying an edit to the node the user is currently
    /// dragging would be work at best and a lost pointer grip at worst.
    canvas: Option<WidgetId>,
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
    /// The body as a hit shape, rebuilt in `layout` when the size changes.
    ///
    /// Rebuilt there rather than derived per pick because a `ShapeHit` carries a
    /// flattened cache, and rebuilding it on every pointer move would throw the cache
    /// away exactly when it pays (§25.1).
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
    /// Pushes this node's new state into every other view of the same graph.
    ///
    /// Writing the model is not enough on its own: a node widget reads the model when
    /// it is *built* and then keeps its own copy, because it is painted far more often
    /// than it is built. A view already showing this node therefore has to be told.
    /// Views that are not showing it need nothing — they will read the model when the
    /// node next scrolls in.
    fn broadcast(&self, ctx: &mut ActionCtx<'_>) {
        // This node first. Masonry's `Checkbox` deliberately does not toggle itself —
        // it emits `CheckboxToggled` and leaves the state to whoever owns the source of
        // truth — so the same reload that carries the edit to the other views is also
        // what makes this one show it.
        ctx.mutate_self_later(|mut widget| Self::reload(&mut widget.downcast::<Self>()));

        let mut peers = Vec::new();
        self.graph.borrow().other_views(self.canvas, &mut peers);
        let index = self.index;
        for peer in peers {
            // The mutate pass is where a widget outside this subtree may legally be
            // changed, and it runs before the next layout — so the other areas show the
            // new value in the same frame.
            ctx.mutate_later(peer, move |mut widget| {
                let mut canvas = widget.downcast::<CanvasLayer>();
                CanvasLayer::update_child(&mut canvas, index, |mut node| {
                    let mut node = node.downcast::<Self>();
                    Self::reload(&mut node);
                });
            });
        }
    }

    /// Re-reads this node's state from the model.
    ///
    /// The receiving half of [`broadcast`](Self::broadcast). Also the reason a node
    /// keeps `value` and `checked` of its own: at `Simplified` there are no control
    /// widgets and the values are painted, so both copies have to be refreshed.
    fn reload(this: &mut masonry::core::WidgetMut<'_, Self>) {
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
        // The painted stand-ins are this widget's own drawing, so they need a repaint
        // of their own even when the control widgets asked for theirs.
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
        // The hit shape follows the size, and it is the *drawn* shape: `paint` fills
        // the same rounded rectangle. Two places deriving a body from the size is one
        // too many, but the alternative — rebuilding the shape inside the hit test —
        // throws away the flattened cache on every pointer move.
        if self.hit.bounds().size() != size {
            self.hit = body_shape(size);
        }

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
        // The *global* level, not this node's: it says how many nodes are on screen,
        // and therefore how much a single extra draw command costs.
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

        // Without control widgets the node still has to show its state. The stand-in
        // sits where the real controls would, because it is swapped for them the
        // moment the pointer arrives: a mismatch here would read as the UI jumping
        // under the cursor.
        //
        // Fidelity is bought only where it is affordable. Every node's scene is
        // appended into the layer scene on every frame, so one extra command here is
        // multiplied by the number of visible nodes.
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
                // Few nodes on screen: draw the knob and the checkbox, so that
                // swapping in the real controls on hover is not visible.
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
            self.broadcast(ctx);
            ctx.set_handled();
        } else if let Some(toggled) = action.downcast_ref::<CheckboxToggled>() {
            self.checked = toggled.0;
            self.graph.borrow_mut().set_checked(self.index, toggled.0);
            self.broadcast(ctx);
            ctx.set_handled();
        }
    }

    /// The precise half of the two-phase hit test (`rnd/architecture.md` §6.2).
    ///
    /// Masonry has already checked the bounding box by the time this runs; what it
    /// cannot know is that the corners are rounded away. Without this a click three
    /// pixels into the corner of a node lands on the node instead of on whatever is
    /// behind it — a link, or the empty canvas that starts a pan.
    fn find_widget_under_pointer<'c>(&'c self, ctx: QueryCtx<'c>, pos: Point) -> Option<WidgetRef<'c, dyn Widget>> {
        self.hit.find_widget(self, ctx, pos)
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

/// The node body as a hit shape.
///
/// The same rounded rectangle `paint` fills. One function, so the two cannot drift
/// apart: a shape that is picked where it is not drawn is a bug nobody sees until
/// they click.
fn body_shape(size: Size) -> ShapeHit {
    ShapeHit::fill(RoundedRect::from_rect(
        Rect::from_origin_size(Point::ORIGIN, size),
        RADIUS,
    ))
}

/// Builds and draws nodes for the canvas.
///
/// A struct rather than a closure because the canvas needs three things from it: a
/// widget when the node is big enough to interact with, a rectangle when it is not,
/// and the answer to "is this point on this node" for both cases. See
/// [`NodeSource::paint_far`] and [`NodeSource::hit`].
pub struct GraphSource {
    graph: SharedGraph,
    /// The canvas this source builds for, once it is in the tree.
    canvas: Option<WidgetId>,
    /// One shape for every node, because every node is the same size.
    ///
    /// Kept here rather than built per pick: the flattened cache inside it is what
    /// makes the exact test 15x cheaper than re-walking the path (§25.1), and it only
    /// pays if it survives between picks.
    body: ShapeHit,
    /// One reusable path per tint, for the far field. See [`NodeSource::paint_far`].
    far_batches: Vec<(Color, BezPath)>,
}

impl GraphSource {
    /// Creates a source over the given graph.
    pub fn new(graph: SharedGraph) -> Self {
        Self {
            graph,
            canvas: None,
            body: body_shape(crate::model::NODE_SIZE),
            far_batches: Vec::new(),
        }
    }
}

impl NodeSource for GraphSource {
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget> {
        GraphNode::build(&self.graph, self.canvas, index, detail)
    }

    /// Registers this canvas as one of the graph's views.
    ///
    /// Done here rather than at construction because a canvas has no id until it is in
    /// the tree, and doing it here keeps every constructor in this crate unchanged: a
    /// canvas over a shared graph is a view of it by the fact of existing.
    fn attached(&mut self, canvas: WidgetId) {
        self.canvas = Some(canvas);
        self.graph.borrow_mut().register_view(canvas);
    }

    /// Records a drag in the model and names the other views that have to follow it.
    fn moved(&mut self, index: usize, pos: Point, peers: &mut Vec<WidgetId>) {
        self.graph.borrow_mut().set_pos(index, pos);
        self.graph.borrow().other_views(self.canvas, peers);
    }

    fn paint_far(&mut self, nodes: &[(usize, Rect)], painter: &mut Painter<'_>) {
        // The far field: no widget, no layout, no widget-tree hit route — the nodes
        // are shapes in the canvas's own scene. Picking still works, because the
        // canvas asks `hit` rather than the tree.
        //
        // Grouped by tint and filled once per group. The generated graph has six
        // tints, so five thousand nodes cost six commands instead of five thousand,
        // and that is the difference between 5.27 ms and 1.40 ms a frame — a command
        // is charged in every frame the scene is appended, which is every frame (§31).
        // The grouping is a linear scan because six is the number: a map would cost
        // more than it saves.
        let graph = self.graph.borrow();
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
            // Each node is its own subpath, so the fill treats them as separate
            // shapes; `move_to` is what keeps them from being joined up.
            batch
                .1
                .extend(RoundedRect::from_rect(rect, RADIUS).path_elements(FAR_TOLERANCE));
        }
        for (tint, path) in &batches {
            if !path.is_empty() {
                painter.fill(path, *tint).draw();
            }
        }
        self.far_batches = batches;
    }

    fn hit(&mut self, _index: usize, rect: Rect, point: Point) -> bool {
        // Every generated node has the same size, so one shape serves them all: the
        // point moves into the shape's frame instead of the shape moving to the node.
        // A graph with per-node sizes would keep a shape per size, not per node.
        if rect.size() != crate::model::NODE_SIZE {
            return rect.contains(point);
        }
        // Scale is irrelevant for a fill: only a stroke has a tolerance to convert.
        self.body.contains(point - rect.origin().to_vec2(), 1.0)
    }
}
