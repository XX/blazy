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

use blazy::areas::{AreaContent, AreaId, AreaScreen, SplitTree};
use blazy::canvas::{CanvasLayer, DetailBudget};
use blazy::masonry::app::RenderRoot;
use blazy::masonry::core::{NewWidget, Widget, WidgetId, WidgetMut};
use blazy::masonry::peniko::Color;
use blazy::node_editor::{EditorSession, SessionHandle};
use node_canvas::CanvasSpec;
use node_canvas::editor::NodeEditor;
use node_canvas::model::{Change, GraphModel, NODE_SIZE, SharedGraph, share};
use node_canvas::node::GraphNode;

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
    pub fn build(self) -> (Screen, SharedGraph) {
        let graph = share(GraphModel::generated(self.nodes));
        let screen = self.over(&graph);
        (screen, graph)
    }

    /// Builds a screen over a graph that already exists.
    ///
    /// What a second window is made of: another screen over the same models, not another
    /// application. The rule it rests on is §30's — the graph is the truth and a view is
    /// a view — and the second window is the case that makes the rule cross a window
    /// boundary rather than an area one.
    pub fn over(self, graph: &SharedGraph) -> Screen {
        self.over_with(graph, None)
    }

    /// The same, with the first area taking over a session that already exists.
    ///
    /// What detach builds: the window is new, the tree is new, the widget is new — and
    /// the session is the one the area had, so the user arrives looking at what they were
    /// looking at, with what they had selected and what they could undo (decision 1).
    pub fn over_with(self, graph: &SharedGraph, carried: Option<AreaSession>) -> Screen {
        let budget = window_budget(self.budget_widgets, self.areas);
        // The builder outlives this call: the screen keeps it so a split can ask for the
        // widget of an area that does not exist yet.
        let building = graph.clone();
        let mut carried = carried;
        AreaScreen::with_payloads(SplitTree::balanced(self.areas), move |area| {
            // The carried session goes to the first area asked for and to no other: an
            // area that appears later is a new view and needs a session of its own, or
            // two areas would share one selection.
            let session = carried.take().unwrap_or_else(|| EditorSession::new(&building).share());
            let canvas = if self.ops {
                area_editor(&building, self.nodes, budget, &session)
            } else {
                area_canvas(&building, self.nodes, budget)
            };
            let content = if self.header {
                AreaContent::header_and_main(HEADER_HEIGHT, area_header(area), canvas)
                    .with_ui_scale(0, self.header_scale.of(area))
            } else {
                AreaContent::new(vec![(blazy::areas::RegionKind::Main, 0.0, canvas)])
            };
            (
                NewWidget::new(content.with_isolated_layer(self.isolated)).erased(),
                session,
            )
        })
    }
}

/// The screen the benchmarks measure: headers, uniform scale, no layers.
pub fn build_screen(areas: usize, nodes: usize, budget_widgets: Option<usize>) -> (Screen, SharedGraph) {
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

/// What an area of this application carries beside its widget: the session its editor
/// shows (decision 1a of the detach task).
///
/// Every area has one, whether or not it has an editor to show it: the payload type is
/// the screen's, so it is the same for every area, and an area without an editor simply
/// never looks at its own.
pub type AreaSession = SessionHandle<GraphModel>;

/// A screen of this application: areas that carry their sessions.
pub type Screen = AreaScreen<AreaSession>;

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
    NewWidget::new(ScaledHeader::new(TINTS[area % TINTS.len()])).erased()
}

/// Takes an area out of a screen, ready to be built in another window.
///
/// `AreaScreen::detach` removes the area and hands back what it carried; this adds the
/// one thing the library cannot know to do — **cancelling whatever was modal in the
/// session** (decision 6). A gesture belongs to the window it was made in: pointer
/// capture is that window's `RenderRoot`'s, and the area is about to stop being there.
///
/// `None` for the last area of a screen, which does not detach (decision 5).
pub fn detach_area(screen: &mut WidgetMut<'_, Screen>, area: AreaId) -> Option<AreaSession> {
    let session = Screen::detach(screen, area)?;
    {
        let session = &mut *session.borrow_mut();
        session.runtime.cancel_all(&mut session.world);
    }
    Some(session)
}

/// Brings one window's canvases up to date with the graph.
///
/// The half of §30 that stops at the window boundary. Inside a window the fan-out is a
/// push: an operator writes the model, the driver carries the change into its own canvas
/// and schedules `mutate_later` for the graph's other views — and that call names a
/// widget in *this* `RenderRoot`'s arena. A canvas in another window is not in it, so the
/// call is dropped without a word, and the second window goes on showing a node where it
/// used to be.
///
/// So the model also records what each view still owes (`GraphModel::take_pending`), and
/// each window pulls its own share when it next runs. Applying a change twice writes the
/// same truth twice, so the two paths need not know about each other.
///
/// Returns how many changes were applied, which is what the criterion counts: a window
/// that is up to date does nothing, and one that is behind does as much work as there
/// were changes — not as much as there are nodes.
pub fn sync_window(root: &mut RenderRoot, graph: &SharedGraph) -> usize {
    let editors: Vec<WidgetId> = root.edit_base_layer(|mut widget| {
        let screen = widget.downcast::<Screen>();
        screen.widget.area_ids()
    });

    let mut applied = 0;
    for area in editors {
        // The area holds its regions; the editor is the main one, and a header does not
        // show the graph.
        let Some(editor_id) = root
            .get_widget(area)
            .and_then(|widget| widget.downcast::<AreaContent>())
            .and_then(|content| content.region_ids().last().copied())
        else {
            continue;
        };
        root.edit_widget(editor_id, |mut widget| {
            // Two shapes, because this example builds both: an area with the operator
            // layer holds a `NodeEditor` over the canvas, and one without holds the canvas
            // itself. Looking only for the editor is how the running window — which is
            // built without operators — came to collect nothing at all.
            if let Some(mut editor) = widget.try_downcast::<NodeEditor>() {
                applied += sync_editor(&mut editor, graph);
            } else if let Some(mut canvas) = widget.try_downcast::<CanvasLayer>() {
                applied += sync_canvas(&mut canvas, graph);
            }
        });
    }
    applied
}

/// Brings one editor up to date, and says how many changes that took.
///
/// Split out from [`sync_window`] because the two callers hold different contexts: an
/// application's driver has a `RenderRoot`, and a test or a benchmark has a harness that
/// does not hand one out (§39.5, upstream candidate 5). What they share is this.
pub fn sync_editor(editor: &mut WidgetMut<'_, NodeEditor>, graph: &SharedGraph) -> usize {
    let canvas = editor.widget.canvas_id();
    let changes = graph.borrow_mut().take_pending(canvas);
    for &change in &changes {
        apply_change(editor, graph, change);
    }
    changes.len()
}

/// Brings one canvas up to date, for an area built without the operator layer.
///
/// The same pull as [`sync_editor`], one widget lower: an area of this example holds either
/// an editor over a canvas or a canvas on its own, and the change has to reach the canvas
/// either way. The name a view is registered under is the canvas's own id — that is what
/// `NodeSource::attached` hands the model — so both paths ask the same question.
pub fn sync_canvas(canvas: &mut WidgetMut<'_, CanvasLayer>, graph: &SharedGraph) -> usize {
    let name = canvas.ctx.widget_id();
    let changes = graph.borrow_mut().take_pending(name);
    for &change in &changes {
        apply_to_canvas(canvas, graph, change);
    }
    changes.len()
}

/// Applies one change of the graph to one view.
fn apply_change(editor: &mut WidgetMut<'_, NodeEditor>, graph: &SharedGraph, change: Change) {
    NodeEditor::with_canvas(editor, |mut canvas| apply_to_canvas(&mut canvas, graph, change));
}

/// The change itself, against the canvas that shows it.
fn apply_to_canvas(canvas: &mut WidgetMut<'_, CanvasLayer>, graph: &SharedGraph, change: Change) {
    match change {
        Change::Moved(index) | Change::Added(index) => {
            let Some(node) = graph.borrow().try_node(index) else {
                return;
            };
            if CanvasLayer::child_pos(canvas, index).is_some() {
                CanvasLayer::move_child(canvas, index, node.pos);
            } else {
                CanvasLayer::insert_node(canvas, index, node.pos, NODE_SIZE);
            }
        },
        Change::Removed(index) => {
            CanvasLayer::remove_node(canvas, index);
        },
        // Nothing to do for a node this view is not showing: it will read the model when
        // it is next built. `update_child` says so by returning false.
        Change::Edited(index) => {
            CanvasLayer::update_child(canvas, index, |mut node| {
                GraphNode::reload(&mut node.downcast::<GraphNode>());
            });
        },
    }
}
