//! Menus: a list of operators under the pointer.
//!
//! A [`Menu`] is data — labels, operator names, properties. Choosing an entry runs the
//! operator as a script does, with the same poll (an entry that would be refused is shown
//! refused) and the same history. A menu is opened by an operator of its own ([`MenuOp`]),
//! which leaves it in the world for the editor to put on screen as a layer.

use std::any::TypeId;
use std::marker::PhantomData;

use blazy_ops::keymap::{Name, Props};
use blazy_ops::runtime::OpCtx;
use blazy_ops::{OpResult, Operator};
use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ActionCtx, ChildrenIds, ErasedAction, EventCtx, Handled, Layer, LayoutCtx, MeasureCtx, NewWidget,
    NoAction, PaintCtx, PointerEvent, PropertiesMut, PropertiesRef, RegisterCtx, UpdateCtx, Widget, WidgetId,
    WidgetPod,
};
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Size, Stroke};
use masonry::layout::{LayoutSize, LenReq, Length, SizeDef};
use masonry::peniko::Color;
use masonry::widgets::{Button, ButtonPress, Flex, Label};

use crate::{EditorWorld, NodeEditor, NodeGraph};

/// One entry of a menu: what it says and what it runs.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    /// What the entry says.
    pub label: Name,
    /// The operator it runs, by name.
    pub op: Name,
    /// The operator's properties.
    pub props: Props,
}

/// A menu: a title and its entries, as data.
#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
    /// What the menu says at the top.
    pub title: Name,
    /// The entries, in order.
    pub items: Vec<MenuItem>,
}

impl Menu {
    /// An empty menu.
    pub fn new(title: impl Into<Name>) -> Self {
        Self {
            title: title.into(),
            items: Vec::new(),
        }
    }

    /// The same menu, with an entry at the end.
    #[must_use]
    pub fn with(mut self, label: impl Into<Name>, op: impl Into<Name>, props: Props) -> Self {
        self.items.push(MenuItem {
            label: label.into(),
            op: op.into(),
            props,
        });
        self
    }
}

/// The operator that opens a menu: one per menu, such as `menu.node`.
pub struct MenuOp {
    name: &'static str,
    menu: Menu,
}

impl MenuOp {
    /// An operator called `name` that opens `menu`.
    pub fn new(name: &'static str, menu: Menu) -> Self {
        Self { name, menu }
    }
}

impl<G: NodeGraph> Operator<EditorWorld<G>> for MenuOp {
    fn name(&self) -> &'static str {
        self.name
    }

    fn invoke(&mut self, cx: &mut OpCtx<'_, EditorWorld<G>>) -> OpResult {
        cx.world_mut().menu = Some(self.menu.clone());
        OpResult::Finished
    }
}

/// What the menu shows of one entry once the runtime has been asked about it.
pub(crate) struct Entry {
    pub(crate) label: Name,
    pub(crate) op: Name,
    pub(crate) props: Props,
    /// Whether the operator's poll would let it run now.
    pub(crate) enabled: bool,
    /// The binding that runs the same operator with the same properties, as text.
    pub(crate) shortcut: Option<String>,
}

/// A menu on screen: a layer above the window, at the pointer.
///
/// An entry runs in the editor that opened the menu, through [`NodeEditor::exec`]. A click
/// elsewhere closes it and goes no further, as in Blender. The editor closes it on `Escape`
/// (a layer cannot take focus when it is added).
pub struct MenuLayer<G: NodeGraph> {
    child: WidgetPod<dyn Widget>,
    /// Each entry's button, and what it runs.
    entries: Vec<(WidgetId, Name, Props)>,
    /// The editor whose session the entries run in.
    target: WidgetId,
    /// Where the menu was opened, in the window's coordinates.
    at: Point,
    /// The window's size, as the layer stack hands it to a layer root to measure in.
    window: Option<Size>,
    /// Whether the menu has been moved inside the window yet.
    placed: bool,
    _graph: PhantomData<fn() -> G>,
}

impl<G: NodeGraph> MenuLayer<G> {
    /// A menu over `entries`, running them in the editor `target`.
    pub(crate) fn new(title: &str, entries: Vec<Entry>, target: WidgetId, at: Point) -> Self {
        let mut column = Flex::column().with_fixed(NewWidget::new(Label::new(title.to_owned())));
        let mut runs = Vec::with_capacity(entries.len());
        for entry in entries {
            let mut row = Flex::row().with_fixed(NewWidget::new(Label::new(entry.label.to_string())));
            if let Some(shortcut) = entry.shortcut {
                row = row.with_spacer(1.0).with_fixed(NewWidget::new(Label::new(shortcut)));
            }
            let button = NewWidget::new(Button::new(NewWidget::new(row))).disabled(!entry.enabled);
            runs.push((button.id(), entry.op, entry.props));
            column = column.with_fixed(button);
        }
        Self {
            child: NewWidget::new(column).erased().to_pod(),
            entries: runs,
            target,
            at,
            window: None,
            placed: false,
            _graph: PhantomData,
        }
    }
}

impl<G: NodeGraph> MenuLayer<G> {
    /// Each entry's button and the operator it runs, in order.
    pub fn entries(&self) -> impl Iterator<Item = (WidgetId, &str)> {
        self.entries.iter().map(|(id, op, _)| (*id, op.as_ref()))
    }
}

impl<G: NodeGraph> Widget for MenuLayer<G> {
    type Action = NoAction;

    /// An entry was chosen: run it in the editor, and close.
    fn on_action(
        &mut self,
        ctx: &mut ActionCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        action: &ErasedAction,
        source: WidgetId,
    ) {
        if !action.is::<ButtonPress>() {
            return;
        }
        let Some((_, op, props)) = self.entries.iter().find(|(id, ..)| *id == source) else {
            return;
        };
        let (op, props) = (op.clone(), props.clone());
        // In the mutate pass, where the editor's `WidgetMut` is: the script path.
        ctx.mutate_later(self.target, move |mut widget| {
            if let Some(mut editor) = widget.try_downcast::<NodeEditor<G>>() {
                NodeEditor::menu_closed(&mut editor);
                NodeEditor::exec(&mut editor, &op, &props);
            }
        });
        ctx.remove_layer(ctx.widget_id());
        ctx.set_handled();
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.child);
    }

    fn property_changed(&mut self, _ctx: &mut UpdateCtx<'_>, _property_type: TypeId) {}

    fn measure(
        &mut self,
        ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        cross_length: Option<Length>,
    ) -> Length {
        // A layer root is measured against the window: that is how the menu learns its size.
        let window = ctx.context_size();
        if let (Some(width), Some(height)) = (window.length(Axis::Horizontal), window.length(Axis::Vertical)) {
            self.window = Some(Size::new(width.get(), height.get()));
        }
        let context = LayoutSize::maybe(axis.cross(), cross_length);
        ctx.compute_length(&mut self.child, len_req.into(), context, axis, cross_length)
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        let child_size = ctx.compute_size(&mut self.child, SizeDef::fit(size), size.into());
        ctx.run_layout(&mut self.child, child_size);
        ctx.place_child(&mut self.child, Point::ORIGIN);
        // Moved inside the window once, if it opened hanging off an edge.
        if !self.placed
            && let Some(window) = self.window
        {
            self.placed = true;
            let x = self.at.x.min(window.width - size.width).max(0.0);
            let y = self.at.y.min(window.height - size.height).max(0.0);
            if (x, y) != (self.at.x, self.at.y) {
                ctx.reposition_layer(ctx.widget_id(), Point::new(x, y));
            }
        }
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        let bounds = ctx.border_box();
        painter.fill(bounds, Color::from_rgb8(0x24, 0x24, 0x2a)).draw();
        painter
            .stroke(
                bounds.inset(-0.5),
                &Stroke::new(1.0),
                Color::from_rgb8(0x50, 0x50, 0x5c),
            )
            .draw();
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.child.id()])
    }

    fn as_layer(&mut self) -> Option<&mut dyn Layer> {
        Some(self)
    }

    fn accessibility_role(&self) -> Role {
        Role::Menu
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

impl<G: NodeGraph> Layer for MenuLayer<G> {
    /// A press outside the menu closes it, and goes no further.
    fn capture_pointer_event(
        &mut self,
        ctx: &mut EventCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        event: &PointerEvent,
    ) -> Handled {
        let PointerEvent::Down(down) = event else {
            return Handled::No;
        };
        let at = ctx.local_position(down.state.position);
        if ctx.border_box().contains(at) {
            return Handled::No;
        }
        ctx.remove_layer(ctx.widget_id());
        ctx.mutate_later(self.target, |mut widget| {
            if let Some(mut editor) = widget.try_downcast::<NodeEditor<G>>() {
                NodeEditor::menu_closed(&mut editor);
            }
        });
        Handled::Yes
    }
}
