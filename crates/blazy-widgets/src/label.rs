//! A caption that follows the interface scale of the region it sits in.

use std::any::TypeId;

use blazy_areas::UiScale;
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NoAction, PaintCtx, PropertiesRef, RegisterCtx, UpdateCtx, Widget,
    WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Size};
use masonry::layout::{LayoutSize, LenDef, LenReq, Length, SizeDef};
use masonry::parley::StyleProperty;

/// A caption whose text size follows [`UiScale`].
///
/// The text channel of §46. A font size is not a property: it is a parley style, set
/// through `WidgetMut<masonry::widgets::Label>`, so nothing can push it onto a widget
/// from outside the way [`scale_box`](crate::scale_box) pushes padding. Something has to
/// know it is holding a label and make the call — and if that something is not this
/// widget, it is the application, downcasting a stock widget to resize its own caption.
///
/// Measured: through this channel a caption's box went from 60x18 to 112x34; through the
/// property channel alone it did not move at all.
pub struct Label {
    inner: WidgetPod<masonry::widgets::Label>,
    /// The size the text has at scale 1, which is the caller's to declare.
    ///
    /// Kept rather than read back: a size read after one scaling is already scaled, and
    /// multiplying it again is how an interface grows on every change.
    base_size: f32,
    /// The scale the text was last set at, so an unchanged scale costs nothing.
    applied: f64,
}

impl Label {
    /// The size a caption has at scale 1 unless the caller says otherwise.
    pub const BASE_SIZE: f32 = 12.0;

    /// A caption with the default base size.
    pub fn new(text: impl Into<std::sync::Arc<str>>) -> Self {
        Self::with_size(text, Self::BASE_SIZE)
    }

    /// A caption whose text measures `base_size` at scale 1.
    pub fn with_size(text: impl Into<std::sync::Arc<str>>, base_size: f32) -> Self {
        let label = masonry::widgets::Label::new(text).with_style(StyleProperty::FontSize(base_size));
        Self {
            inner: WidgetPod::new(label),
            base_size,
            applied: 1.0,
        }
    }

    /// The size this caption's text has at scale 1.
    pub fn base_size(&self) -> f32 {
        self.base_size
    }

    /// The scale this caption's text was last set at.
    ///
    /// What a test asks instead of reading the parley style back: the style is inside the
    /// inner label, and what matters is whether this widget noticed the scale at all.
    pub fn applied_scale(&self) -> f64 {
        self.applied
    }

    /// Sets the text size for `scale`, now rather than through a property.
    ///
    /// The fast path, and it exists for a reason measured in rewrite passes: carrying a
    /// scale by property costs one pass per level of nesting, because each level has to
    /// be told, lay itself out and tell the next. Four levels is what Masonry allows
    /// before it calls the loop a loop — and header, bar, caption, inner label is four
    /// (§46). A container that knows it is holding one of these calls this instead and
    /// spends no pass at all.
    pub fn set_scale(this: &mut WidgetMut<'_, Self>, scale: f64) {
        if this.widget.applied == scale {
            return;
        }
        this.widget.applied = scale;
        let size = this.widget.base_size * scale as f32;
        {
            let mut inner = this.ctx.get_mut(&mut this.widget.inner);
            masonry::widgets::Label::insert_style(&mut inner, StyleProperty::FontSize(size));
        }
        // The inner label knows it changed; this one is the widget whose measurement the
        // parent cached, and nothing else will tell it (§46).
        this.ctx.request_layout();
    }

    /// Replaces the text.
    pub fn set_text(this: &mut WidgetMut<'_, Self>, text: impl Into<std::sync::Arc<str>>) {
        let mut inner = this.ctx.get_mut(&mut this.widget.inner);
        masonry::widgets::Label::set_text(&mut inner, text);
    }
}

impl Widget for Label {
    type Action = NoAction;

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        // A scale change is a layout change, and saying so is §9's first rule: asking
        // for a repaint here is what makes an interface scale look free and behave wrong.
        UiScale::prop_changed(ctx, property_type);
    }

    fn measure(
        &mut self,
        ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        cross_length: Option<Length>,
    ) -> Length {
        let auto = match len_req {
            LenReq::MinContent => LenDef::MinContent,
            LenReq::MaxContent => LenDef::MaxContent,
            LenReq::FitContent(space) => LenDef::FitContent(space),
        };
        ctx.compute_length(&mut self.inner, auto, LayoutSize::default(), axis, cross_length)
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, props: &PropertiesRef<'_>, size: Size) {
        // The slow path: somebody pushed `UiScale` onto this widget without knowing what
        // it was. Correct, and one rewrite pass more expensive than [`Self::set_scale`],
        // which is why a `Bar` uses that one instead.
        //
        // Here rather than in `measure` because a widget whose parent fixed its size is
        // never measured, so a scale read there is read never. Setting a style needs a
        // `WidgetMut`, which layout does not have — and the mutate pass runs before the
        // next layout pass of the same loop, so the text is resized in the same frame.
        let scale = props.get::<UiScale>(ctx.property_cache()).0;
        if self.applied != scale {
            self.applied = scale;
            let text_size = self.base_size * scale as f32;
            ctx.mutate_child_later(&mut self.inner, move |mut label| {
                masonry::widgets::Label::insert_style(&mut label, StyleProperty::FontSize(text_size));
            });
        }
        let chosen = ctx.compute_size(&mut self.inner, SizeDef::fit(size), size.into());
        ctx.run_layout(&mut self.inner, chosen);
        ctx.place_child(&mut self.inner, Point::ORIGIN);
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.inner);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.inner.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}
