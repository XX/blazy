//! A text field bound to what a node holds.

use std::rc::Rc;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ActionCtx, ChildrenIds, ErasedAction, LayoutCtx, MeasureCtx, NoAction, PaintCtx, PropertiesMut,
    PropertiesRef, RegisterCtx, Update, UpdateCtx, Widget, WidgetId, WidgetMut, WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Size};
use masonry::layout::{LenDef, LenReq, Length};
use masonry::widgets::{TextAction, TextArea, TextInput};

use crate::{Change, NodeGraph, SessionHandle, SharedGraph};

/// What a node's content reads as in the field.
type Show<C> = dyn Fn(&C) -> String;
/// The content the field's text makes of the current content, or `None` for text that is not
/// one yet — half a number.
type Edit<C> = dyn Fn(&C, &str) -> Option<C>;

/// A text field that edits what node `index` holds — in a node's widget or in a panel (§70).
///
/// The four things every such field of `hello` and `calc` did by hand, in one place:
///
/// * **every key into the model**, and recorded for every view but the one the edit is shown in already — the field's
///   canvas, or its panel — so the others follow as it is typed and this one is not rebuilt under the caret (§47);
/// * **one step in the history** when the edit ends — the field left, `Enter`, `Escape` — if it changed anything, as
///   Blender writes one (§67), not one per key;
/// * **text that does not read** — an emptied number — leaves the model alone, and the field shows the model's value
///   again when it is left;
/// * `Enter` and `Escape` leave the field, so the keys are the editor's again.
///
/// The application says only how its content reads as text and what text makes of it.
pub struct ContentField<G: NodeGraph> {
    input: WidgetPod<TextInput>,
    graph: SharedGraph<G>,
    session: SessionHandle<G>,
    index: usize,
    /// The view that shows the edit already.
    past: Option<WidgetId>,
    show: Rc<Show<G::Content>>,
    edit: Rc<Edit<G::Content>>,
    /// What the node held when the edit began; `None` while nothing is being edited.
    before: Option<G::Content>,
}

impl<G: NodeGraph> ContentField<G> {
    /// A field over node `index` of `graph`, recorded in `session`'s history, its edits shown
    /// already in `past` (the canvas the node is in, or the panel).
    ///
    /// `show` is how the content reads in the field; `edit` is what the field's text makes of
    /// the current content, `None` for text that is not one.
    pub fn new(
        graph: &SharedGraph<G>,
        session: &SessionHandle<G>,
        index: usize,
        past: Option<WidgetId>,
        show: impl Fn(&G::Content) -> String + 'static,
        edit: impl Fn(&G::Content, &str) -> Option<G::Content> + 'static,
    ) -> Self {
        let text = show(&graph.borrow().content(index));
        Self {
            input: WidgetPod::new(TextInput::new(&text)),
            graph: graph.clone(),
            session: session.clone(),
            index,
            past,
            show: Rc::new(show),
            edit: Rc::new(edit),
            before: None,
        }
    }

    /// The text input inside, for a test or an application that styles it.
    pub fn input_mut<'t>(this: &'t mut WidgetMut<'_, Self>) -> WidgetMut<'t, TextInput> {
        this.ctx.get_mut(&mut this.widget.input)
    }

    /// The edit ended: one step if it changed what reads, and the model's value shown again.
    fn finish(&mut self, ctx: &mut UpdateCtx<'_>) {
        if let Some(before) = self.before.take() {
            let after = self.graph.borrow().content(self.index);
            if (self.show)(&before) != (self.show)(&after) {
                self.session.borrow_mut().record_content(self.index, before, after);
            }
        }
        ctx.mutate_self_later(|mut widget| {
            let mut field = widget.downcast::<Self>();
            let shown = (field.widget.show)(&field.widget.graph.borrow().content(field.widget.index));
            let mut input = field.ctx.get_mut(&mut field.widget.input);
            let mut text = TextInput::text_mut(&mut input);
            if text.widget.text().to_string() != shown {
                TextArea::reset_text(&mut text, &shown);
            }
        });
    }
}

impl<G: NodeGraph> Widget for ContentField<G> {
    type Action = NoAction;

    fn measure(
        &mut self,
        ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        cross_length: Option<Length>,
    ) -> Length {
        let def = match len_req {
            LenReq::FitContent(space) => LenDef::Fixed(space),
            LenReq::MinContent => LenDef::MinContent,
            LenReq::MaxContent => LenDef::MaxContent,
        };
        let context = masonry::layout::LayoutSize::default();
        ctx.compute_length(&mut self.input, def, context, axis, cross_length)
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        ctx.run_layout(&mut self.input, size);
        ctx.place_child(&mut self.input, Point::ORIGIN);
    }

    fn on_action(
        &mut self,
        ctx: &mut ActionCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        action: &ErasedAction,
        _source: WidgetId,
    ) {
        let Some(action) = action.downcast_ref::<TextAction>() else {
            return;
        };
        match action {
            TextAction::Changed(text) => {
                let graph = &mut *self.graph.borrow_mut();
                let current = graph.content(self.index);
                if let Some(content) = (self.edit)(&current, text) {
                    self.before.get_or_insert(current);
                    graph.set_content(self.index, content);
                    let change = Change::Contents { index: self.index };
                    match self.past {
                        Some(view) => graph.views().note_except(view, change),
                        None => graph.views().note(change),
                    }
                }
            },
            TextAction::Entered(_) | TextAction::Cancelled => ctx.resign_focus(),
        }
        ctx.set_handled();
    }

    fn update(&mut self, ctx: &mut UpdateCtx<'_>, _props: &mut PropertiesMut<'_>, event: &Update) {
        if matches!(event, Update::ChildFocusChanged(false)) {
            self.finish(ctx);
        }
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.input);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.input.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}
