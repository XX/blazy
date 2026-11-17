//! The smallest application on blazy, and the check that one can be written.
//!
//! A node editor over a graph of its own: two areas side by side showing the same graph,
//! each with its own view, selection and history, in as many windows as the user opens —
//! against the facade and nothing else. Every other example reaches the library the same
//! way, but each of them is an experiment first; this one exists only so that "an
//! application needs nothing but `blazy`" is a program that compiles rather than a
//! sentence in `crates/blazy` (§15.1), and so that what an application has to write is
//! counted rather than claimed (`issues/application assembly.md`).
//!
//! What it writes is what only it knows — the graph, what a node looks like, what fills
//! an area. Everything else is `blazy::app::EditorApp`.
//!
//! ```text
//! cargo run -p hello
//! cargo run -p hello -- my.keymap     # with keymap overrides
//! ```
//!
//! Drag a node to move it (by its body: the name field at its top is a text field, and
//! a press there edits the name), drag out of a port (the dots on a node's edges) into another
//! node's port to link them — out of an input that has a link to move the link, or to let
//! it go on nothing and drop it; `Ctrl`-drag an output to move all of its links. `B` to box
//! select, `G` to grab, `Shift+A` to add, `X` to delete, `F` to link the two selected
//! nodes, `Shift+D` to duplicate the selection and grab the copies, `Ctrl+C` / `Ctrl+V`
//! to copy it and paste it at the pointer — in any area, in any window — and `Ctrl+Z` to
//! undo. A node or a link held at
//! the edge of an area scrolls the view. `Alt+X` / `Alt+Y` split the area under the
//! pointer, `Alt+J` joins it with its sibling, `Alt+N` opens another window over the same
//! graph and `Alt+D` moves the area into one of its own. `N` shows the area's sidebar — the
//! selected node's name, place and size, and arrows that move it — and its left edge widens
//! it. Click a node's name to rename
//! it — copy, paste and an input method work there — and `Enter` or `Escape` to leave
//! the field; the other areas show the new name.

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::rc::Rc;

use blazy::app::EditorApp;
use blazy::areas::{CarriesScale, ScaleCarrier, SplitTree, push_ui_scale};
use blazy::canvas::{CanvasLayer, Detail, Link, NodeSource, PortLayout, Ports};
use blazy::masonry::accesskit::{Node as AccessNode, Role};
use blazy::masonry::core::{
    AccessCtx, ActionCtx, ChildrenIds, DefaultProperties, ErasedAction, LayoutCtx, MeasureCtx, NewWidget, NoAction,
    PaintCtx, PropertiesMut, PropertiesRef, PropertySet, PropertyStack, RegisterCtx, Selector, Widget, WidgetId,
    WidgetMut, WidgetPod,
};
use blazy::masonry::imaging::Painter;
use blazy::masonry::kurbo::{Axis, BezPath, Point, Rect, Shape, Size};
use blazy::masonry::layout::{LenDef, LenReq, Length, SizeDef};
use blazy::masonry::peniko::Color;
use blazy::masonry::properties::{Background, BorderColor, BorderWidth, CornerRadius, Padding};
use blazy::masonry::theme::default_property_set;
use blazy::masonry::widgets::{Button, ButtonPress, TextAction, TextInput};
use blazy::node_editor::{
    Change, EdgePan, GraphPanel, NodeEditor, NodeGraph, OverlayStyle, SELECTED, SelectionOutline, SharedGraph,
    ViewToken, Views,
};
use blazy::ops::keymap::Props;
use blazy::shell::window::{Error, WindowConfig};
use blazy::widgets::{Bar, Label, Metrics};

/// Nodes the graph starts with.
const NODES: usize = 12;
/// Nodes per row of the grid they start on.
const COLS: usize = 4;
/// Every node's size: room for the name field and a body below it to drag the node by.
const NODE: Size = Size::new(140.0, 80.0);
/// The widths a node may be dragged to by its left or right edge (`node.resize`).
const NODE_WIDTHS: (f64, f64) = (90.0, 480.0);
/// Inputs and outputs every node has.
const PORTS_IN: u16 = 2;
const PORTS_OUT: u16 = 1;
const PORTS: Ports = Ports {
    inputs: PORTS_IN,
    outputs: PORTS_OUT,
};
/// Where they are on a node: in two rows down its edges, drawn as dots.
const PORT_LAYOUT: PortLayout = PortLayout {
    first: Some(20.0),
    step: 20.0,
    dot_radius: 4.0,
    dot_color: Color::from_rgb8(0x9a, 0x9a, 0xa8),
};

/// The graph: where each node is, what it is called, which nodes are linked, and who is
/// looking.
///
/// A node's index is an index with holes where removals were, and a freed one is handed
/// out again first — the rule `NodeGraph::insert_node` asks for (§43). Its title is what
/// the user typed into its field; the model holds it, because the field's widget comes
/// and goes with the view (§20.2) and there is one per area (§30).
#[derive(Default)]
struct Graph {
    nodes: Vec<Option<Rect>>,
    titles: Vec<String>,
    free: Vec<usize>,
    links: Vec<Link>,
    views: Views,
}

impl Graph {
    fn grid() -> SharedGraph<Self> {
        let nodes = (0..NODES)
            .map(|i| {
                let pos = Point::new(40.0 + (i % COLS) as f64 * 180.0, 40.0 + (i / COLS) as f64 * 110.0);
                Some(Rect::from_origin_size(pos, NODE))
            })
            .collect();
        let links = (0..NODES)
            .filter(|i| i % COLS != COLS - 1 && i + 1 < NODES)
            .map(|i| Link::new(i, i + 1))
            .collect();
        Rc::new(RefCell::new(Self {
            nodes,
            titles: (0..NODES).map(default_title).collect(),
            links,
            ..Self::default()
        }))
    }
}

impl NodeGraph for Graph {
    fn node_count(&self) -> usize {
        self.nodes.iter().flatten().count()
    }

    fn node_rect(&self, index: usize) -> Rect {
        self.nodes.get(index).copied().flatten().unwrap_or_default()
    }

    fn set_node_pos(&mut self, index: usize, pos: Point) {
        if let Some(Some(rect)) = self.nodes.get_mut(index) {
            *rect = Rect::from_origin_size(pos, rect.size());
        }
    }

    fn set_node_rect(&mut self, index: usize, rect: Rect) {
        if let Some(Some(held)) = self.nodes.get_mut(index) {
            *held = rect;
        }
    }

    /// Wide enough for a name and the ports' dots, and not so wide as to cover the area.
    fn width_limits(&self, _index: usize) -> Option<(f64, f64)> {
        Some(NODE_WIDTHS)
    }

    fn views(&self) -> &Views {
        &self.views
    }

    /// A new node gets a title of its own, even under a name a removal freed.
    fn insert_node(&mut self, rect: Rect) -> usize {
        match self.free.pop() {
            Some(index) => {
                self.nodes[index] = Some(rect);
                self.titles[index] = default_title(index);
                index
            },
            None => {
                self.nodes.push(Some(rect));
                self.titles.push(default_title(self.nodes.len() - 1));
                self.nodes.len() - 1
            },
        }
    }

    /// Back with the title it had: the history keeps it, so it comes back even when the
    /// name was handed out again in between.
    fn restore_node(&mut self, index: usize, rect: Rect, title: String) {
        if self.nodes.len() <= index {
            self.nodes.resize(index + 1, None);
            self.titles.resize_with(index + 1, String::new);
        }
        self.free.retain(|&free| free != index);
        self.nodes[index] = Some(rect);
        self.titles[index] = title;
    }

    fn remove_node(&mut self, index: usize) -> Vec<Link> {
        self.nodes[index] = None;
        self.free.push(index);
        let ends_here = |link: &Link| link.from as usize == index || link.to as usize == index;
        let gone = self.links.iter().copied().filter(ends_here).collect();
        self.links.retain(|link| !ends_here(link));
        gone
    }

    fn insert_link(&mut self, link: Link) -> bool {
        let live = |end: u32| self.nodes.get(end as usize).is_some_and(Option::is_some);
        let known = self.links.iter().any(|&l| l == link || l == link.reversed());
        if !self.accepts_link(link) || !live(link.from) || !live(link.to) || known {
            return false;
        }
        self.links.push(link);
        true
    }

    /// The rule a link drag shows before the release: ports that exist, on two nodes.
    fn accepts_link(&self, link: Link) -> bool {
        link.from != link.to && link.from_port < PORTS_OUT && link.to_port < PORTS_IN
    }

    fn remove_link(&mut self, link: Link) {
        self.links.retain(|&l| l != link && l != link.reversed());
    }

    /// A node holds its title, and a copy of it is called the same.
    type Content = String;

    fn content(&self, index: usize) -> String {
        self.titles.get(index).cloned().unwrap_or_default()
    }

    /// A copy of `hello`'s nodes is read by `hello` alone: any text is a title, so the name
    /// is what keeps another application's copy from becoming nodes named after its lines.
    fn content_format(&self) -> Option<&str> {
        Some("hello-title")
    }

    /// The title is its own text: a copy goes to the system clipboard as it is.
    fn content_text(&self, title: &String) -> Option<String> {
        Some(title.clone())
    }

    fn content_from_text(&self, text: &str) -> Option<String> {
        Some(text.to_owned())
    }

    fn insert_copy(&mut self, rect: Rect, title: String) -> usize {
        let index = self.insert_node(rect);
        self.titles[index] = title;
        index
    }

    fn links_of(&self, index: usize) -> Vec<Link> {
        let ends_here = |link: &&Link| link.from as usize == index || link.to as usize == index;
        self.links.iter().filter(ends_here).copied().collect()
    }
}

/// What a node looks like, and the canvas's place among the graph's views.
///
/// A node's widget is built when the node scrolls into view and dropped when it leaves,
/// so it holds no state of its own (§20.3): what it shows comes from the name, which is
/// to say from the model. The token is held here because the canvas drops its source
/// when it leaves the tree, and that is how the graph learns the view is gone.
struct Nodes {
    graph: SharedGraph<Graph>,
    view: Option<ViewToken>,
}

/// The title a node starts with.
fn default_title(index: usize) -> String {
    format!("node {index}")
}

/// A node's fill, and its outline.
const FILL: Color = Color::from_rgb8(0x2c, 0x2c, 0x34);
const OUTLINE: Color = Color::from_rgb8(0x50, 0x50, 0x5c);
/// The outline of a selected node.
const PICKED: Color = Color::from_rgb8(0xff, 0xa5, 0x2c);

impl NodeSource for Nodes {
    /// A title field in a box. Everything the box looks like — selected or not — is in
    /// [`properties`]: the node does not know what a selection is.
    fn build(&mut self, index: usize, _detail: Detail) -> NewWidget<dyn Widget> {
        let title = self.graph.borrow().titles.get(index).cloned().unwrap_or_default();
        NewWidget::new(Node {
            title: WidgetPod::new(TextInput::new(&title)),
            graph: self.graph.clone(),
            index,
            canvas: self.view.as_ref().map(ViewToken::canvas),
        })
        .erased()
    }

    /// The nodes too small to deserve widgets, all in one fill (§31): without this an
    /// overview shows the links and nothing at their ends.
    fn paint_far(&mut self, nodes: &[(usize, Rect)], _scale: f64, painter: &mut Painter<'_>) {
        let mut path = BezPath::new();
        for &(_, rect) in nodes {
            path.extend(rect.path_elements(0.1));
        }
        if !path.is_empty() {
            painter.fill(&path, FILL).draw();
        }
    }

    /// Two inputs and an output: what a link can be dragged out of and into.
    fn ports(&mut self, _index: usize) -> Ports {
        PORTS
    }

    fn attached(&mut self, canvas: WidgetId) {
        self.view = Some(self.graph.borrow().views().attach(canvas));
    }
}

/// A node: its title in a text field, a box, and the dots of its ports.
///
/// Its own type rather than a stock `SizedBox` for the dots: they are drawn by the node,
/// so that they sit at its level among the nodes — a node over this one covers them, and
/// the corner it leaves unpainted shows them (§50.8). The box is Masonry's: it paints
/// the background and the border of any widget from its properties.
///
/// The field is a stock `TextInput`, which is the point of having one: it is what needs
/// the host's clipboard and input method (`issues/ime and clipboard in the host.md`).
struct Node {
    title: WidgetPod<TextInput>,
    graph: SharedGraph<Graph>,
    index: usize,
    /// The canvas this node is in, so an edit is recorded for the others only.
    canvas: Option<WidgetId>,
}

impl Widget for Node {
    type Action = NoAction;

    /// The canvas gives a node its size from the graph; this is only a fallback.
    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::FitContent(space) => space,
            LenReq::MinContent | LenReq::MaxContent => Length::px(match axis {
                Axis::Horizontal => NODE.width,
                Axis::Vertical => NODE.height,
            }),
        }
    }

    /// The field across the top at its own height; the rest of the box is what a drag
    /// takes the node by, because a press on the field is the field's.
    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        let auto = SizeDef::new(LenDef::Fixed(Length::px(size.width)), LenDef::MaxContent);
        let title = ctx.compute_size(&mut self.title, auto, size.into());
        ctx.run_layout(&mut self.title, title);
        ctx.place_child(&mut self.title, Point::ORIGIN);
    }

    /// An edit of the title goes into the model, and is recorded for the other views.
    ///
    /// For the others only: this field shows the edit already, and a change owed to its
    /// own canvas would rebuild the node — and the field — under the cursor on every key
    /// (§47). `Enter` and `Escape` leave the field, so the keys are the editor's again.
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
                if let Some(title) = graph.titles.get_mut(self.index) {
                    title.clone_from(text);
                }
                let change = Change::Contents { index: self.index };
                match self.canvas {
                    Some(canvas) => graph.views().note_except(canvas, change),
                    None => graph.views().note(change),
                }
            },
            TextAction::Entered(_) | TextAction::Cancelled => ctx.resign_focus(),
        }
        ctx.set_handled();
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn post_paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        PORT_LAYOUT.paint_ports(painter, ctx.border_box(), PORTS);
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.title);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.title.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut AccessNode) {}
}

/// How far a press of an arrow in the panel moves the node, in canvas units.
const NUDGE: f64 = 20.0;

/// What the panel shows for `selection`: the node's name, to edit; where it is and how
/// big; two arrows that move it — or a word, for no node or several.
///
/// Every container a carrier, so the sidebar's interface scale reaches the last caption in
/// the event it is set in (§62).
fn properties_of(
    graph: &SharedGraph<Graph>,
    selection: &std::collections::BTreeSet<usize>,
    panel: WidgetId,
    editor: Option<WidgetId>,
) -> NewWidget<dyn Widget> {
    let row = |bar: Bar| NewWidget::new(bar).with_props(ScaleCarrier::of::<Bar>()).erased();
    let caption = |text: String| NewWidget::new(Label::new(text)).erased();
    let metrics = Metrics {
        padding: 4.0,
        gap: 6.0,
        ..Metrics::default()
    };
    let mut column = Bar::column(metrics);
    let shown = match selection.iter().collect::<Vec<_>>()[..] {
        [&index] if graph.borrow().node_rect_of(index).is_some() => Some(index),
        _ => None,
    };
    let Some(index) = shown else {
        let word = if selection.is_empty() {
            "No node selected"
        } else {
            "Several nodes selected"
        };
        column = column.with(row(Bar::new(metrics).with(caption(word.into()), metrics)), metrics);
        return NewWidget::new(column).with_props(ScaleCarrier::of::<Bar>()).erased();
    };
    let (title, rect) = {
        let graph = graph.borrow();
        (
            graph.titles.get(index).cloned().unwrap_or_default(),
            graph.node_rect(index),
        )
    };
    let name = NewWidget::new(TextInput::new(&title));
    let left = NewWidget::new(Button::with_text("<"));
    let right = NewWidget::new(Button::with_text(">"));
    let (left_id, right_id) = (left.id(), right.id());
    let line = |label: &str, value: NewWidget<dyn Widget>| {
        row(Bar::new(metrics)
            .with(caption(label.into()), metrics)
            .with(value, metrics))
    };
    column = column
        .with(line("Name", name.erased()), metrics)
        .with(
            line("Position", caption(format!("{:.0}, {:.0}", rect.x0, rect.y0))),
            metrics,
        )
        .with(
            line("Size", caption(format!("{:.0} x {:.0}", rect.width(), rect.height()))),
            metrics,
        )
        .with(
            row(Bar::new(metrics)
                .with(caption("Move".into()), metrics)
                .with(left.erased(), metrics)
                .with(right.erased(), metrics)),
            metrics,
        );
    NewWidget::new(Properties {
        content: NewWidget::new(column)
            .with_props(ScaleCarrier::of::<Bar>())
            .erased()
            .to_pod(),
        graph: graph.clone(),
        index,
        panel,
        editor,
        nudges: [(left_id, -NUDGE), (right_id, NUDGE)],
    })
    .with_props(ScaleCarrier::of::<Properties>())
    .erased()
}

/// One node's properties: what the panel's controls do.
///
/// The name is the model's, written as the node's own field writes it and recorded past
/// the panel — the panel shows the edit already, and rebuilding it would take the field
/// from under the caret (§47). The arrows are a move, so they go through the editor's
/// `node.move`: a step in the history, which `Ctrl+Z` undoes, and in every view.
struct Properties {
    content: WidgetPod<dyn Widget>,
    graph: SharedGraph<Graph>,
    index: usize,
    /// The `GraphPanel` this is the content of: the id the name's edits are recorded past.
    panel: WidgetId,
    /// The editor of the area, which runs the move.
    editor: Option<WidgetId>,
    /// Each arrow, and how far it moves the node along x.
    nudges: [(WidgetId, f64); 2],
}

impl CarriesScale for Properties {
    fn carry_now(this: &mut WidgetMut<'_, Self>, scale: f64) {
        let mut content = this.ctx.get_mut(&mut this.widget.content);
        push_ui_scale(&mut content, scale);
    }
}

impl Widget for Properties {
    type Action = NoAction;

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::FitContent(space) => space,
            _ => Length::ZERO,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        ctx.run_layout(&mut self.content, size);
        ctx.place_child(&mut self.content, Point::ORIGIN);
    }

    fn on_action(
        &mut self,
        ctx: &mut ActionCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        action: &ErasedAction,
        source: WidgetId,
    ) {
        // The one text field is the name. Its action comes from the text area inside the
        // `TextInput`, not from the input itself, so it is told by its type and not its id.
        if let Some(action) = action.downcast_ref::<TextAction>() {
            match action {
                TextAction::Changed(text) => {
                    let graph = &mut *self.graph.borrow_mut();
                    if let Some(title) = graph.titles.get_mut(self.index) {
                        title.clone_from(text);
                    }
                    graph
                        .views()
                        .note_except(self.panel, Change::Contents { index: self.index });
                },
                TextAction::Entered(_) | TextAction::Cancelled => ctx.resign_focus(),
            }
            ctx.set_handled();
        } else if action.downcast_ref::<ButtonPress>().is_some()
            && let Some(&(_, dx)) = self.nudges.iter().find(|(id, _)| *id == source)
            && let Some(editor) = self.editor
        {
            // The move is the selection's — this node, since the panel shows one.
            ctx.mutate_later(editor, move |mut widget| {
                let props = Props::new().with_float("dx", dx);
                let _ = NodeEditor::<Graph>::exec(&mut widget.downcast(), "node.move", &props);
            });
            ctx.set_handled();
        }
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.content);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.content.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::Form
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut AccessNode) {}
}

/// The theme, plus what a node looks like — and what it looks like selected.
///
/// The style is the node type's own stack: a property stack replaces the theme's for
/// its type, so styling a `Label` would lose every label's. The box is in the stack
/// rather than on each node because a property put on the widget itself beats the
/// stack, and the outline could then never change colour.
fn properties() -> DefaultProperties {
    let mut properties = default_property_set();
    let mut stack = PropertyStack::new();
    stack.push_layer(
        Selector::new(),
        PropertySet::new()
            .with(Background::Color(FILL))
            .with(BorderColor { color: OUTLINE })
            .with(BorderWidth { width: Length::px(1.0) })
            .with(CornerRadius {
                radius: Length::px(6.0),
            })
            .with(Padding::all(Length::px(8.0))),
    );
    // Selected is a class the editor puts on the node; a colour is all it changes, so
    // a click repaints the node and lays nothing out.
    stack.push_layer(Selector::classes(&[SELECTED]), BorderColor { color: PICKED });
    properties.insert_stack::<Node>(stack);
    properties
}

/// The application: what fills an area is an editor over the graph.
fn app(graph: &SharedGraph<Graph>) -> EditorApp<Graph> {
    let graph_of_areas = graph.clone();
    EditorApp::new(graph, move |_area, session| {
        let graph = graph_of_areas.clone();
        let names = graph.borrow().nodes.len();
        let links = graph.borrow().links.clone();
        let geometry = {
            let graph = graph.clone();
            move |index: usize| graph.borrow().node_rect_of(index)
        };
        let nodes = Nodes { graph, view: None };
        let canvas = CanvasLayer::new(names, geometry, nodes)
            .with_links(links)
            .with_ports(PORT_LAYOUT);
        // The nodes look selected by themselves, so the editor outlines only the far
        // field, where there is no node widget to wear the class.
        let style = OverlayStyle {
            outline: SelectionOutline::FarField,
            ..OverlayStyle::default()
        };
        NewWidget::new(NodeEditor::with_session(canvas, session.clone()).with_style(style)).erased()
    })
    // A node or a link held at the edge of an area scrolls the view, as in Blender. A
    // choice rather than a default: without it the gesture carries on past the border and
    // the view stands still.
    .with_edge_pan(EdgePan::default())
    // A sidebar on every area, `N` to show it, its left edge to widen it: the properties of
    // the selected node.
    .with_sidebar(|_area, session| {
        let session = session.clone();
        let graph = session.borrow().world.graph.clone();
        let panel = GraphPanel::new(&graph, move |graph, selection, panel| {
            let editor = session.borrow().editor;
            properties_of(graph, selection, panel, editor)
        });
        NewWidget::new(panel)
            .with_props(ScaleCarrier::of::<GraphPanel<Graph>>())
            .erased()
    })
}

impl Graph {
    /// Where node `index` is, or `None` for a name a removal freed.
    fn node_rect_of(&self, index: usize) -> Option<(Point, Size)> {
        self.nodes
            .get(index)
            .copied()
            .flatten()
            .map(|rect| (rect.origin(), rect.size()))
    }
}

fn main() -> Result<(), Error> {
    let config = WindowConfig::default()
        .with_title("blazy - hello")
        .with_size(1000.0, 600.0);
    let app = app(&Graph::grid());
    // A path, if given, is a file of keymap overrides (`blazy::app::default_keymap()`
    // written out is where one starts).
    let app = match std::env::args_os().nth(1) {
        Some(path) => app.with_keymap_overrides(&path).unwrap_or_else(|error| {
            eprintln!("{}: {error}", std::path::Path::new(&path).display());
            std::process::exit(2);
        }),
        None => app,
    };
    app.with_properties(properties()).run(config, SplitTree::balanced(2))
}

#[cfg(test)]
mod tests {
    use blazy::masonry::dpi::PhysicalSize;
    use blazy::masonry::testing::TestHarness;
    use blazy::masonry::widgets::TextArea;
    use blazy::node_editor::sync_canvas;
    use blazy::ops::keymap::Props;

    use super::*;

    /// The window's content builds, lays out and virtualises, headless — and the graph
    /// can be edited in it. A window cannot open on CI; the tree it would show can, and
    /// through the same facade.
    #[test]
    fn the_screen_builds_and_an_edit_in_one_area_reaches_the_other() {
        let graph = Graph::grid();
        let screen = app(&graph).screen(SplitTree::balanced(2));
        let mut harness =
            TestHarness::create_with_size(properties(), NewWidget::new(screen), PhysicalSize::new(1000, 600));
        let _ = harness.redraw();

        let areas = editor_ids(&harness);
        assert_eq!(areas.len(), 2);
        assert_eq!(
            graph.borrow().views().len(),
            2,
            "each area's canvas is a view of the graph"
        );
        for &id in &areas {
            let stats = harness
                .get_widget_with_id(id)
                .downcast::<NodeEditor<Graph>>()
                .expect("every area is an editor")
                .stats();
            assert_eq!(stats.total, NODES);
            assert!(stats.materialised > 0, "a canvas showing nodes built widgets for them");
        }

        // A link made by an operator in one area is in the model and in the other area.
        harness.edit_widget_with_id(areas[0], |mut widget| {
            let mut editor = widget.downcast::<NodeEditor<Graph>>();
            NodeEditor::exec(
                &mut editor,
                "link.add",
                &Props::new().with_int("from", 0).with_int("to", 4),
            );
        });
        let _ = harness.redraw();
        assert!(graph.borrow().links.contains(&Link::new(0, 4)));
        let in_other = harness.edit_widget_with_id(areas[1], |mut widget| {
            let mut editor = widget.downcast::<NodeEditor<Graph>>();
            NodeEditor::with_canvas(&mut editor, |mut canvas| {
                CanvasLayer::link_name(&mut canvas, Link::new(0, 4)).is_some()
            })
        });
        assert!(in_other, "the other area shows the link in the same frame (§30)");
    }

    /// A quick pull out of a port, whose first move is already past the port, is a link.
    ///
    /// The gesture is decided on the first move past the drag threshold and is about where
    /// the press was. Deciding it on what the pointer was over by then handed a fast drag
    /// out of a port to the view, which panned — seen on a render of this window, and
    /// invisible to the editor's own tests, whose single-widget harness did not move the
    /// hover while the button was down (§52). Here, in the shape the window builds.
    #[test]
    fn a_fast_pull_out_of_a_port_links_rather_than_pans() {
        use blazy::masonry::kurbo::Point;
        use blazy::masonry::ui_events::pointer::PointerButton;

        let graph = Graph::grid();
        let screen = app(&graph).screen(SplitTree::balanced(2));
        let mut harness =
            TestHarness::create_with_size(properties(), NewWidget::new(screen), PhysicalSize::new(1000, 600));
        let _ = harness.redraw();
        // Node 0 is at (40, 40), 140 by 60: its output is at (180, 60). Node 1 is at
        // (220, 40): its second input is at (220, 80).
        harness.mouse_move(Point::new(180.0, 60.0));
        harness.mouse_button_press(Some(PointerButton::Primary));
        harness.mouse_move(Point::new(190.0, 90.0));
        harness.mouse_move(Point::new(220.0, 80.0));
        harness.mouse_button_release(Some(PointerButton::Primary));
        let _ = harness.redraw();
        assert!(
            graph.borrow().links.contains(&Link::between(0, 0, 1, 1)),
            "the pull made a link, from node 0's output to node 1's second input"
        );
    }

    /// The editor of each area, in area order: the main region of the area's `AreaContent`,
    /// since every area here has a sidebar beside its editor.
    fn editor_ids(harness: &TestHarness<blazy::app::EditorScreen<Graph>>) -> Vec<WidgetId> {
        harness
            .root_widget()
            .area_ids()
            .into_iter()
            .map(|area| {
                let widget = harness.get_widget_with_id(area);
                match widget.downcast::<blazy::areas::AreaContent>() {
                    Some(content) => content.inner().region_ids()[0],
                    None => area,
                }
            })
            .collect()
    }

    /// The text field inside node `index` of area `area`, by id.
    fn title_field(
        harness: &mut TestHarness<blazy::app::EditorScreen<Graph>>,
        area: WidgetId,
        index: usize,
    ) -> WidgetId {
        harness.edit_widget_with_id(area, |mut widget| {
            let mut editor = widget.downcast::<NodeEditor<Graph>>();
            NodeEditor::with_canvas(&mut editor, |mut canvas| {
                let mut id = None;
                CanvasLayer::update_child(&mut canvas, index, |mut node| {
                    let mut node = node.downcast::<Node>();
                    let mut input = node.ctx.get_mut(&mut node.widget.title);
                    id = Some(TextInput::text_mut(&mut input).ctx.widget_id());
                });
                id.expect("the node is on screen")
            })
        })
    }

    fn text_of(harness: &TestHarness<blazy::app::EditorScreen<Graph>>, field: WidgetId) -> String {
        let widget = harness.get_widget_with_id(field);
        let area = widget.downcast::<TextArea<true>>().expect("a text field");
        area.inner().text().into_iter().collect()
    }

    /// A title typed into a node's field is the model's, and the other area shows it once
    /// it collects what it is owed — without the field being typed into rebuilt under the
    /// cursor (§47).
    #[test]
    fn a_title_typed_in_one_area_reaches_the_other() {
        let graph = Graph::grid();
        let screen = app(&graph).screen(SplitTree::balanced(2));
        let mut harness =
            TestHarness::create_with_size(properties(), NewWidget::new(screen), PhysicalSize::new(1000, 600));
        let _ = harness.redraw();
        let areas = editor_ids(&harness);

        let field = title_field(&mut harness, areas[0], 0);
        harness.focus_on(Some(field));
        harness.keyboard_type_chars("!");
        let _ = harness.redraw();
        let typed = text_of(&harness, field);
        assert!(typed.contains('!'), "{typed:?}");
        assert_eq!(graph.borrow().titles[0], typed, "the model holds what was typed");

        // What the window does before it draws: each view collects what it is owed.
        let views = graph.borrow().views().ids();
        for canvas in views {
            let graph = graph.clone();
            harness.edit_widget_with_id(canvas, |mut widget| {
                sync_canvas(&mut widget.downcast::<CanvasLayer>(), graph.borrow().views())
            });
        }
        let _ = harness.redraw();

        assert_eq!(
            harness.focused_widget_id(),
            Some(field),
            "the field typed into is still there, focused"
        );
        let other = title_field(&mut harness, areas[1], 0);
        assert_eq!(text_of(&harness, other), typed, "the other area shows the new title");
    }
    /// A duplicate is called what its original is called, in both areas, and an undone
    /// delete brings a node back under its name even when the name was handed out again
    /// in between — the title travels with the history, not in the graph's slot (§57).
    #[test]
    fn a_copy_keeps_the_title_and_an_undone_delete_brings_it_back() {
        let graph = Graph::grid();
        let screen = app(&graph).screen(SplitTree::balanced(2));
        let mut harness =
            TestHarness::create_with_size(properties(), NewWidget::new(screen), PhysicalSize::new(1000, 600));
        let _ = harness.redraw();
        let areas = editor_ids(&harness);
        graph.borrow_mut().titles[0] = "source".into();

        let exec = |harness: &mut TestHarness<blazy::app::EditorScreen<Graph>>, op: &str, props: Props| {
            harness.edit_widget_with_id(areas[0], |mut widget| {
                let mut editor = widget.downcast::<NodeEditor<Graph>>();
                NodeEditor::exec(&mut editor, op, &props);
            });
            let _ = harness.redraw();
        };
        exec(&mut harness, "node.select", Props::new().with_int("index", 0));
        exec(
            &mut harness,
            "node.duplicate",
            Props::new().with_float("dx", 0.0).with_float("dy", 400.0),
        );
        let copy = NODES;
        assert_eq!(graph.borrow().titles[copy], "source", "the copy is called the same");
        let field = title_field(&mut harness, areas[1], copy);
        assert_eq!(text_of(&harness, field), "source", "and the other area shows it so");

        // Delete the original, let `Shift+A` take its name, and undo both: the original
        // comes back with its title, not with the one the new node was given.
        exec(&mut harness, "node.delete", Props::new().with_int("index", 0));
        exec(&mut harness, "node.add", Props::new());
        assert_eq!(graph.borrow().titles[0], "node 0", "the name was handed out again");
        exec(&mut harness, "ed.undo", Props::new());
        exec(&mut harness, "ed.undo", Props::new());
        assert_eq!(graph.borrow().titles[0], "source");
    }

    /// A node is widened by its right edge, and its name field with it: the field is laid
    /// out across the node, so a longer name has room.
    #[test]
    fn a_node_dragged_by_its_right_edge_widens_and_its_name_field_with_it() {
        use blazy::masonry::kurbo::Point;
        use blazy::masonry::ui_events::pointer::PointerButton;

        let graph = Graph::grid();
        let screen = app(&graph).screen(SplitTree::balanced(2));
        let mut harness =
            TestHarness::create_with_size(properties(), NewWidget::new(screen), PhysicalSize::new(1000, 600));
        let _ = harness.redraw();
        let areas = editor_ids(&harness);
        let field_width = |harness: &mut TestHarness<blazy::app::EditorScreen<Graph>>| {
            let field = title_field(harness, areas[0], 0);
            harness.get_widget_with_id(field).ctx().border_box().width()
        };
        let before = field_width(&mut harness);

        // Node 0 is at (40, 40), 140 wide: its right edge is x = 180, its output (180, 60).
        harness.mouse_move(Point::new(177.0, 100.0));
        harness.mouse_button_press(Some(PointerButton::Primary));
        for x in [190.0, 210.0, 237.0] {
            harness.mouse_move(Point::new(x, 100.0));
        }
        harness.mouse_button_release(Some(PointerButton::Primary));
        let _ = harness.redraw();
        assert_eq!(graph.borrow().node_rect(0).width(), 200.0, "60 wider");
        assert_eq!(field_width(&mut harness), before + 60.0, "and the name field too");
    }

    /// Every widget of type `T` under `root`, depth first.
    fn find_all<T: Widget>(harness: &TestHarness<blazy::app::EditorScreen<Graph>>, root: WidgetId) -> Vec<WidgetId> {
        fn walk<T: Widget>(widget: blazy::masonry::core::WidgetRef<'_, dyn Widget>, out: &mut Vec<WidgetId>) {
            if widget.downcast::<T>().is_some() {
                out.push(widget.id());
            }
            for child in widget.children() {
                walk::<T>(child, out);
            }
        }
        let mut out = Vec::new();
        walk::<T>(harness.get_widget_with_id(root), &mut out);
        out
    }

    /// What a window does before it draws: each canvas and each panel collects its share.
    fn pull(harness: &mut TestHarness<blazy::app::EditorScreen<Graph>>, graph: &SharedGraph<Graph>) {
        let views = graph.borrow().views().clone();
        for canvas in views.ids() {
            harness.edit_widget_with_id(canvas, |mut widget| {
                sync_canvas(&mut widget.downcast::<CanvasLayer>(), &views)
            });
        }
        for (watcher, refresh) in views.owed_watchers() {
            harness.edit_widget_with_id(watcher, |mut widget| refresh(&mut widget));
        }
        let _ = harness.redraw();
    }

    /// The sidebar shows the selected node: a name typed there reaches the node in both
    /// areas without rebuilding the panel under the caret, and an arrow moves the node
    /// through `node.move`, so undo puts it back. Both areas' panels follow the selection.
    #[test]
    fn the_panel_shows_the_selected_node_and_edits_it() {
        use blazy::masonry::ui_events::pointer::PointerButton;

        let graph = Graph::grid();
        let screen = app(&graph).screen(SplitTree::balanced(2));
        let mut harness =
            TestHarness::create_with_size(properties(), NewWidget::new(screen), PhysicalSize::new(1000, 600));
        let _ = harness.redraw();
        // Show both sidebars, as `N` does.
        let area_widgets = harness.root_widget().area_ids();
        for (area, &id) in area_widgets.iter().enumerate() {
            let session = harness.root_widget().payload(area).expect("a session").clone();
            session.borrow().sidebar.set(blazy::areas::Sidebar::new(true, 240.0));
            harness.edit_widget_with_id(id, |mut widget| {
                blazy::areas::AreaContent::refresh(&mut widget.downcast::<blazy::areas::AreaContent>());
            });
        }
        let _ = harness.redraw();
        let editors = editor_ids(&harness);
        let panel_of = |harness: &TestHarness<blazy::app::EditorScreen<Graph>>, area: usize| {
            let content = harness.get_widget_with_id(area_widgets[area]);
            content
                .downcast::<blazy::areas::AreaContent>()
                .expect("an area with regions")
                .inner()
                .region_ids()[1]
        };
        let fields = |harness: &TestHarness<blazy::app::EditorScreen<Graph>>, area: usize| {
            // The editable text inside each field: what `text_of` reads and focus lands on.
            find_all::<TextArea<true>>(harness, panel_of(harness, area))
        };
        assert!(fields(&harness, 0).is_empty(), "nothing selected, no name to edit");

        // Select node 0 in the left area, on its body below the name.
        harness.mouse_move(Point::new(100.0, 108.0));
        harness.mouse_button_press(Some(PointerButton::Primary));
        harness.mouse_button_release(Some(PointerButton::Primary));
        let _ = harness.redraw();
        let name = fields(&harness, 0);
        assert_eq!(name.len(), 1, "the left panel shows the node");
        assert_eq!(text_of(&harness, name[0]), "node 0");
        assert_eq!(
            fields(&harness, 1).len(),
            1,
            "and so does the right one, in the same frame"
        );

        harness.focus_on(Some(name[0]));
        harness.keyboard_type_chars("!");
        pull(&mut harness, &graph);
        // Where the caret lands in a field focused from a test is the field's business.
        let typed = graph.borrow().titles[0].clone();
        assert!(
            typed.contains('!') && typed.len() == "node 0!".len(),
            "the model holds it: {typed:?}"
        );
        assert_eq!(
            harness.focused_widget_id(),
            Some(name[0]),
            "the field typed into is the one still there"
        );
        for (area, &editor) in editors.iter().enumerate() {
            let field = title_field(&mut harness, editor, 0);
            assert_eq!(text_of(&harness, field), typed, "area {area}'s node shows it");
        }
        let far = fields(&harness, 1)[0];
        assert_eq!(text_of(&harness, far), typed, "and the other panel");
        harness.focus_on(None);

        let right = find_all::<Button>(&harness, panel_of(&harness, 0))[1];
        harness.mouse_click_on(right, Some(PointerButton::Primary));
        pull(&mut harness, &graph);
        assert_eq!(graph.borrow().node_rect(0).x0, 40.0 + NUDGE, "the arrow moved it");
        harness.edit_widget_with_id(editors[0], |mut widget| {
            NodeEditor::<Graph>::exec(&mut widget.downcast(), "ed.undo", &Props::new())
        });
        pull(&mut harness, &graph);
        assert_eq!(graph.borrow().node_rect(0).x0, 40.0, "and undo put it back");
    }

    /// A copy from another application on blazy is not `hello`'s to read, though any text
    /// would make a title: its format name keeps it out. A copy in `hello`'s format is read.
    #[test]
    fn a_copy_in_another_applications_format_is_not_pasted() {
        use blazy::node_editor::{EditorWorld, ops};

        let graph = Graph::grid();
        let mut world = EditorWorld::new(&graph);
        let mut runtime = ops::runtime::<Graph>();
        let calc = "blazy-nodes 1 calc\nnode 40 40 150 90 number 2\nnode 260 60 150 90 add\nlink 0 0 1 0\n";
        graph.borrow().views().offer_clip_text(calc.into());
        assert_eq!(
            runtime.poll(&mut world, "node.paste", &Props::new()),
            Some(false),
            "nothing to paste: the calc copy is refused and no copy is in the process"
        );
        let ours = "blazy-nodes 1 hello-title\nnode 40 40 140 80 pasted\n";
        graph.borrow().views().offer_clip_text(ours.into());
        let nodes = graph.borrow().nodes.iter().flatten().count();
        let _ = runtime.exec(&mut world, "node.paste", &Props::new());
        assert_eq!(graph.borrow().nodes.iter().flatten().count(), nodes + 1);
        let pasted = graph.borrow().nodes.len() - 1;
        assert_eq!(graph.borrow().titles[pasted], "pasted");
    }
}
