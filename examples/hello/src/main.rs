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
//! Drag a node to move it, `B` to box select, `G` to grab, `Shift+A` to add, `X` to
//! delete, `F` to link the two selected nodes, `Ctrl+Z` to undo. `Alt+X` / `Alt+Y` split
//! the area under the pointer, `Alt+J` joins it with its sibling, `Alt+N` opens another
//! window over the same graph and `Alt+D` moves the area into one of its own.

// On Windows, don't open a console for the GUI mode.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::rc::Rc;

use blazy::app::EditorApp;
use blazy::areas::SplitTree;
use blazy::canvas::{CanvasLayer, Detail, Link, NodeSource};
use blazy::masonry::core::{DefaultProperties, NewWidget, PropertySet, PropertyStack, Selector, Widget, WidgetId};
use blazy::masonry::imaging::Painter;
use blazy::masonry::kurbo::{BezPath, Point, Rect, Shape, Size};
use blazy::masonry::layout::Length;
use blazy::masonry::peniko::Color;
use blazy::masonry::properties::{Background, BorderColor, BorderWidth, CornerRadius, Padding};
use blazy::masonry::theme::default_property_set;
use blazy::masonry::widgets::{Label, SizedBox};
use blazy::node_editor::{
    NodeEditor, NodeGraph, OverlayStyle, SELECTED, SelectionOutline, SharedGraph, ViewToken, Views,
};
use blazy::shell::window::{Error, WindowConfig};

/// Nodes the graph starts with.
const NODES: usize = 12;
/// Nodes per row of the grid they start on.
const COLS: usize = 4;
/// Every node's size.
const NODE: Size = Size::new(140.0, 60.0);

/// The graph: where each node is, which nodes are linked, and who is looking.
///
/// A name is an index with holes where removals were, and a freed name is handed out
/// again first — the rule `NodeGraph::insert_node` asks for (§43).
#[derive(Default)]
struct Graph {
    nodes: Vec<Option<Rect>>,
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

    fn views(&self) -> &Views {
        &self.views
    }

    fn insert_node(&mut self, rect: Rect) -> usize {
        match self.free.pop() {
            Some(index) => {
                self.nodes[index] = Some(rect);
                index
            },
            None => {
                self.nodes.push(Some(rect));
                self.nodes.len() - 1
            },
        }
    }

    fn restore_node(&mut self, index: usize, rect: Rect) {
        if self.nodes.len() <= index {
            self.nodes.resize(index + 1, None);
        }
        self.free.retain(|&free| free != index);
        self.nodes[index] = Some(rect);
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
        let known = self
            .links
            .iter()
            .any(|l| (l.from, l.to) == (link.from, link.to) || (l.from, l.to) == (link.to, link.from));
        if link.from == link.to || !live(link.from) || !live(link.to) || known {
            return false;
        }
        self.links.push(link);
        true
    }

    fn remove_link(&mut self, link: Link) {
        self.links
            .retain(|l| (l.from, l.to) != (link.from, link.to) && (l.from, l.to) != (link.to, link.from));
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

/// A node's fill, and its outline.
const FILL: Color = Color::from_rgb8(0x2c, 0x2c, 0x34);
const OUTLINE: Color = Color::from_rgb8(0x50, 0x50, 0x5c);
/// The outline of a selected node.
const PICKED: Color = Color::from_rgb8(0xff, 0xa5, 0x2c);

impl NodeSource for Nodes {
    /// A caption in a box. The box is a stock `SizedBox` and everything it looks like —
    /// selected or not — is in [`properties`]: the node does not know what a selection is.
    fn build(&mut self, index: usize, _detail: Detail) -> NewWidget<dyn Widget> {
        NewWidget::new(SizedBox::new(NewWidget::new(Label::new(format!("node {index}"))))).erased()
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

    fn attached(&mut self, canvas: WidgetId) {
        self.view = Some(self.graph.borrow().views().attach(canvas));
    }
}

/// The theme, plus what a node looks like — and what it looks like selected.
///
/// The node is a `SizedBox` because the theme has no style of its own for one: a
/// property stack replaces the theme's for its type, and a `Label` would lose every
/// label's. The box is in the stack rather than on each node because a property put on
/// the widget itself beats the stack, and the outline could then never change colour.
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
    properties.insert_stack::<SizedBox>(stack);
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
        let canvas = CanvasLayer::new(names, geometry, nodes).with_links(links);
        // The nodes look selected by themselves, so the editor outlines only the far
        // field, where there is no node widget to wear the class.
        let style = OverlayStyle {
            outline: SelectionOutline::FarField,
            ..OverlayStyle::default()
        };
        NewWidget::new(NodeEditor::with_session(canvas, session.clone()).with_style(style)).erased()
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

        let areas = harness.root_widget().area_ids();
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
}
