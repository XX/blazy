//! The canvas, checked through its widgets.

use masonry::core::NewWidget;
use masonry::peniko::Color;

use super::*;
use crate::{DEFAULT_WIDGET_BUDGET, DetailThresholds, Link};

fn diff(live: &[usize], desired: &[usize]) -> (Vec<usize>, Vec<usize>) {
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    diff_sorted(live, desired, |_| false, &mut removed, &mut added);
    (removed, added)
}

#[test]
fn diff_of_equal_sets_is_empty() {
    assert_eq!(diff(&[1, 2, 3], &[1, 2, 3]), (vec![], vec![]));
}

#[test]
fn diff_reports_arrivals_and_departures() {
    assert_eq!(diff(&[1, 3, 5], &[3, 4, 5, 6]), (vec![1], vec![4, 6]));
    assert_eq!(diff(&[], &[0, 1]), (vec![], vec![0, 1]));
    assert_eq!(diff(&[0, 1], &[]), (vec![0, 1], vec![]));
}

#[test]
fn stale_entries_are_rebuilt_in_place() {
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    diff_sorted(&[1, 2, 3], &[1, 2, 3], |i| i == 2, &mut removed, &mut added);
    assert_eq!((removed, added), (vec![2], vec![2]));
}

#[test]
fn diff_reuses_its_buffers() {
    let (mut removed, mut added) = (vec![99], vec![99]);
    diff_sorted(&[1], &[1], |_| false, &mut removed, &mut added);
    assert!(removed.is_empty() && added.is_empty(), "stale contents must be cleared");
}

#[test]
fn contains_rect_is_inclusive() {
    let outer = Rect::new(0.0, 0.0, 10.0, 10.0);
    assert!(contains_rect(outer, outer));
    assert!(contains_rect(outer, Rect::new(1.0, 1.0, 9.0, 9.0)));
    assert!(!contains_rect(outer, Rect::new(-1.0, 0.0, 5.0, 5.0)));
}

#[test]
fn detail_thresholds_are_ordered() {
    let thresholds = DetailThresholds::default();
    assert_eq!(thresholds.for_scale(1.0), Detail::Full);
    assert_eq!(thresholds.for_scale(0.2), Detail::Simplified);
    assert_eq!(thresholds.for_scale(0.01), Detail::Box);
}

// --- MARK: HIT TESTS

/// A source whose nodes are rounded rectangles, like a real one.
struct RoundedSource {
    shape: blazy_shape::ShapeHit,
    size: Size,
}

impl RoundedSource {
    fn new(size: Size) -> Self {
        Self {
            shape: blazy_shape::ShapeHit::fill(masonry::kurbo::RoundedRect::from_rect(
                Rect::from_origin_size(Point::ORIGIN, size),
                12.0,
            )),
            size,
        }
    }
}

impl NodeSource for RoundedSource {
    fn build(&mut self, _index: usize, _detail: Detail) -> NewWidget<dyn Widget> {
        unimplemented!("these tests never materialise a widget: that is the point")
    }

    fn hit(&mut self, _index: usize, rect: Rect, point: Point) -> bool {
        assert_eq!(rect.size(), self.size);
        self.shape.contains(point - rect.origin().to_vec2(), 1.0)
    }
}

/// A canvas content over the given node rectangles and edges, already culled.
///
/// Built directly rather than through a harness: everything under test answers
/// from the model, so a widget tree would only add a way for the test to be
/// about something else.
fn content(rects: &[Rect], edges: Vec<Link>, visible: Rect) -> CanvasContent {
    content_at_scale(rects, edges, visible, 1.0)
}

/// The same, at a given zoom — which is what the short-link rule reads.
fn content_at_scale(rects: &[Rect], edges: Vec<Link>, visible: Rect, scale: f64) -> CanvasContent {
    let size = rects.first().map_or(Size::ZERO, Rect::size);
    let slots = rects
        .iter()
        .map(|r| Slot {
            alive: true,
            pos: r.origin(),
            size: r.size(),
            pod: None,
            built: None,
        })
        .collect();
    let mut content = CanvasContent::new(slots, Box::new(RoundedSource::new(size)));
    content.links = LinkLayer::new(edges, rects.len());
    content.links.invalidate();
    content.detail = Some(Detail::Full);
    content.live_rect = visible;
    content.scale = scale;
    content.cull();
    content
}

fn node_rect(x: f64, y: f64) -> Rect {
    Rect::from_origin_size(Point::new(x, y), Size::new(100.0, 60.0))
}

const EVERYTHING: Rect = Rect::new(-1000.0, -1000.0, 1000.0, 1000.0);

/// The claim of §6.1: the hit geometry is the shape, not the box it sits in.
#[test]
fn a_point_in_the_corner_of_a_node_misses_it() {
    let mut canvas = content(&[node_rect(0.0, 0.0)], Vec::new(), EVERYTHING);
    let corner = Point::new(1.0, 1.0);

    assert!(node_rect(0.0, 0.0).contains(corner), "inside the rectangle");
    assert_eq!(canvas.hit(corner, 1.0), None, "outside the rounded body");
    assert!(matches!(
        canvas.hit(Point::new(50.0, 30.0), 1.0),
        Some(CanvasHit::Node { index: 0, .. })
    ));
}

/// Picking a link is the case Masonry cannot answer at all: a curve is not a
/// widget, so nothing in the tree knows it is there.
#[test]
fn a_link_is_picked_along_its_curve() {
    let mut canvas = content(
        &[node_rect(0.0, 0.0), node_rect(400.0, 0.0)],
        vec![Link::new(0, 1)],
        EVERYTHING,
    );

    // The curve runs from the right edge of one node to the left edge of the
    // other, both centred on y = 30.
    assert!(matches!(
        canvas.hit(Point::new(250.0, 30.0), 1.0),
        Some(CanvasHit::Link { edge: 0, .. })
    ));
    assert_eq!(canvas.hit(Point::new(250.0, 90.0), 1.0), None);
}

/// Nodes are painted over links, so they are picked over links too (§25.3).
#[test]
fn a_node_wins_over_a_link_running_under_it() {
    let mut canvas = content(
        &[node_rect(0.0, 0.0), node_rect(400.0, 0.0), node_rect(200.0, 0.0)],
        vec![Link::new(0, 1)],
        EVERYTHING,
    );
    let on_both = Point::new(250.0, 30.0);

    assert!(
        blazy_shape::near_segment(
            crate::links::link_curve(node_rect(0.0, 0.0), node_rect(400.0, 0.0)).into(),
            on_both,
            4.0
        ),
        "the point really is on the curve"
    );
    assert!(matches!(
        canvas.hit(on_both, 1.0),
        Some(CanvasHit::Node { index: 2, .. })
    ));
}

/// The tolerance is in screen pixels, so the same canvas point picks a link when
/// the canvas is zoomed out and misses it when zoomed in (§25.2).
#[test]
fn the_link_tolerance_follows_the_zoom() {
    let mut canvas = content(
        &[node_rect(0.0, 0.0), node_rect(400.0, 0.0)],
        vec![Link::new(0, 1)],
        EVERYTHING,
    );
    // Three canvas units off the curve, with a stroke one unit wide either side.
    let near = Point::new(250.0, 33.0);

    assert!(canvas.hit(near, 1.0).is_some(), "3 px away at 1x");
    assert_eq!(canvas.hit(near, 8.0), None, "24 px away at 8x");
    assert!(canvas.hit(near, 0.25).is_some(), "well under a pixel at 0.25x");
}

/// What can be picked is what is drawn, including where that is not enough: a
/// link whose ends are both outside the recorded region is neither (§24.4).
#[test]
fn a_link_that_is_not_drawn_is_not_picked() {
    let mut canvas = content(
        &[node_rect(-5000.0, 0.0), node_rect(5000.0, 0.0)],
        vec![Link::new(0, 1)],
        Rect::new(-200.0, -200.0, 200.0, 200.0),
    );

    assert!(canvas.links.recorded().is_empty(), "neither end is near the viewport");
    assert_eq!(canvas.hit(Point::new(0.0, 30.0), 1.0), None);
}

/// Nodes below the far-field threshold have no widget at all, and must still be
/// pickable — the reason picking asks the model and not the tree (§20.6).
#[test]
fn a_node_with_no_widget_is_still_picked() {
    let mut canvas = content(&[node_rect(0.0, 0.0)], Vec::new(), EVERYTHING);
    canvas.detail = Some(Detail::Box);
    canvas.cull();

    assert!(canvas.live.is_empty(), "far field: nothing is materialised");
    assert!(matches!(
        canvas.hit(Point::new(50.0, 30.0), 1.0),
        Some(CanvasHit::Node { index: 0, .. })
    ));
}

/// Picking must not walk the graph: the candidates come from the grid.
#[test]
fn picking_does_not_examine_the_whole_graph() {
    let rects: Vec<Rect> = (0..4000)
        .map(|i| node_rect((i % 80) as f64 * 220.0, (i / 80) as f64 * 220.0))
        .collect();
    let mut canvas = content(&rects, Vec::new(), Rect::new(0.0, 0.0, 1100.0, 750.0));

    let before = canvas.hit_node_tests;
    canvas.hit(Point::new(50.0, 30.0), 1.0);
    let examined = canvas.hit_node_tests - before;

    assert!(examined < 64, "examined {examined} geometries of 4000");
}

/// A hover highlights a curve, and a highlight is a repaint. Nothing here is
/// allowed to ask for layout — that is what `set_active` is for.
#[test]
fn only_a_link_hover_asks_for_a_repaint() {
    let mut canvas = content(&[node_rect(0.0, 0.0)], Vec::new(), EVERYTHING);
    let node = Some(CanvasHit::Node {
        index: 0,
        pos: Point::ORIGIN,
    });
    let link = Some(CanvasHit::Link {
        edge: 0,
        link: Link::new(0, 1),
    });

    assert!(!canvas.set_hovered(node), "a node highlight is not drawn");
    assert!(!canvas.set_hovered(node), "and an unchanged hover is not a change");
    assert!(canvas.set_hovered(link), "arriving on a curve repaints it");
    assert!(canvas.set_hovered(None), "and leaving it repaints it back");
}

#[test]
fn detail_thresholds_are_configurable() {
    let thresholds = DetailThresholds {
        full: 0.6,
        simplified: 0.25,
    };
    assert_eq!(thresholds.for_scale(0.4), Detail::Simplified);
    assert_eq!(thresholds.for_scale(0.1), Detail::Box);
}

// --- MARK: budget

/// The budget picks the most detailed level that fits, and nothing finer.
#[test]
fn the_budget_takes_the_finest_level_that_fits() {
    let budget = DetailBudget {
        widgets: 1000,
        full_cost: 4,
        simplified_cost: 1,
        hysteresis: 0.0,
    };
    // 100 nodes cost 400 widgets in full; 300 cost 1200 and do not fit, but the
    // same 300 cost 300 simplified.
    assert_eq!(budget.level_for(100, None), Detail::Full);
    assert_eq!(budget.level_for(300, None), Detail::Simplified);
    assert_eq!(budget.level_for(1001, None), Detail::Box);
}

/// An unlimited budget leaves the zoom thresholds as the only rule.
#[test]
fn an_unlimited_budget_never_binds() {
    let budget = DetailBudget::unlimited();
    assert_eq!(budget.level_for(1_000_000, None), Detail::Full);
}

/// Coming back up costs more than staying put, which is what stops the flapping.
///
/// The gap is the measured jitter of the visible set during a pan (§29.1): with
/// the two thresholds equal, a set wobbling between 249 and 251 nodes switches
/// level twice a second and rebuilds every visible node each time.
#[test]
fn the_budget_is_hysteretic() {
    let budget = DetailBudget {
        widgets: 1000,
        full_cost: 4,
        simplified_cost: 1,
        hysteresis: 0.25,
    };
    // 240 nodes cost 960 widgets: inside the budget, so `Full` holds...
    assert_eq!(budget.level_for(240, Some(Detail::Full)), Detail::Full);
    // ...but is not reached from below, where the ceiling is 750.
    assert_eq!(budget.level_for(240, Some(Detail::Simplified)), Detail::Simplified);
    // Well clear of the margin, it is reached.
    assert_eq!(budget.level_for(180, Some(Detail::Simplified)), Detail::Full);
}

/// A window budget divided between the canvases sharing the window.
#[test]
fn a_split_budget_shares_one_ceiling() {
    let budget = DetailBudget::default().split(8);
    assert_eq!(budget.widgets, DEFAULT_WIDGET_BUDGET / 8);
    assert_eq!(budget.full_cost, DetailBudget::default().full_cost);
    // Dividing by nothing is the whole budget rather than a panic.
    assert_eq!(DetailBudget::default().split(0).widgets, DEFAULT_WIDGET_BUDGET);
}

/// Readability and cost are separate rules and the stricter one decides.
///
/// Both directions matter: a zoom too small for a slider cannot be rescued by a
/// generous budget, and a graph too dense cannot be rescued by a legible zoom.
#[test]
fn the_stricter_of_the_two_rules_wins() {
    let thresholds = DetailThresholds::default();
    let budget = DetailBudget {
        widgets: 1000,
        full_cost: 4,
        simplified_cost: 1,
        hysteresis: 0.0,
    };
    // Legible zoom, unaffordable set: cost decides.
    let readable = thresholds.for_scale(1.0);
    assert_eq!(readable.max(budget.level_for(400, None)), Detail::Simplified);
    // Affordable set, illegible zoom: readability decides.
    let readable = thresholds.for_scale(0.01);
    assert_eq!(readable.max(budget.level_for(1, None)), Detail::Box);
}

// --- MARK: SHORT LINKS

/// Room for the long link of the tests below, which reaches out to x = 2000.
const WIDE: Rect = Rect::new(-3000.0, -3000.0, 3000.0, 3000.0);

/// A curve too short to see is dropped when the set is chosen, not when it is
/// drawn — so it leaves the picture and the pointer's reach together.
///
/// One link only, so "no link here" is unambiguous: with a second one on screen
/// the pick tolerance at this zoom (screen pixels divided by 0.01) reaches far
/// enough to find it, and the test would be measuring the tolerance instead.
#[test]
fn a_link_too_short_to_see_is_neither_drawn_nor_picked() {
    let rects = [node_rect(0.0, 0.0), node_rect(120.0, 0.0)];
    let mut canvas = content_at_scale(&rects, vec![Link::new(0, 1)], WIDE, 0.01);

    assert!(
        canvas.links.recorded().is_empty(),
        "a curve under a pixel long should not be recorded"
    );
    assert_eq!(canvas.links.hidden(), 1);
    // Between the two nodes, where the curve would run.
    assert_eq!(canvas.hit(Point::new(110.0, 30.0), 0.01), None);
}

/// And it drops only the short one: the rule is a threshold, not an off switch.
#[test]
fn the_short_link_rule_keeps_the_links_that_are_visible() {
    let rects = [
        node_rect(0.0, 0.0),
        node_rect(120.0, 0.0),
        node_rect(0.0, 400.0),
        node_rect(2000.0, 400.0),
    ];
    let edges = vec![Link::new(0, 1), Link::new(2, 3)];
    let canvas = content_at_scale(&rects, edges, WIDE, 0.01);

    assert_eq!(canvas.links.recorded(), &[1], "the long link survives");
    assert_eq!(canvas.links.hidden(), 1);
}

/// The rule needs no "only when zoomed out" clause because an on-screen length
/// grows with the zoom. This is what says so: at a zoom anything is readable at,
/// it hides nothing at all.
#[test]
fn the_short_link_rule_is_silent_at_a_working_zoom() {
    let rects = [node_rect(0.0, 0.0), node_rect(120.0, 0.0)];
    let mut canvas = content_at_scale(&rects, vec![Link::new(0, 1)], EVERYTHING, 1.0);

    assert_eq!(canvas.links.recorded(), &[0]);
    assert_eq!(canvas.links.hidden(), 0);
    assert!(matches!(
        canvas.hit(Point::new(110.0, 30.0), 1.0),
        Some(CanvasHit::Link { edge: 0, .. })
    ));
}

// --- MARK: BATCHING

/// Strokes some curves, either as one path or as one command each.
///
/// Two widgets' worth of behaviour in one, because the whole question is whether
/// the two are the same picture.
struct Curves {
    links: Vec<(Rect, Rect)>,
    batched: bool,
}

impl Widget for Curves {
    type Action = NoAction;

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::ZERO,
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {}

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        let stroke = Stroke::new(3.0);
        let colour = Color::from_rgb8(0xd0, 0xd0, 0xe0);
        if self.batched {
            let mut path = BezPath::new();
            for &(from, to) in &self.links {
                push_link(&mut path, from, to);
            }
            if !path.is_empty() {
                painter.stroke(&path, &stroke, colour).draw();
            }
        } else {
            for &(from, to) in &self.links {
                let mut path = BezPath::new();
                push_link(&mut path, from, to);
                painter.stroke(&path, &stroke, colour).draw();
            }
        }
    }

    fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

fn drawn(links: &[(Rect, Rect)], batched: bool) -> Vec<u8> {
    let mut harness = masonry::testing::TestHarness::create_with_size(
        masonry::theme::default_property_set(),
        NewWidget::new(Curves {
            links: links.to_vec(),
            batched,
        }),
        masonry::dpi::PhysicalSize::new(400, 200),
    );
    harness.render().into_raw()
}

/// The claim the whole batch rests on: `move_to` starts a subpath, and separate
/// subpaths are stroked separately. If they were joined up, a segment would run
/// from the end of one link to the start of the next and these would differ.
#[test]
fn a_batched_stroke_draws_what_separate_strokes_draw() {
    let links = [
        (node_rect(20.0, 20.0), node_rect(240.0, 30.0)),
        (node_rect(20.0, 120.0), node_rect(240.0, 130.0)),
    ];
    assert_eq!(
        drawn(&links, true),
        drawn(&links, false),
        "one command with two subpaths must paint what two commands paint"
    );

    // And the space between the two links stays empty, which is the same claim
    // read the other way round.
    let empty = drawn(&[], true);
    let batched = drawn(&links, true);
    let midpoint = (100 * 400 + 200) * 4;
    assert_eq!(
        batched[midpoint..midpoint + 4],
        empty[midpoint..midpoint + 4],
        "no segment joins the end of one link to the start of the next"
    );
}

/// Where the batch is *not* pixel-identical, and it is worth knowing which way.
///
/// Two curves leaving the same point overlap, and their antialiased coverage is
/// composited once inside a shared command against twice as separate ones. It
/// moved 50 pixels of 46 800 in the `canvas_with_links` snapshot, by at most 25
/// of 255, all of them on curve overlaps — small, real, and not something to
/// discover later from a failing gate.
#[test]
fn overlapping_curves_composite_once_in_a_batch() {
    let shared = node_rect(20.0, 90.0);
    let links = [(shared, node_rect(240.0, 20.0)), (shared, node_rect(240.0, 150.0))];
    assert_ne!(
        drawn(&links, true),
        drawn(&links, false),
        "if this ever matches, the difference the snapshot records has gone away"
    );
}

// --- MARK: STRUCTURE (§43)

/// Three nodes in a row, wired 0-1-2, everything visible.
fn chain() -> CanvasContent {
    content(
        &[node_rect(0.0, 0.0), node_rect(200.0, 0.0), node_rect(400.0, 0.0)],
        vec![Link::new(0, 1), Link::new(1, 2)],
        EVERYTHING,
    )
}

/// What the canvas answers about a point, as a node index.
fn node_at(canvas: &mut CanvasContent, x: f64, y: f64) -> Option<usize> {
    canvas.hit(Point::new(x, y), 1.0).and_then(CanvasHit::node)
}

/// The rule §41.2 made for areas, here: a removal renumbers nothing.
///
/// The node that stayed keeps its name, its place and its links — which is what a
/// selection, a history and the other views of the graph are all keyed by.
#[test]
fn removing_a_node_renumbers_nothing() {
    let mut canvas = chain();
    let before = canvas.live_rect_of(2).expect("node 2 is there");

    let removed = canvas.remove_node(1);
    canvas.cull();

    assert_eq!(
        canvas.live_rect_of(2),
        Some(before),
        "node 2 kept its name and its place"
    );
    assert!(canvas.live_slot(1).is_none(), "node 1 is gone");
    assert_eq!(canvas.node_count(), 2);
    assert_eq!(removed.len(), 2, "both of its links came back, named");
    assert_eq!(
        node_at(&mut canvas, 250.0, 30.0),
        None,
        "nothing is picked where it was"
    );
    assert_eq!(node_at(&mut canvas, 450.0, 30.0), Some(2));
}

/// A removal takes its links with it, and undo puts them back as they were.
#[test]
fn a_removed_node_comes_back_with_its_links() {
    let mut canvas = chain();
    let rect = canvas.live_rect_of(1).expect("node 1 is there");
    canvas.refresh_links();
    let recorded = canvas.links.recorded().to_vec();

    let removed = canvas.remove_node(1);
    canvas.cull();
    assert!(canvas.links.recorded().is_empty(), "no link survives both of its ends");

    let _ = canvas.insert_node(1, rect.origin(), rect.size());
    for &(name, link) in &removed {
        canvas.restore_link(name, link);
    }
    canvas.cull();

    assert_eq!(canvas.live_rect_of(1), Some(rect));
    assert_eq!(
        canvas.links.recorded(),
        recorded,
        "the same links, under the same names"
    );
}

/// A structural edit is not a view change, and the recorded sets are chosen by the
/// view — so the edit has to say so itself (§28.4).
#[test]
fn an_edit_re_chooses_the_recorded_sets() {
    let mut canvas = chain();
    canvas.cull();
    let before = canvas.links.recorded().len();

    let name = canvas.insert_link(Link::new(0, 2));
    canvas.cull();
    assert_eq!(canvas.links.recorded().len(), before + 1, "the new link is drawn");

    canvas.remove_link(name);
    canvas.cull();
    assert_eq!(
        canvas.links.recorded().len(),
        before,
        "and stops being drawn when it goes"
    );
}

/// A node inserted at a name nothing holds is found there, by the index as well as by
/// the slot array — the pick goes through the grid, so a node missing from it is
/// invisible to the pointer while being drawn.
#[test]
fn an_inserted_node_is_found_by_the_pointer() {
    let mut canvas = chain();
    let _ = canvas.insert_node(7, Point::new(600.0, 0.0), Size::new(100.0, 60.0));
    canvas.cull();

    assert_eq!(canvas.node_count(), 4, "three and the new one; the holes are not nodes");
    assert_eq!(node_at(&mut canvas, 650.0, 30.0), Some(7));
    assert!(canvas.visible.contains(&7), "and it is in the visible set");
}

/// A removal costs the links of the node it removed, not the graph's.
#[test]
fn a_removal_walks_its_own_links_only() {
    // A star: every node wired to node 0, so removing a leaf must not cost the hub.
    let rects: Vec<Rect> = (0..64).map(|i| node_rect(i as f64 * 200.0, 0.0)).collect();
    let edges: Vec<Link> = (1..64).map(|i| Link::new(0, i)).collect();
    let mut canvas = content(&rects, edges, EVERYTHING);

    let before = canvas.links.edit_counters().1;
    canvas.remove_node(63);
    let leaf = canvas.links.edit_counters().1 - before;

    let before = canvas.links.edit_counters().1;
    canvas.remove_node(0);
    let hub = canvas.links.edit_counters().1 - before;

    assert!(leaf <= 4, "a leaf has one link: {leaf} names walked");
    assert!(hub >= 62, "the hub has sixty-two left: {hub} names walked");
}

/// Adding links one at a time must not re-pack the adjacency once per edit, and the
/// adjacency has to stay right across the re-packing that does happen.
#[test]
fn the_adjacency_survives_its_own_compaction() {
    let rects: Vec<Rect> = (0..200).map(|i| node_rect(i as f64 * 200.0, 0.0)).collect();
    // A viewport that holds the whole row: what is recorded is chosen by the region, and
    // this test is about the adjacency rather than about the selection.
    let all = Rect::new(-1000.0, -1000.0, 41000.0, 1000.0);
    let mut canvas = content(&rects, Vec::new(), all);

    for i in 0..199 {
        canvas.insert_link(Link::new(i, i + 1));
    }
    let compactions = canvas.links.edit_counters().0;
    assert!(compactions > 0, "the packing is redone at all");
    assert!(compactions < 20, "but not once an edit: {compactions} in 199 edits");

    // Every link is reachable from both of its ends, whichever side of a compaction it
    // was added on.
    canvas.links.invalidate();
    canvas.live_rect = all;
    canvas.refresh_links();
    assert_eq!(canvas.links.recorded().len(), 199);
}
