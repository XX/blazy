//! The seam between a canvas and the model behind it.

use masonry::core::{NewWidget, Widget, WidgetId};
use masonry::imaging::Painter;
use masonry::kurbo::{Point, Rect};

use crate::detail::Detail;
use crate::links::Ports;

/// Builds the widget for a node when it scrolls into view. A node's state lives in the
/// model behind this, since the widget does not exist most of the time.
pub trait NodeSource: 'static {
    /// Builds the widget for the node at `index`, from the model's current state, every time
    /// the node enters view.
    ///
    /// `detail` is [`Detail::Full`] or [`Detail::Simplified`]. Build fewer child widgets at
    /// `Simplified` rather than stashing them: a stashed widget still costs every pass.
    fn build(&mut self, index: usize, detail: Detail) -> NewWidget<dyn Widget>;

    /// Called once, when the canvas is in the widget tree, with the canvas's own id: what a
    /// source registers its view under.
    fn attached(&mut self, canvas: WidgetId) {
        let _ = canvas;
    }

    /// The canvas's own drag moved node `index` to `pos`: write it to the model, and push
    /// into `peers` the other canvases over it, which the canvas moves too.
    fn moved(&mut self, index: usize, pos: Point, peers: &mut Vec<WidgetId>) {
        let _ = (index, pos, peers);
    }

    /// Draws the nodes too small to deserve widgets (below [`Detail::Box`]), all at once into
    /// the canvas's scene. The default draws nothing.
    ///
    /// The whole set at once so an implementation can group its fills: a far-field frame
    /// costs draw commands, not geometry. `nodes` is `(index, rect)` in canvas coordinates,
    /// ascending; `scale` is screen pixels per canvas unit when the scene is recorded.
    fn paint_far(&mut self, nodes: &[(usize, Rect)], scale: f64, painter: &mut Painter<'_>) {
        let _ = (nodes, scale, painter);
    }

    /// How many ports node `index` has on each side.
    ///
    /// Asked only when the pointer is near the node — to pick a port — and for the dots of
    /// nodes that have widgets, never per link: where a port *is* is the canvas's rule
    /// (`PortLayout`), and a link names its ports by number. The default is one input and
    /// one output, which is what a link meant before ports and what makes a link draggable
    /// out of any node of an application that never heard of ports.
    fn ports(&mut self, index: usize) -> Ports {
        let _ = index;
        Ports { inputs: 1, outputs: 1 }
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
