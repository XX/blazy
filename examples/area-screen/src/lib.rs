//! Phase 0.5 feasibility experiment: a window tiled into Blender-style areas.
//!
//! Phase 0 measured one editor filling one window and found that frame cost is the
//! cost of walking the widget tree, which is a per-window quantity
//! (`rnd/architecture.md` §20.2). A Blender screen is six or eight editors in that
//! same window, and each of those editors is itself divided into regions with their
//! own interface scale. This experiment asks the questions that leaves open:
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
//! Every area holds a node canvas over **one shared graph**, so the numbers line up
//! with Phase 0's and so the sweep over area counts changes only the tiling. Above
//! each canvas sits a header region that honours its own `ui_scale`; the window opens
//! with a different scale per area, which is the shortest way to see what "per-region"
//! means.

pub mod header;

#[cfg(test)]
mod tests;

use blazy::areas::{AreaContent, AreaScreen, SplitTree};
use blazy::canvas::DetailBudget;
use blazy::masonry::core::{NewWidget, Widget};
use blazy::masonry::peniko::Color;
use node_canvas::CanvasSpec;
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
    /// A different scale per area, cycling [`STAGGERED_SCALES`].
    ///
    /// What the window opens with. A screenshot of one staggered screen says what
    /// per-region `ui_scale` means more directly than any number does: the same header,
    /// built from the same widget, at four sizes in one window, while the canvases below
    /// them are untouched.
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
///
/// One description with defaults rather than a constructor per combination — the same
/// reason [`node_canvas::CanvasSpec`] exists: header or not, staggered scales or not,
/// scene layers or not are independent axes, and a function per combination grows as
/// their product.
#[derive(Clone, Debug)]
pub struct ScreenSpec {
    /// Areas the window is tiled into.
    pub areas: usize,
    /// Nodes in the graph every area shows.
    pub nodes: usize,
    /// Widget budget for the whole window, or `None` for the default split by area.
    pub budget_widgets: Option<usize>,
    /// Whether each area carries a header region above its canvas.
    ///
    /// The headerless form is what the sweep over region counts compares against: one
    /// region per area, so the difference between the two is the price of a region and
    /// nothing else.
    pub header: bool,
    /// Whether each area asks to be its own scene layer (§36).
    ///
    /// Off by default, because the layer is only worth its price to a host that caches
    /// layers, and the price — a repaint request per area per frame — is paid whether or
    /// not anyone caches.
    pub isolated: bool,
    /// How the headers are scaled.
    pub header_scale: HeaderScale,
    /// Whether each area's canvas is wrapped in the operator layer's driver (§38).
    ///
    /// Off by default, and that is not only about keeping the older measurements
    /// still: with operators an area holds a `NodeEditor`, and what that changes about
    /// a frame — one more widget, an overlay in `post_paint` — has to be visible as a
    /// row of its own rather than folded into every number in the file.
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
    ///
    /// The canvases keep only a shared borrow, and the model is the source of truth that
    /// outlives every view (§30).
    pub fn build(self) -> (AreaScreen, SharedGraph) {
        let graph = share(GraphModel::generated(self.nodes));
        let budget = window_budget(self.budget_widgets, self.areas);
        // The builder outlives this call: the screen keeps it so a split can ask for the
        // widget of an area that does not exist yet.
        let building = graph.clone();
        let screen = AreaScreen::new(SplitTree::balanced(self.areas), move |area| {
            let canvas = if self.ops {
                area_editor(&building, self.nodes, budget)
            } else {
                area_canvas(&building, self.nodes, budget)
            };
            let content = if self.header {
                AreaContent::header_and_main(HEADER_HEIGHT, area_header(area), canvas)
                    .with_ui_scale(0, self.header_scale.of(area))
            } else {
                AreaContent::new(vec![(blazy::areas::RegionKind::Main, 0.0, canvas)])
            };
            NewWidget::new(content.with_isolated_layer(self.isolated)).erased()
        });
        (screen, graph)
    }
}

/// The screen the benchmarks measure: headers, uniform scale, no layers.
pub fn build_screen(areas: usize, nodes: usize, budget_widgets: Option<usize>) -> (AreaScreen, SharedGraph) {
    ScreenSpec::new(areas, nodes).with_budget(budget_widgets).build()
}

/// The share of the window's widget budget one area gets.
///
/// The whole reason the budget is a public policy rather than a constant inside the
/// canvas. A frame walks the window's widget tree, not any one canvas's (§20.2), so a
/// screen of eight areas each independently obeying the default ceiling puts eight
/// times that in one window — and no canvas is in a position to notice. Measured on
/// this example: one idle area zoomed out to the worst point cost the *other* area
/// sixteen times its frame (§29.1), which is what makes this an arithmetic problem
/// rather than a tuning one.
///
/// Even shares, because what an area holds does not follow its size: a small area at a
/// small zoom holds as much as a large one.
pub fn window_budget(widgets: Option<usize>, areas: usize) -> DetailBudget {
    widgets
        .map(|widgets| DetailBudget {
            widgets,
            ..Default::default()
        })
        .unwrap_or_else(|| DetailBudget::default().split(areas))
}

/// The canvas inside an area, as a `dyn Widget`, holding `budget` of the window's
/// widgets.
pub fn area_canvas(graph: &SharedGraph, nodes: usize, budget: DetailBudget) -> NewWidget<dyn Widget> {
    NewWidget::new(CanvasSpec::new(nodes).over(graph).with_budget(budget)).erased()
}

/// The same canvas with the operator layer's driver around it (§38).
///
/// The driver is a widget of the area rather than of the window, which is the shape
/// §11's nesting asks for — and it is also the only shape available: Masonry's pre-tree
/// hook belongs to a *layer root*, so in a window of eight areas exactly one widget can
/// have it, and it is not any of the editors (§38.1).
pub fn area_editor(graph: &SharedGraph, nodes: usize, budget: DetailBudget) -> NewWidget<dyn Widget> {
    let canvas = CanvasSpec::new(nodes).over(graph).with_budget(budget);
    NewWidget::new(node_canvas::editor::with_ops(canvas, graph)).erased()
}

/// The header of area `area`, tinted so the areas are told apart by eye.
pub fn area_header(area: usize) -> NewWidget<dyn Widget> {
    const TINTS: [Color; 4] = [
        Color::from_rgb8(0x6b, 0x4b, 0x8a),
        Color::from_rgb8(0x3c, 0x6e, 0x71),
        Color::from_rgb8(0x8a, 0x5a, 0x3c),
        Color::from_rgb8(0x44, 0x6b, 0x3c),
    ];
    NewWidget::new(ScaledHeader::new(TINTS[area % TINTS.len()])).erased()
}
