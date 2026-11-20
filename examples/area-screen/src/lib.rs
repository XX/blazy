//! Phase 0.5 feasibility experiment: a window tiled into Blender-style areas.
//!
//! A Blender screen puts several editors in one window, each with regions of its own
//! interface scale. The questions:
//!
//! * do the areas' costs add up, or does splitting a window merely divide the same viewport into smaller pieces?
//! * does dragging a splitter re-lay-out the screen, or only what it moved?
//! * does a region's `ui_scale` reach layout, and does it stay inside its region?
//! * does zooming a region's content stay out of layout entirely?
//!
//! ```text
//! cargo make run-area-screen      # interactive window
//! cargo make bench-areas          # headless measurements and the criteria
//! ```
//!
//! Every area holds a node canvas over one shared graph, under a header with its own
//! `ui_scale`; the window opens with a different scale per area.

pub mod header;

#[cfg(test)]
mod tests;

use blazy::areas::{AreaContent, AreaId, AreaScreen, SplitTree};
use blazy::canvas::DetailBudget;
use blazy::masonry::app::RenderRoot;
use blazy::masonry::core::{NewWidget, Widget, WidgetMut};
use blazy::masonry::peniko::Color;
use blazy::node_editor::{EditorSession, SessionHandle};
use node_canvas::CanvasSpec;
use node_canvas::editor::NodeEditor;
use node_canvas::model::{GraphModel, SharedGraph, share};

use crate::header::ScaledHeader;

/// Height of a region header at `ui_scale` 1.0, in logical pixels.
pub const HEADER_HEIGHT: f64 = 24.0;

/// Default area count. Roughly what a working Blender screen carries.
pub const DEFAULT_AREAS: usize = 8;

/// Interface scales a staggered screen hands out, cycled over the areas.
pub const STAGGERED_SCALES: [f64; 4] = [1.0, 1.25, 1.5, 1.75];

/// How the headers of a screen are scaled.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum HeaderScale {
    /// Every header at 1.0.
    #[default]
    Uniform,
    /// A different scale per area, cycling [`STAGGERED_SCALES`]: what the window opens with.
    Staggered,
    /// One scale for every header, whatever the area.
    Forced(f64),
}

impl HeaderScale {
    fn of(self, area: usize) -> f64 {
        match self {
            Self::Uniform => 1.0,
            Self::Staggered => STAGGERED_SCALES[area % STAGGERED_SCALES.len()],
            Self::Forced(scale) => scale,
        }
    }
}

/// How a screen of areas is put together for a window, a test or a measurement.
#[derive(Clone, Debug)]
pub struct ScreenSpec {
    /// Areas the window is tiled into.
    pub areas: usize,
    /// Nodes in the graph every area shows.
    pub nodes: usize,
    /// Widget budget for the whole window, or `None` for the default split by area.
    pub budget_widgets: Option<usize>,
    /// Whether each area carries a header region above its canvas.
    pub header: bool,
    /// Whether each area asks to be its own scene layer. Off by default.
    pub isolated: bool,
    /// How the headers are scaled.
    pub header_scale: HeaderScale,
    /// Whether each area's canvas is wrapped in the operator layer's driver. Off by default.
    pub ops: bool,
}

impl ScreenSpec {
    #[must_use]
    pub fn new(areas: usize, nodes: usize) -> Self {
        Self {
            areas,
            nodes,
            budget_widgets: None,
            header: true,
            isolated: false,
            header_scale: HeaderScale::Uniform,
            ops: false,
        }
    }

    /// Wraps every area's canvas in the operator layer's driver.
    #[must_use]
    pub fn with_ops(mut self, ops: bool) -> Self {
        self.ops = ops;
        self
    }

    #[must_use]
    pub fn with_budget(mut self, widgets: Option<usize>) -> Self {
        self.budget_widgets = widgets;
        self
    }

    #[must_use]
    pub fn without_header(mut self) -> Self {
        self.header = false;
        self
    }

    #[must_use]
    pub fn with_isolated_layers(mut self, isolated: bool) -> Self {
        self.isolated = isolated;
        self
    }

    #[must_use]
    pub fn with_header_scale(mut self, scale: HeaderScale) -> Self {
        self.header_scale = scale;
        self
    }

    /// Builds the screen, and hands back the graph alongside it.
    pub fn build(self) -> (Screen, SharedGraph) {
        let graph = share(GraphModel::generated(self.nodes));
        let screen = self.over(&graph);
        (screen, graph)
    }

    /// Builds a screen over a graph that already exists (a second window).
    pub fn over(self, graph: &SharedGraph) -> Screen {
        self.over_with(graph, None)
    }

    /// The same, with the first area taking over a session that already exists (detach).
    pub fn over_with(self, graph: &SharedGraph, carried: Option<AreaSession>) -> Screen {
        // The screen keeps the builder for splits.
        let building = graph.clone();
        let mut carried = carried;
        AreaScreen::with_payloads(SplitTree::balanced(self.areas), move |area| {
            // The carried session goes to the first area only.
            let session = carried.take().unwrap_or_else(|| EditorSession::new(&building).share());
            (self.area(&building, area, &session), session)
        })
    }

    /// What fills one area of a screen built to this spec, showing `session`: shared by the
    /// window (through `EditorApp`), the tests and the benchmarks.
    pub fn area(&self, graph: &SharedGraph, area: AreaId, session: &AreaSession) -> NewWidget<dyn Widget> {
        let budget = window_budget(self.budget_widgets, self.areas);
        let canvas = if self.ops {
            area_editor(graph, self.nodes, budget, session)
        } else {
            area_canvas(graph, self.nodes, budget)
        };
        let content = if self.header {
            AreaContent::header_and_main(HEADER_HEIGHT, area_header(area), canvas)
                .with_ui_scale(0, self.header_scale.of(area))
        } else {
            AreaContent::new(vec![(blazy::areas::RegionKind::Main, 0.0, canvas)])
        };
        NewWidget::new(content.with_isolated_layer(self.isolated)).erased()
    }
}

/// The screen the benchmarks measure: headers, uniform scale, no layers.
pub fn build_screen(areas: usize, nodes: usize, budget_widgets: Option<usize>) -> (Screen, SharedGraph) {
    ScreenSpec::new(areas, nodes).with_budget(budget_widgets).build()
}

/// The share of the window's widget budget one area gets: an even share, since a frame
/// walks the whole window's tree.
pub fn window_budget(widgets: Option<usize>, areas: usize) -> DetailBudget {
    widgets
        .map(|widgets| DetailBudget {
            widgets,
            ..Default::default()
        })
        .unwrap_or_else(|| DetailBudget::default().split(areas))
}

/// What an area of this application carries beside its widget: the session its editor
/// shows (every area has one).
pub type AreaSession = SessionHandle<GraphModel>;

/// A screen of this application: areas that carry their sessions.
pub type Screen = AreaScreen<AreaSession>;

/// The canvas inside an area, as a `dyn Widget`, holding `budget` of the window's
/// widgets.
pub fn area_canvas(graph: &SharedGraph, nodes: usize, budget: DetailBudget) -> NewWidget<dyn Widget> {
    NewWidget::new(CanvasSpec::new(nodes).over(graph).with_budget(budget)).erased()
}

/// The same canvas with the operator layer's driver around it, one per area.
pub fn area_editor(
    graph: &SharedGraph,
    nodes: usize,
    budget: DetailBudget,
    session: &AreaSession,
) -> NewWidget<dyn Widget> {
    let canvas = CanvasSpec::new(nodes).over(graph).with_budget(budget);
    NewWidget::new(node_canvas::editor::with_session(canvas, session.clone())).erased()
}

/// The header of area `area`, tinted so the areas are told apart by eye.
pub fn area_header(area: usize) -> NewWidget<dyn Widget> {
    const TINTS: [Color; 4] = [
        Color::from_rgb8(0x6b, 0x4b, 0x8a),
        Color::from_rgb8(0x3c, 0x6e, 0x71),
        Color::from_rgb8(0x8a, 0x5a, 0x3c),
        Color::from_rgb8(0x44, 0x6b, 0x3c),
    ];
    NewWidget::new(ScaledHeader::new(TINTS[area % TINTS.len()]))
        .with_props(blazy::areas::ScaleCarrier::of::<ScaledHeader>())
        .erased()
}

/// Takes an area out of a screen, ready to be built in another window — and cancels
/// whatever was modal in its session. The library's since the assembly moved there.
pub use blazy::app::detach_area;

/// Brings one window's canvases up to date with the graph (`blazy::node_editor::sync_root`),
/// and returns how many changes were applied.
pub fn sync_window(root: &mut RenderRoot, graph: &SharedGraph) -> usize {
    // Cloned: applying a change may read the graph.
    let views = graph.borrow().views().clone();
    blazy::node_editor::sync_root(root, &views)
}

/// Brings one editor up to date, and says how many changes that took (for a harness).
pub fn sync_editor(editor: &mut WidgetMut<'_, NodeEditor>, graph: &SharedGraph) -> usize {
    let views = graph.borrow().views().clone();
    NodeEditor::with_canvas(editor, |mut canvas| {
        blazy::node_editor::sync_canvas(&mut canvas, &views)
    })
}
