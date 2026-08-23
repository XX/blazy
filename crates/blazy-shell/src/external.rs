//! A widget that reserves its rectangle for the host to draw.
//!
//! `rnd/architecture.md` §4.3: `VisualLayerKind::External` is how the core paint model
//! says "the content here is not mine". For a Blender-like application this is the
//! 3D viewport, and §14 lists it as a requirement rather than an option.
//!
//! # Why this is a widget and not a note in the documentation
//!
//! The declaration does not survive a frame in which the widget does not paint
//! (§26.1): `paint_layer_mode` is reset to `Inline` at the start of every paint pass
//! for every widget, and only the widget's own `paint` can set it again — which a
//! retained tree does not call for a clean widget. The frame then carries the
//! widget's *cached scene* inline instead of a hole, and nothing reports it.
//!
//! So an external widget has to keep painting. It draws nothing, so the cost is a
//! function call and an empty scene; what it really costs is that the window cannot
//! idle while one is on screen. That is acceptable for what this is for — a hole
//! exists because something live is behind it — but it is a workaround, and the
//! proper fix is upstream: the mode wants to be a property of the widget rather than
//! a flag of the pass.

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
    ///
    /// The declaration is per-paint and easy to lose (§26.1), so the count is worth
    /// having: compared against the holes the host received, it is the difference
    /// between "there was no hole" and "the hole went missing".
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
        // The declaration lives for exactly one paint (§26.1), so the widget has to
        // paint every frame to keep it. Asking for the next animation frame here is
        // what keeps that going without the host having to know this widget exists.
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
        // Nothing is drawn on purpose: whatever the host composites into this
        // rectangle would otherwise be painted over by our own placeholder.
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
