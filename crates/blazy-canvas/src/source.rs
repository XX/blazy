//! The seam between a canvas and the model behind it.

use masonry::core::{NewWidget, Widget, WidgetId};
use masonry::imaging::Painter;
use masonry::kurbo::{Point, Rect};

use crate::detail::Detail;

/// Builds the widget for a node when it scrolls into view.
///
/// The canvas materialises widgets lazily, so a node's *state* cannot live in its
/// widget: the widget does not exist most of the time. The model behind this trait
/// is the source of truth, and the widget is a view over it — which is the normal
/// arrangement for a node editor anyway, since the graph outlives any view of it.
pub trait NodeSource: 'static {
    /// Builds the widget for the node at `index`, at the given detail level.
    ///
    /// Called every time the node enters the materialised region, so it must read
    /// current state from the model rather than assuming defaults.
    ///
    /// `detail` is [`Detail::Full`] or [`Detail::Simplified`]; below that the canvas
    /// paints the node itself and never calls this. Implementations should build
    /// *fewer child widgets* at `Simplified`, not merely stash them: a stashed widget
    /// still costs a visit in every pass. A control a few pixels tall cannot be used,
    /// so it should be drawn rather than built.
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget>;

    /// Called once, when the canvas is in the widget tree, with the canvas's own id.
    ///
    /// A source usually belongs to one canvas, and several canvases over one model is
    /// the normal arrangement rather than an exotic one (§21): the same graph shown in
    /// two areas is two canvases, two sets of geometry and two widget trees over one
    /// model. Knowing which canvas it serves is what lets a source tell the *others*
    /// apart from itself when a change has to be broadcast — see [`moved`](Self::moved).
    fn attached(&mut self, canvas: WidgetId) {
        let _ = canvas;
    }

    /// The user has dragged node `index` to `pos`, and the canvas has already moved
    /// its own copy of the geometry.
    ///
    /// Where the position goes back into the model. Node geometry is *state*, and by
    /// §20.2 state lives in the model, not in the view — the widget does not exist
    /// most of the time, and neither does the canvas's copy of the graph survive a
    /// second view of it. Without this the two views of one graph drift apart on the
    /// first drag, which is exactly what happened before §30.
    ///
    /// Push into `peers` the ids of the other canvases over the same model: the canvas
    /// schedules the same move on each of them. The fan-out goes through the canvas
    /// because a source holds no widget context and cannot reach another widget; the
    /// canvas is handling an event and can. `peers` arrives empty and is a buffer the
    /// canvas reuses, so pushing into it allocates nothing after the first drag.
    fn moved(&mut self, index: usize, pos: Point, peers: &mut Vec<WidgetId>) {
        let _ = (index, pos, peers);
    }

    /// Draws the nodes that are too small to deserve widgets, all of them at once.
    ///
    /// Below the [`Detail::Box`] threshold the canvas stops materialising widgets
    /// entirely and paints the nodes itself, in one pass, into its own scene. A node
    /// a few pixels across does not need layout, hit testing, accessibility or an
    /// event route — it needs a filled rectangle, and a rectangle costs nanoseconds
    /// where a widget costs microseconds.
    ///
    /// **The whole set rather than one node at a time, and that is the point.** What a
    /// far-field frame costs is the number of *draw commands* in the recorded scene,
    /// not the geometry in them: the paint pass re-appends the scene every frame, and
    /// a command there costs fifteen times what the same rectangle costs inside a
    /// shared one (§31.1). A per-node signature forces the expensive shape and gives
    /// an implementation no way out; this one lets it group — by colour, by kind — and
    /// pay for the groups instead. The example draws six tints in six commands where
    /// it used to draw five thousand rectangles in five thousand.
    ///
    /// `nodes` is `(index, rect)` in canvas coordinates, ascending by index, and is a
    /// buffer the canvas reuses. The default draws nothing.
    ///
    /// **`scale` is how many screen pixels a canvas unit is worth** when the scene is
    /// recorded, and it is here because the other half of a far-field frame is charged
    /// in *path segments* (§32.3, §35): a rounded rectangle is eight of them and a
    /// plain one is four, so what an implementation draws at a given size is worth as
    /// much as how many commands it draws it in. The recorded scene is in canvas
    /// coordinates and survives a pan untouched (§20.6a), so this value is the scale at
    /// recording time and goes slightly stale between re-recordings — the same trade
    /// the short-link rule makes and for the same reason (§31.4).
    fn paint_far(&mut self, nodes: &[(usize, Rect)], scale: f64, painter: &mut Painter<'_>) {
        let _ = (nodes, scale, painter);
    }

    /// Whether the canvas-space `point` is inside node `index`, whose rectangle is
    /// `rect`.
    ///
    /// The canvas knows where a node is and how big it is; only the application knows
    /// what it looks like, and a node is not usually its bounding box — a rounded
    /// corner, a notch, a circular port. This is the seam: the canvas narrows the
    /// candidates down through its index and asks this about each survivor, so an
    /// implementation is called a handful of times per pick and can afford to be
    /// exact. `blazy_shape::ShapeHit` is the intended tool, kept by the implementor
    /// so that its flattened cache survives between picks.
    ///
    /// Called for nodes that have no widget as well — the far field, and anything
    /// off the materialised set — which is why it cannot be a method on the widget.
    ///
    /// The default is the rectangle, which is what the canvas would answer on its own.
    fn hit(&mut self, index: usize, rect: Rect, point: Point) -> bool {
        let _ = index;
        rect.contains(point)
    }
}

impl<F> NodeSource for F
where
    F: FnMut(usize, Detail) -> NewWidget<dyn Widget> + 'static,
{
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget> {
        self(index, detail)
    }
}
