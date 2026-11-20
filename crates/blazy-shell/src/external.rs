//! A widget that reserves its rectangle for the host to draw.
//!
//! `VisualLayerKind::External` marks content that is not Masonry's — a 3D viewport.
//!
//! `paint_layer_mode` is reset every paint pass and only the widget's own `paint` sets it
//! again, so the widget repaints every frame (an animation frame each), and the window
//! cannot idle while it is on screen. The proper fix is upstream: a property of the
//! widget rather than a flag of the pass.

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NoAction, PaintCtx, PaintLayerMode, PropertiesMut, PropertiesRef,
    RegisterCtx, UpdateCtx, Widget,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Size};
use masonry::layout::{LenReq, Length};

/// Reserves a rectangle for content the host draws itself.
///
/// The widget occupies space, takes no pointer input by default, and paints nothing:
/// what appears there is whatever the host puts in the [`Hole`](crate::Hole) it
/// reports for this widget.
pub struct ExternalContent {
    /// Size to ask for when nothing constrains it.
    natural: Size,
    /// How many times this widget has declared itself a hole.
    declarations: u64,
}

impl ExternalContent {
    /// A hole with a natural size, used when the layout does not decide one.
    pub fn new(natural: Size) -> Self {
        Self {
            natural,
            declarations: 0,
        }
    }

    /// How many frames this widget has declared itself an external layer in.
    pub fn declarations(&self) -> u64 {
        self.declarations
    }
}

impl Widget for ExternalContent {
    type Action = NoAction;

    fn on_anim_frame(&mut self, ctx: &mut UpdateCtx<'_>, _props: &mut PropertiesMut<'_>, _interval: u64) {
        // The declaration lives one paint: ask for the next frame.
        ctx.request_paint_only();
        ctx.request_anim_frame();
    }

    fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

    fn update(&mut self, ctx: &mut UpdateCtx<'_>, _props: &mut PropertiesMut<'_>, event: &masonry::core::Update) {
        // Start the loop as soon as the widget is in the tree.
        if matches!(event, masonry::core::Update::WidgetAdded) {
            ctx.request_anim_frame();
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
        let natural = match axis {
            Axis::Horizontal => self.natural.width,
            Axis::Vertical => self.natural.height,
        };
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(natural),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {}

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {
        // Nothing drawn: the host composites here.
        ctx.set_paint_layer_mode(PaintLayerMode::External);
        self.declarations += 1;
    }

    fn accepts_pointer_interaction(&self) -> bool {
        false
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}
