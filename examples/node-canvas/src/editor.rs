//! The node editor: a canvas plus a heads-up display of the Phase 0 counters.
//!
//! The HUD exists because Phase 0 is a measurement, not a demo. Numbers that only
//! appear in a log are numbers nobody checks while dragging a node around.

use std::fmt::Write as _;
use std::mem;

use blazy_canvas::{CanvasLayer, CanvasStats};
use masonry::accesskit::{Node as AccessNode, Role};
use masonry::core::{
    AccessCtx, BrushIndex, ChildrenIds, EventCtx, LayoutCtx, MeasureCtx, NoAction, PaintCtx, PointerEvent,
    PropertiesMut, PropertiesRef, RegisterCtx, StyleProperty, Widget, WidgetMut, WidgetPod, render_text,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Affine, Axis, Point, Rect, Size};
use masonry::layout::{LenReq, Length, SizeDef};
use masonry::parley::Layout;
use masonry::peniko::Color;
use masonry::{TextAlign, TextAlignOptions};

/// Renders a canvas and overlays live statistics on top of it.
pub struct NodeEditor {
    canvas: WidgetPod<CanvasLayer>,
    /// Stats cached during layout, so `post_paint` draws numbers from this frame.
    stats: CanvasStats,
    /// The HUD text currently on screen.
    hud: String,
    /// Scratch buffer the next HUD text is formatted into, reused between frames.
    hud_next: String,
    /// The HUD text, shaped.
    ///
    /// Shaping three lines of text costs more than everything else this widget does,
    /// so it must not happen per frame. It is redone only when [`Self::hud`] actually
    /// changes, which during a steady pan is almost never.
    hud_layout: Option<Layout<BrushIndex>>,
}

impl NodeEditor {
    /// Statistics from the canvas, as of the last layout pass.
    pub fn stats(&self) -> CanvasStats {
        self.stats
    }

    /// Runs a callback with a `WidgetMut` for the inner canvas.
    ///
    /// The canvas is reached through a context rather than a field because a
    /// `WidgetPod` hands its widget to the arena once inserted.
    pub fn with_canvas<R>(this: &mut WidgetMut<'_, Self>, f: impl FnOnce(WidgetMut<'_, CanvasLayer>) -> R) -> R {
        let canvas = this.ctx.get_mut(&mut this.widget.canvas);
        f(canvas)
    }

    /// Wraps a canvas in an editor with a HUD.
    pub fn new(canvas: CanvasLayer) -> Self {
        Self {
            canvas: WidgetPod::new(canvas),
            stats: CanvasStats::default(),
            hud: String::new(),
            hud_next: String::new(),
            hud_layout: None,
        }
    }
}

/// Formats the HUD into `out`, reusing its allocation.
///
/// Deliberately shows only quantities a human watches: how much of the graph is
/// materialised, the zoom, the detail level, and how many widgets have been built.
/// The cumulative pass counters live in [`CanvasStats`] for the benchmark; putting
/// them on screen would change the text every frame and defeat the cache.
fn format_hud(stats: &CanvasStats, out: &mut String) {
    let detail = match stats.detail {
        Some(detail) => detail.as_str(),
        None => "-",
    };
    out.clear();
    write!(
        out,
        "nodes {visible}/{total} materialised   zoom {zoom:.2}x   lod {detail}   built {builds}\n\
         drag a node - left-drag empty space or middle-drag to pan - wheel to zoom",
        visible = stats.materialised,
        total = stats.total,
        zoom = stats.zoom,
        builds = stats.counters.builds,
    )
    .ok();
}

impl Widget for NodeEditor {
    type Action = NoAction;

    fn on_pointer_event(&mut self, ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, event: &PointerEvent) {
        // Any pointer activity may have moved, zoomed or dragged something, so the
        // HUD needs redrawing. The numbers themselves are refreshed in `layout`,
        // which runs before paint, so what gets drawn is this frame's data.
        if matches!(
            event,
            PointerEvent::Move(_) | PointerEvent::Scroll(_) | PointerEvent::Down(_) | PointerEvent::Up(_)
        ) {
            ctx.request_post_paint();
        }
    }

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        let fallback = match axis {
            Axis::Horizontal => 1100.0,
            Axis::Vertical => 750.0,
        };
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(fallback),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        let canvas_size = ctx.compute_size(&mut self.canvas, SizeDef::fixed(size), size.into());
        ctx.run_layout(&mut self.canvas, canvas_size);
        ctx.place_child(&mut self.canvas, Point::ORIGIN);

        // Read the canvas counters back after its layout has run.
        let (canvas, _) = ctx.get_raw(&mut self.canvas);
        let stats = canvas.stats();
        self.stats = stats;

        // Format into the scratch buffer and only swap when the text really changed.
        // Formatting is cheap; shaping is not, so the point is to keep `hud_layout`
        // valid for as long as possible.
        format_hud(&stats, &mut self.hud_next);
        if self.hud_next != self.hud {
            mem::swap(&mut self.hud, &mut self.hud_next);
            self.hud_layout = None;
        }
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        painter
            .fill(ctx.content_box(), Color::from_rgb8(0x1c, 0x1c, 0x20))
            .draw();
    }

    fn post_paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        let content_box = ctx.content_box();
        let panel = Rect::new(content_box.x0, content_box.y1 - 46.0, content_box.x1, content_box.y1);
        painter.fill(panel, Color::from_rgba8(0x10, 0x10, 0x14, 0xd0)).draw();

        if self.hud_layout.is_none() {
            let text = &self.hud;
            let (fcx, lcx) = ctx.text_contexts();
            let mut builder = lcx.ranged_builder(fcx, text, 1.0, true);
            builder.push_default(StyleProperty::FontSize(12.0));
            let mut layout = builder.build(text);
            layout.break_all_lines(None);
            layout.align(None, TextAlign::Start, TextAlignOptions::default());
            self.hud_layout = Some(layout);
        }
        let Some(layout) = self.hud_layout.as_ref() else {
            return;
        };

        render_text(
            painter,
            Affine::translate((panel.x0 + 10.0, panel.y0 + 8.0)),
            layout,
            &[Color::from_rgb8(0xd0, 0xd0, 0xd8).into()],
            true,
        );
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.canvas);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.canvas.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut AccessNode) {}
}
