//! `calc`: an arithmetic node graph — the second application on blazy (§63).
//!
//! Written against the facade alone and without looking inside `hello`, to find out what an
//! application that did not grow up with the library has to do. Four kinds of node with
//! different ports and contents — a number you type, a sum, a product, a display — links
//! that refuse to make a cycle, a properties panel in each area's sidebar (`N`), the graph
//! and the screen saved with `Ctrl+S` and read back on the next start, and a second window.
//!
//! ```text
//! cargo make run-calc            # calc.graph and calc.workspace in the current directory
//! cargo make run-calc my.graph   # or a graph of your own, with my.workspace beside it
//! ```
//!
//! `Shift+A` opens the `Add` menu of the four kinds, from the graph's own list (§64).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use blazy::app::EditorApp;
use blazy::areas::{ScaleCarrier, SplitTree};
use blazy::canvas::{CanvasLayer, Detail, Link, NodeSource, PortLayout, Ports};
use blazy::masonry::accesskit::{Node as AccessNode, Role};
use blazy::masonry::core::{
    AccessCtx, ActionCtx, ChildrenIds, DefaultProperties, ErasedAction, LayoutCtx, MeasureCtx, NewWidget, NoAction,
    PaintCtx, PropertiesMut, PropertiesRef, PropertySet, PropertyStack, RegisterCtx, Selector, Widget, WidgetId,
    WidgetPod,
};
use blazy::masonry::imaging::Painter;
use blazy::masonry::kurbo::{Axis, Point, Rect, Size};
use blazy::masonry::layout::{LenDef, LenReq, Length, SizeDef};
use blazy::masonry::peniko::Color;
use blazy::masonry::properties::{Background, BorderColor, BorderWidth, CornerRadius, Padding};
use blazy::masonry::theme::default_property_set;
use blazy::masonry::widgets::{Label as StockLabel, TextAction, TextInput};
use blazy::node_editor::{
    Change, EdgePan, GraphPanel, NodeGraph, NodeKind, OverlayStyle, SELECTED, SelectionOutline, SharedGraph, ViewToken,
    Views,
};
use blazy::shell::window::{Error, WindowConfig};
use blazy::widgets::{Bar, Label, Metrics};

/// What a node is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// A number typed into the node.
    Number(f64),
    /// The sum of its two inputs.
    Add,
    /// The product of its two inputs.
    Multiply,
    /// Shows what reaches its one input.
    Show,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Number(_) => "number",
            Self::Add => "add",
            Self::Multiply => "multiply",
            Self::Show => "show",
        }
    }

    fn ports(self) -> Ports {
        let (inputs, outputs) = match self {
            Self::Number(_) => (0, 1),
            Self::Add | Self::Multiply => (2, 1),
            Self::Show => (1, 0),
        };
        Ports { inputs, outputs }
    }

    /// Whether what it shows follows from its inputs, so a change upstream changes it.
    fn computed(self) -> bool {
        !matches!(self, Self::Number(_))
    }
}

/// A node: where it is, and what it is.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Slot {
    rect: Rect,
    kind: Kind,
}

/// The graph: nodes by name, with holes a removal left, and links.
struct Calc {
    nodes: Vec<Option<Slot>>,
    /// Names a removal freed, handed out again before new ones.
    free: Vec<usize>,
    links: Vec<Link>,
    views: Views,
}

/// The size a new node gets.
const NODE: Size = Size::new(150.0, 90.0);
/// Where a node's ports are.
const PORT_LAYOUT: PortLayout = PortLayout {
    first: Some(30.0),
    step: 24.0,
    dot_radius: 4.0,
    dot_color: Color::from_rgb8(0x9a, 0x9a, 0xa8),
};
/// The magic word and version of the graph file.
const GRAPH_FORMAT: &str = "calc-graph 1";

impl Calc {
    /// Two numbers, their sum and their product, the product shown.
    fn sample() -> Self {
        let mut calc = Self {
            nodes: Vec::new(),
            free: Vec::new(),
            links: Vec::new(),
            views: Views::new(),
        };
        let at = |x: f64, y: f64| Rect::from_origin_size((x, y), NODE);
        for (rect, kind) in [
            (at(40.0, 40.0), Kind::Number(2.0)),
            (at(40.0, 180.0), Kind::Number(3.0)),
            (at(260.0, 60.0), Kind::Add),
            (at(260.0, 200.0), Kind::Multiply),
            (at(480.0, 130.0), Kind::Show),
        ] {
            calc.nodes.push(Some(Slot { rect, kind }));
        }
        calc.links = vec![
            Link::between(0, 0, 2, 0),
            Link::between(1, 0, 2, 1),
            Link::between(2, 0, 3, 0),
            Link::between(1, 0, 3, 1),
            Link::between(3, 0, 4, 0),
        ];
        calc
    }

    fn slot(&self, index: usize) -> Option<&Slot> {
        self.nodes.get(index).and_then(Option::as_ref)
    }

    fn kind(&self, index: usize) -> Option<Kind> {
        self.slot(index).map(|slot| slot.kind)
    }

    /// What node `index` puts out, or `None` where an input is missing.
    ///
    /// No cycle guard: `accepts_link` refuses every link that would close one.
    fn value(&self, index: usize) -> Option<f64> {
        let input = |port: u16| {
            self.links
                .iter()
                .find(|link| link.to as usize == index && link.to_port == port)
                .and_then(|link| self.value(link.from as usize))
        };
        match self.kind(index)? {
            Kind::Number(value) => Some(value),
            Kind::Add => Some(input(0)? + input(1)?),
            Kind::Multiply => Some(input(0)? * input(1)?),
            Kind::Show => input(0),
        }
    }

    /// Whether `to` can be reached from `from` along links.
    fn reaches(&self, from: usize, to: usize) -> bool {
        let mut stack = vec![from];
        let mut seen = BTreeSet::new();
        while let Some(at) = stack.pop() {
            if at == to {
                return true;
            }
            if seen.insert(at) {
                stack.extend(
                    self.links
                        .iter()
                        .filter(|l| l.from as usize == at)
                        .map(|l| l.to as usize),
                );
            }
        }
        false
    }

    /// Records that every computed node may show another value, for every view.
    ///
    /// The application's to do: the editor knows a link changed, not what follows from it.
    fn values_changed(&self) {
        for (index, slot) in self.nodes.iter().enumerate() {
            if slot.is_some_and(|slot| slot.kind.computed()) {
                self.views.note(Change::Contents { index });
            }
        }
    }

    /// The graph as text: one line per node and per link, names kept.
    fn write(&self) -> String {
        let mut out = format!("{GRAPH_FORMAT}\n");
        for (index, slot) in self.nodes.iter().enumerate() {
            let Some(Slot { rect, kind }) = slot else {
                continue;
            };
            let _ = write!(
                out,
                "node {index} {} {} {} {} {}",
                kind.name(),
                rect.x0,
                rect.y0,
                rect.width(),
                rect.height()
            );
            if let Kind::Number(value) = kind {
                let _ = write!(out, " {value}");
            }
            out.push('\n');
        }
        for link in &self.links {
            let _ = writeln!(
                out,
                "link {} {} {} {}",
                link.from, link.from_port, link.to, link.to_port
            );
        }
        out
    }

    /// Reads what [`write`](Self::write) wrote; the line it gave up on, from one, if not.
    fn read(text: &str) -> Result<Self, usize> {
        let mut lines = text.lines().enumerate().map(|(at, line)| (at + 1, line.trim()));
        if lines.next().map(|(_, line)| line) != Some(GRAPH_FORMAT) {
            return Err(1);
        }
        let mut calc = Self {
            nodes: Vec::new(),
            free: Vec::new(),
            links: Vec::new(),
            views: Views::new(),
        };
        for (number, line) in lines.filter(|(_, line)| !line.is_empty()) {
            let words: Vec<&str> = line.split_whitespace().collect();
            let float = |at: usize| {
                words
                    .get(at)
                    .and_then(|w| w.parse::<f64>().ok())
                    .filter(|v| v.is_finite())
            };
            match words.first() {
                Some(&"node") => {
                    let index: usize = words
                        .get(1)
                        .and_then(|w| w.parse().ok())
                        .filter(|&i| i < 100_000)
                        .ok_or(number)?;
                    let kind = match *words.get(2).ok_or(number)? {
                        "number" => Kind::Number(float(7).ok_or(number)?),
                        "add" => Kind::Add,
                        "multiply" => Kind::Multiply,
                        "show" => Kind::Show,
                        _ => return Err(number),
                    };
                    let (x, y, w, h) = (float(3), float(4), float(5), float(6));
                    let rect = Rect::from_origin_size(
                        (x.ok_or(number)?, y.ok_or(number)?),
                        (w.ok_or(number)?.max(1.0), h.ok_or(number)?.max(1.0)),
                    );
                    if calc.nodes.len() <= index {
                        calc.nodes.resize(index + 1, None);
                    }
                    calc.nodes[index] = Some(Slot { rect, kind });
                },
                Some(&"link") => {
                    let at = |i: usize| words.get(i).and_then(|w| w.parse::<u32>().ok()).ok_or(number);
                    let link = Link::between(at(1)? as usize, at(2)? as u16, at(3)? as usize, at(4)? as u16);
                    calc.links.push(link);
                },
                _ => return Err(number),
            }
        }
        // A link to a node the file does not hold is a file that lies; drop it rather than
        // keep a curve to nowhere.
        let links = std::mem::take(&mut calc.links);
        calc.links = links
            .into_iter()
            .filter(|l| calc.slot(l.from as usize).is_some() && calc.slot(l.to as usize).is_some())
            .collect();
        calc.free = (0..calc.nodes.len())
            .rev()
            .filter(|&i| calc.nodes[i].is_none())
            .collect();
        Ok(calc)
    }

    /// The node's rectangle, or `None` for a name a removal freed.
    fn node_rect_of(&self, index: usize) -> Option<(Point, Size)> {
        self.slot(index).map(|slot| (slot.rect.origin(), slot.rect.size()))
    }

    /// Takes a name: a freed one first.
    fn take_name(&mut self, slot: Slot) -> usize {
        match self.free.pop() {
            Some(index) => {
                self.nodes[index] = Some(slot);
                index
            },
            None => {
                self.nodes.push(Some(slot));
                self.nodes.len() - 1
            },
        }
    }
}

impl NodeGraph for Calc {
    type Content = Kind;

    fn node_count(&self) -> usize {
        self.nodes.len()
    }

    fn node_rect(&self, index: usize) -> Rect {
        self.slot(index).map_or(Rect::ZERO, |slot| slot.rect)
    }

    fn set_node_pos(&mut self, index: usize, pos: Point) {
        if let Some(Some(slot)) = self.nodes.get_mut(index) {
            slot.rect = Rect::from_origin_size(pos, slot.rect.size());
        }
    }

    fn set_node_rect(&mut self, index: usize, rect: Rect) {
        if let Some(Some(slot)) = self.nodes.get_mut(index) {
            slot.rect = rect;
        }
    }

    fn width_limits(&self, _index: usize) -> Option<(f64, f64)> {
        Some((110.0, 400.0))
    }

    fn views(&self) -> &Views {
        &self.views
    }

    /// A number: what `node.add` without a kind adds — from a script, say. `Shift+A` and the
    /// menu go through [`insert_kind`](Self::insert_kind).
    fn insert_node(&mut self, rect: Rect) -> usize {
        self.take_name(Slot {
            rect: Rect::from_origin_size(rect.origin(), NODE),
            kind: Kind::Number(0.0),
        })
    }

    /// The four kinds, by the names the graph file writes them under.
    fn node_kinds(&self) -> Vec<NodeKind> {
        [
            ("number", "Number"),
            ("add", "Add"),
            ("multiply", "Multiply"),
            ("show", "Show"),
        ]
        .into_iter()
        .map(|(name, label)| NodeKind::new(name, label))
        .collect()
    }

    /// A node of the kind named, at the one size every node starts at.
    fn insert_kind(&mut self, rect: Rect, kind: &str) -> Option<usize> {
        let kind = match kind {
            "number" => Kind::Number(0.0),
            "add" => Kind::Add,
            "multiply" => Kind::Multiply,
            "show" => Kind::Show,
            _ => return None,
        };
        Some(self.take_name(Slot {
            rect: Rect::from_origin_size(rect.origin(), NODE),
            kind,
        }))
    }

    fn restore_node(&mut self, index: usize, rect: Rect, kind: Kind) {
        if self.nodes.len() <= index {
            self.nodes.resize(index + 1, None);
        }
        self.free.retain(|&free| free != index);
        self.nodes[index] = Some(Slot { rect, kind });
    }

    fn remove_node(&mut self, index: usize) -> Vec<Link> {
        if let Some(slot) = self.nodes.get_mut(index)
            && slot.take().is_some()
        {
            self.free.push(index);
        }
        let (gone, kept) = self
            .links
            .iter()
            .partition(|l| l.from as usize == index || l.to as usize == index);
        self.links = kept;
        self.values_changed();
        gone
    }

    fn insert_link(&mut self, link: Link) -> bool {
        if !self.accepts_link(link) || self.links.contains(&link) {
            return false;
        }
        self.links.push(link);
        self.values_changed();
        true
    }

    /// Ports that exist, and no cycle: a value cannot follow from itself.
    fn accepts_link(&self, link: Link) -> bool {
        let (Some(from), Some(to)) = (self.kind(link.from as usize), self.kind(link.to as usize)) else {
            return false;
        };
        link.from_port < from.ports().outputs
            && link.to_port < to.ports().inputs
            && !self.reaches(link.to as usize, link.from as usize)
    }

    fn remove_link(&mut self, link: Link) {
        let before = self.links.len();
        self.links.retain(|l| *l != link);
        if self.links.len() != before {
            self.values_changed();
        }
    }

    fn content(&self, index: usize) -> Kind {
        self.kind(index).unwrap_or(Kind::Number(0.0))
    }

    fn insert_copy(&mut self, rect: Rect, kind: Kind) -> usize {
        self.take_name(Slot { rect, kind })
    }

    fn links_of(&self, index: usize) -> Vec<Link> {
        self.links
            .iter()
            .filter(|l| l.from as usize == index || l.to as usize == index)
            .copied()
            .collect()
    }
}

/// How a value reads, in a node and in the panel.
fn shown(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |value| format!("{value}"))
}

/// The canvas's side of the graph: a widget per node in view.
struct Nodes {
    graph: SharedGraph<Calc>,
    view: Option<ViewToken>,
}

impl NodeSource for Nodes {
    fn build(&mut self, index: usize, _detail: Detail) -> NewWidget<dyn Widget> {
        let graph = self.graph.borrow();
        let kind = graph.content(index);
        let body = match kind {
            Kind::Number(value) => NewWidget::new(NumberField::new(
                &self.graph,
                index,
                value,
                self.view.as_ref().map(ViewToken::canvas),
            ))
            .erased(),
            _ => NewWidget::new(StockLabel::new(shown(graph.value(index)))).erased(),
        };
        NewWidget::new(CalcNode {
            title: NewWidget::new(StockLabel::new(kind.name())).erased().to_pod(),
            body: body.to_pod(),
            kind,
        })
        .erased()
    }

    fn ports(&mut self, index: usize) -> Ports {
        self.graph.borrow().content(index).ports()
    }

    fn attached(&mut self, canvas: WidgetId) {
        self.view = Some(self.graph.borrow().views().attach(canvas));
    }
}

/// A field that edits a number node's value — in the node, and in the panel.
///
/// One widget for both places, told apart by the view its edits are recorded past: the
/// canvas the node is in, or the panel. What follows from the value — every computed node
/// — is recorded for every view.
struct NumberField {
    input: WidgetPod<TextInput>,
    graph: SharedGraph<Calc>,
    index: usize,
    /// The view that shows the edit already, and must not be rebuilt under the caret.
    past: Option<WidgetId>,
}

impl NumberField {
    fn new(graph: &SharedGraph<Calc>, index: usize, value: f64, past: Option<WidgetId>) -> Self {
        Self {
            input: WidgetPod::new(TextInput::new(&format!("{value}"))),
            graph: graph.clone(),
            index,
            past,
        }
    }
}

impl Widget for NumberField {
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
        let context = blazy::masonry::layout::LayoutSize::default();
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
                // Half-typed text that is not a number yet leaves the value as it was.
                if let Ok(value) = text.trim().parse::<f64>()
                    && let Some(Some(slot)) = graph.nodes.get_mut(self.index)
                    && value.is_finite()
                {
                    slot.kind = Kind::Number(value);
                    let change = Change::Contents { index: self.index };
                    match self.past {
                        Some(view) => graph.views.note_except(view, change),
                        None => graph.views.note(change),
                    }
                    graph.values_changed();
                }
            },
            TextAction::Entered(_) | TextAction::Cancelled => ctx.resign_focus(),
        }
        ctx.set_handled();
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

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut AccessNode) {}
}

/// A node: its kind across the top, its value below, its ports' dots on its edges.
struct CalcNode {
    title: WidgetPod<dyn Widget>,
    body: WidgetPod<dyn Widget>,
    kind: Kind,
}

impl Widget for CalcNode {
    type Action = NoAction;

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
            _ => Length::px(match axis {
                Axis::Horizontal => NODE.width,
                Axis::Vertical => NODE.height,
            }),
        }
    }

    /// The kind, then the value, each at its own height across the node; the rest of the
    /// box is what a drag takes the node by.
    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        let auto = SizeDef::new(LenDef::Fixed(Length::px(size.width)), LenDef::MaxContent);
        let title = ctx.compute_size(&mut self.title, auto, size.into());
        ctx.run_layout(&mut self.title, title);
        ctx.place_child(&mut self.title, Point::ORIGIN);
        let body = ctx.compute_size(&mut self.body, auto, size.into());
        ctx.run_layout(&mut self.body, body);
        ctx.place_child(&mut self.body, Point::new(0.0, title.height + 4.0));
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn post_paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        PORT_LAYOUT.paint_ports(painter, ctx.border_box(), self.kind.ports());
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.title);
        ctx.register_child(&mut self.body);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.title.id(), self.body.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut AccessNode) {}
}

/// What the panel shows: the selected node's kind, value and place.
fn panel_of(graph: &SharedGraph<Calc>, selection: &BTreeSet<usize>, panel: WidgetId) -> NewWidget<dyn Widget> {
    let metrics = Metrics {
        padding: 4.0,
        gap: 6.0,
        ..Metrics::default()
    };
    let row = |bar: Bar| NewWidget::new(bar).with_props(ScaleCarrier::of::<Bar>()).erased();
    let caption = |text: String| NewWidget::new(Label::new(text)).erased();
    let line = |name: &str, value: NewWidget<dyn Widget>| {
        row(Bar::new(metrics)
            .with(caption(name.into()), metrics)
            .with(value, metrics))
    };
    let mut column = Bar::column(metrics);
    let calc = graph.borrow();
    match selection.iter().copied().collect::<Vec<_>>()[..] {
        [index] if calc.slot(index).is_some() => {
            let kind = calc.content(index);
            let value = match kind {
                Kind::Number(value) => NewWidget::new(NumberField::new(graph, index, value, Some(panel))).erased(),
                _ => caption(shown(calc.value(index))),
            };
            let rect = calc.node_rect(index);
            column = column
                .with(line("Kind", caption(kind.name().into())), metrics)
                .with(line("Value", value), metrics)
                .with(
                    line("Position", caption(format!("{:.0}, {:.0}", rect.x0, rect.y0))),
                    metrics,
                );
        },
        _ => {
            column = column.with(
                row(Bar::new(metrics).with(caption("Select one node".into()), metrics)),
                metrics,
            )
        },
    }
    NewWidget::new(column).with_props(ScaleCarrier::of::<Bar>()).erased()
}

/// A node's box, and its outline when selected.
fn properties() -> DefaultProperties {
    let mut properties = default_property_set();
    let mut stack = PropertyStack::new();
    stack.push_layer(
        Selector::new(),
        PropertySet::new()
            .with(Background::Color(Color::from_rgb8(0x26, 0x30, 0x2c)))
            .with(BorderColor {
                color: Color::from_rgb8(0x4a, 0x5c, 0x54),
            })
            .with(BorderWidth { width: Length::px(1.0) })
            .with(CornerRadius {
                radius: Length::px(4.0),
            })
            .with(Padding::all(Length::px(6.0))),
    );
    stack.push_layer(Selector::classes(&[SELECTED]), BorderColor {
        color: Color::from_rgb8(0xff, 0xa5, 0x2c),
    });
    properties.insert_stack::<CalcNode>(stack);
    properties
}

/// The application over `graph`, saving the graph to `file` and the screen beside it.
fn app(graph: &SharedGraph<Calc>, file: &Path) -> EditorApp<Calc> {
    let graph_of_areas = graph.clone();
    let file_to_save = file.to_path_buf();
    EditorApp::new(graph, move |_area, session| {
        let graph = graph_of_areas.clone();
        let (names, links) = {
            let calc = graph.borrow();
            (calc.nodes.len(), calc.links.clone())
        };
        let geometry = {
            let graph = graph.clone();
            move |index: usize| graph.borrow().node_rect_of(index)
        };
        let canvas = CanvasLayer::new(names, geometry, Nodes { graph, view: None })
            .with_links(links)
            .with_ports(PORT_LAYOUT);
        let style = OverlayStyle {
            outline: SelectionOutline::FarField,
            ..OverlayStyle::default()
        };
        NewWidget::new(blazy::node_editor::NodeEditor::with_session(canvas, session.clone()).with_style(style)).erased()
    })
    .with_edge_pan(EdgePan::default())
    .with_sidebar(|_area, session| {
        let graph = session.borrow().world.graph.clone();
        NewWidget::new(GraphPanel::new(&graph, panel_of))
            .with_props(ScaleCarrier::of::<GraphPanel<Calc>>())
            .erased()
    })
    .with_workspace(file.with_extension("workspace"))
    .on_save(
        move |graph| match std::fs::write(&file_to_save, graph.borrow().write()) {
            Ok(()) => eprintln!("saved {}", file_to_save.display()),
            Err(error) => eprintln!("{}: {error}", file_to_save.display()),
        },
    )
}

/// The graph in `file`, or the sample if there is none yet.
fn open(file: &Path) -> Calc {
    match std::fs::read_to_string(file) {
        Ok(text) => Calc::read(&text).unwrap_or_else(|line| {
            eprintln!("{}: line {line} could not be read", file.display());
            std::process::exit(2);
        }),
        Err(_) => Calc::sample(),
    }
}

fn main() -> Result<(), Error> {
    let file = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("calc.graph"), PathBuf::from);
    let graph = std::rc::Rc::new(std::cell::RefCell::new(open(&file)));
    let config = WindowConfig::default()
        .with_title("blazy - calc")
        .with_size(1100.0, 650.0);
    app(&graph, &file)
        .with_properties(properties())
        .with_windows(2)
        .run(config, SplitTree::balanced(2))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use blazy::app::EditorScreen;
    use blazy::masonry::dpi::PhysicalSize;
    use blazy::masonry::testing::TestHarness;
    use blazy::masonry::widgets::TextArea;
    use blazy::node_editor::sync_canvas;

    use super::*;

    /// The sample computes: (2 + 3) × 3 = 15 at the display.
    #[test]
    fn the_sample_computes() {
        let calc = Calc::sample();
        assert_eq!(calc.value(2), Some(5.0));
        assert_eq!(calc.value(4), Some(15.0));
    }

    /// A link that would close a cycle, or name a port a node does not have, is refused.
    #[test]
    fn a_cycle_and_a_missing_port_are_refused() {
        let mut calc = Calc::sample();
        assert!(!calc.insert_link(Link::between(3, 0, 2, 0)), "add <- multiply <- add");
        assert!(!calc.insert_link(Link::between(4, 0, 2, 0)), "show has no output");
        assert!(!calc.insert_link(Link::between(0, 0, 0, 0)), "nothing into a number");
    }

    /// The file gives back the graph: nodes under their names, holes included, values and
    /// links — and a file that is not one is refused at the line it goes wrong.
    #[test]
    fn the_graph_survives_its_file() {
        let mut calc = Calc::sample();
        let _ = calc.remove_node(1);
        calc.set_node_rect(0, Rect::new(10.0, 20.0, 200.0, 110.0));
        let read = Calc::read(&calc.write()).expect("it reads back");
        assert_eq!(read.nodes, calc.nodes);
        assert_eq!(read.links, calc.links);
        assert_eq!(read.free, vec![1], "the hole is a name to hand out again");
        assert_eq!(Calc::read("something else"), Err(1));
        assert_eq!(Calc::read("calc-graph 1\nnode 0 cube 0 0 1 1\n"), Err(2));
    }

    impl PartialEq for Calc {
        fn eq(&self, other: &Self) -> bool {
            self.nodes == other.nodes && self.links == other.links
        }
    }

    impl std::fmt::Debug for Calc {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.write())
        }
    }

    /// Every text field under `root`.
    fn fields(harness: &TestHarness<EditorScreen<Calc>>, root: WidgetId) -> Vec<WidgetId> {
        fn walk(widget: blazy::masonry::core::WidgetRef<'_, dyn Widget>, out: &mut Vec<WidgetId>) {
            if widget.downcast::<TextArea<true>>().is_some() {
                out.push(widget.id());
            }
            for child in widget.children() {
                walk(child, out);
            }
        }
        let mut out = Vec::new();
        walk(harness.get_widget_with_id(root), &mut out);
        out
    }

    /// Every caption under `root`, as text.
    fn captions(harness: &TestHarness<EditorScreen<Calc>>, root: WidgetId) -> Vec<String> {
        fn walk(widget: blazy::masonry::core::WidgetRef<'_, dyn Widget>, out: &mut Vec<String>) {
            if let Some(label) = widget.downcast::<StockLabel>() {
                out.push(label.inner().text().to_string());
            }
            for child in widget.children() {
                walk(child, out);
            }
        }
        let mut out = Vec::new();
        walk(harness.get_widget_with_id(root), &mut out);
        out
    }

    /// What a window does before it draws.
    fn pull(harness: &mut TestHarness<EditorScreen<Calc>>, graph: &SharedGraph<Calc>) {
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

    /// A number typed into a node reaches the display in both areas, through the graph.
    #[test]
    fn a_number_typed_in_one_area_changes_the_display_in_both_and_is_saved() {
        let graph = Rc::new(RefCell::new(Calc::sample()));
        let file = std::env::temp_dir().join(format!("calc-{}.graph", std::process::id()));
        let screen = app(&graph, &file).screen(SplitTree::balanced(2));
        let mut harness =
            TestHarness::create_with_size(properties(), NewWidget::new(screen), PhysicalSize::new(1200, 600));
        let _ = harness.redraw();
        let areas = harness.root_widget().area_ids();

        // The first field of the left area is node 0's, the number 2.
        let field = fields(&harness, areas[0])[0];
        harness.focus_on(Some(field));
        harness.keyboard_type_chars("1");
        pull(&mut harness, &graph);
        // Where the caret lands is the field's business: "12" or "21".
        let Kind::Number(typed) = graph.borrow().content(0) else {
            panic!("node 0 is a number");
        };
        assert!(typed == 12.0 || typed == 21.0, "{typed}");
        let expected = shown(Some((typed + 3.0) * 3.0));
        for &area in &areas {
            assert!(
                captions(&harness, area).contains(&expected),
                "area {area:?} shows {expected}: {:?}",
                captions(&harness, area)
            );
        }
        assert_eq!(
            harness.focused_widget_id(),
            Some(field),
            "the field typed into is still there"
        );
    }

    /// `Ctrl+S` writes the graph beside the workspace; the next start reads the graph and
    /// brings the screen back as it was left — a sidebar shown in the right-hand area.
    #[test]
    fn a_saved_graph_and_screen_come_back_on_the_next_start() {
        use blazy::app::ScreenAction;
        use blazy::masonry::app::{RenderRoot, RenderRootOptions, WindowSizePolicy};
        use blazy::shell::window::{ShellCtx, ShellDriver};

        let file = std::env::temp_dir().join(format!("calc-restart-{}.graph", std::process::id()));
        let window = |app: &EditorApp<Calc>| {
            RenderRoot::new(
                NewWidget::new(app.screen(SplitTree::balanced(2))).erased(),
                |_signal| {},
                RenderRootOptions {
                    default_properties: std::sync::Arc::new(properties()),
                    use_system_fonts: false,
                    size_policy: WindowSizePolicy::User,
                    size: PhysicalSize::new(1200, 600),
                    scale_factor: 1.0,
                    test_font: None,
                },
            )
        };
        let sidebar_of_right = |root: &mut RenderRoot| {
            root.edit_base_layer(|mut widget| {
                let screen = widget.downcast::<EditorScreen<Calc>>();
                let session = screen.widget.payload(1).expect("two areas").clone();
                session.borrow().sidebar.get()
            })
        };

        // The first run: change the graph, show a sidebar, save.
        let graph = Rc::new(RefCell::new(Calc::sample()));
        let _ = graph.borrow_mut().remove_node(3);
        let mut first = app(&graph, &file);
        let mut root = window(&first);
        let mut cx = ShellCtx::new();
        let key = cx.name_window();
        first.started(&mut cx, key, &mut root);
        root.edit_base_layer(|mut widget| {
            let screen = widget.downcast::<EditorScreen<Calc>>();
            let session = screen.widget.payload(1).expect("two areas").clone();
            session.borrow().sidebar.set(blazy::areas::Sidebar::new(true, 300.0));
        });
        let _ = first.perform(&mut cx, &mut root, ScreenAction::SaveWorkspace);

        // The next: the graph from the file, and the screen as the workspace file left it.
        let read = open(&file);
        assert_eq!(read, *graph.borrow(), "the graph is the one saved");
        let graph = Rc::new(RefCell::new(read));
        let mut next = app(&graph, &file);
        let mut root = window(&next);
        let mut cx = ShellCtx::new();
        let key = cx.name_window();
        assert!(!sidebar_of_right(&mut root).shown, "built hidden, as every area is");
        next.started(&mut cx, key, &mut root);
        assert_eq!(
            sidebar_of_right(&mut root),
            blazy::areas::Sidebar::new(true, 300.0),
            "and brought back"
        );

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(file.with_extension("workspace"));
    }

    /// `Shift+A`'s menu lists the four kinds; `Show` adds a display, seen in both areas at the
    /// size every node starts at.
    #[test]
    fn the_add_menu_adds_the_kind_chosen_in_both_areas() {
        use blazy::masonry::ui_events::pointer::PointerButton;
        use blazy::node_editor::{MenuLayer, NodeEditor};
        use blazy::ops::keymap::Props;

        let graph = Rc::new(RefCell::new(Calc::sample()));
        let file = std::env::temp_dir().join(format!("calc-add-{}.graph", std::process::id()));
        let screen = app(&graph, &file).screen(SplitTree::balanced(2));
        let mut harness =
            TestHarness::create_with_size(properties(), NewWidget::new(screen), PhysicalSize::new(1200, 600));
        let _ = harness.redraw();
        let editors: Vec<WidgetId> = (0..2)
            .map(|area| {
                let session = harness.root_widget().payload(area).expect("a session").clone();
                session.borrow().editor.expect("an editor")
            })
            .collect();
        let shows = |graph: &SharedGraph<Calc>| {
            graph
                .borrow()
                .nodes
                .iter()
                .flatten()
                .filter(|slot| slot.kind == Kind::Show)
                .count()
        };
        assert_eq!(shows(&graph), 1);

        harness.mouse_move(Point::new(300.0, 450.0));
        harness.edit_widget_with_id(editors[0], |mut widget| {
            NodeEditor::<Calc>::exec(&mut widget.downcast(), blazy::node_editor::ops::ADD_MENU, &Props::new())
        });
        let _ = harness.redraw();
        let menu = harness
            .get_widget_with_id(editors[0])
            .downcast::<NodeEditor<Calc>>()
            .expect("the editor")
            .inner()
            .open_menu()
            .expect("the Add menu is open");
        let entries: Vec<WidgetId> = harness
            .get_widget_with_id(menu)
            .downcast::<MenuLayer<Calc>>()
            .expect("a menu")
            .inner()
            .entries()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(entries.len(), 4, "one entry per kind");
        // Unchecked: the harness looks for a widget under the pointer in the base layer only,
        // and a menu is a layer of its own. A click, which in a window of two areas used to
        // spill past the event's rewrite passes (§65).
        harness.mouse_move_to_unchecked(entries[3]);
        harness.mouse_button_press(Some(PointerButton::Primary));
        harness.mouse_button_release(Some(PointerButton::Primary));
        pull(&mut harness, &graph);

        assert_eq!(shows(&graph), 2, "a display was added");
        let added = graph
            .borrow()
            .nodes
            .iter()
            .rposition(|slot| slot.is_some_and(|s| s.kind == Kind::Show));
        let added = added.expect("there");
        assert_eq!(
            graph.borrow().node_rect(added).size(),
            NODE,
            "at the size every node starts at"
        );
        let views = graph.borrow().views().ids();
        assert_eq!(views.len(), 2);
        for canvas in views {
            let live = harness.edit_widget_with_id(canvas, |mut widget| {
                CanvasLayer::live_children(&mut widget.downcast::<CanvasLayer>())
            });
            assert!(live.iter().any(|&(index, _)| index == added), "both areas show it");
        }
    }
}
