//! How deep a scale change can travel in one event (`issues/rewrite pass budget.md`).
//!
//! Its own file because it needs a widget the example does not have: a container with
//! one child, stacked to any depth between a region's root and a bar of controls. It is
//! defined here rather than borrowed, for the reason the shell's benchmark defines its
//! own widgets — the mechanism under test does not care what the container is, only that
//! it is one more level.
//!
//! Measured on a `RenderRoot` rather than a harness: Masonry runs four rewrite passes an
//! event and carries the rest into the next frame with a warning, which is what a window
//! does — the harness panics instead, and a panic is not a number.

use std::sync::Arc;

use bench_utils::criteria::{Criterion, Kind, ScenarioRecord};
use blazy::areas::{AreaContent, CarriesScale, ScaleCarrier, UiScale, push_ui_scale};
use blazy::masonry::accesskit::{Node, Role};
use blazy::masonry::app::{RenderRoot, RenderRootOptions, WindowSizePolicy};
use blazy::masonry::core::{
    AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PropertiesRef, RegisterCtx,
    UpdateCtx, Widget, WidgetMut, WidgetPod,
};
use blazy::masonry::dpi::PhysicalSize;
use blazy::masonry::imaging::Painter;
use blazy::masonry::kurbo::{Axis, Point, Size};
use blazy::masonry::layout::{LayoutSize, LenDef, LenReq, Length, SizeDef};
use blazy::masonry::theme::default_property_set;
use blazy::masonry::widgets::Button;
use blazy::widgets::{Bar, Label, Metrics, carry_ui_scale};

/// The deepest stack measured.
///
/// Past the depth at which the slow path stops settling in one event — three — and past
/// the one at which it is two frames late — seven — so the sweep shows the effect it
/// guards against rather than stopping short of it (§24.1).
const DEEPEST: usize = 8;

/// One container between a region's root and its controls.
struct Nest {
    child: WidgetPod<dyn Widget>,
    carried: Option<f64>,
}

impl CarriesScale for Nest {
    fn carry_now(this: &mut WidgetMut<'_, Self>, scale: f64) {
        this.widget.carried = Some(scale);
        {
            let mut child = this.ctx.get_mut(&mut this.widget.child);
            push_ui_scale(&mut child, scale);
        }
        this.ctx.request_layout();
    }
}

impl Widget for Nest {
    type Action = NoAction;

    fn property_changed(&mut self, ctx: &mut UpdateCtx<'_>, property_type: std::any::TypeId) {
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
        ctx.compute_length(&mut self.child, auto, LayoutSize::default(), axis, cross_length)
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, props: &PropertiesRef<'_>, size: Size) {
        // The slow path, always present: a container without a carrier is carried here.
        let scale = props.get::<UiScale>(ctx.property_cache()).0;
        carry_ui_scale(ctx, &mut self.child, &mut self.carried, scale);
        let chosen = ctx.compute_size(&mut self.child, SizeDef::fixed(size), size.into());
        ctx.run_layout(&mut self.child, chosen);
        ctx.place_child(&mut self.child, Point::ORIGIN);
    }

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.child);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.child.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

/// One point of the sweep.
pub(crate) struct DepthRow {
    carriers: bool,
    depth: usize,
    /// Whether the rewrite passes of the event finished their work.
    settled: bool,
    /// Frames drawn after the event before the caption at the bottom had the scale.
    late: usize,
}

/// Changes a header's scale through `depth` containers, with or without carriers.
fn row(depth: usize, carriers: bool) -> DepthRow {
    let label = NewWidget::new(Label::new("deep"));
    let label_id = label.id();
    let bar = NewWidget::new(Bar::new(Metrics::default()).with(label.erased(), Metrics::default()));
    let mut header: NewWidget<dyn Widget> = if carriers {
        bar.with_props(ScaleCarrier::of::<Bar>()).erased()
    } else {
        bar.erased()
    };
    for _ in 0..depth {
        let nest = NewWidget::new(Nest {
            child: header.to_pod(),
            carried: None,
        });
        header = if carriers {
            nest.with_props(ScaleCarrier::of::<Nest>()).erased()
        } else {
            nest.erased()
        };
    }
    let content = AreaContent::header_and_main(40.0, header, NewWidget::new(Button::with_text("main")).erased());
    let mut root = RenderRoot::new(NewWidget::new(content).erased(), |_signal| {}, RenderRootOptions {
        default_properties: Arc::new(default_property_set()),
        use_system_fonts: false,
        size_policy: WindowSizePolicy::User,
        size: PhysicalSize::new(800, 600),
        scale_factor: 1.0,
        test_font: None,
    });
    let _ = root.redraw();

    let area = root.get_layer_root(0).id();
    root.edit_widget(area, |mut widget| {
        AreaContent::set_ui_scale(&mut widget.downcast::<AreaContent>(), 0, 2.0);
    });
    let settled = !root.needs_rewrite_passes();
    let applied = |root: &RenderRoot| {
        root.get_widget(label_id)
            .and_then(|widget| widget.downcast::<Label>())
            .map(|label| label.applied_scale())
    };
    let mut late = 0;
    while applied(&root) != Some(2.0) && late < 10 {
        let _ = root.redraw();
        late += 1;
    }
    DepthRow {
        carriers,
        depth,
        settled,
        late,
    }
}

/// The sweep, both ways.
pub(crate) fn depth_table() -> Vec<DepthRow> {
    let rows: Vec<DepthRow> = [false, true]
        .into_iter()
        .flat_map(|carriers| (0..=DEEPEST).map(move |depth| row(depth, carriers)))
        .collect();
    println!("\nscale change through containers: one event or more");
    println!("  {:<10} {:>6} {:>8} {:>6}", "carriers", "depth", "settled", "late");
    for row in &rows {
        println!(
            "  {:<10} {:>6} {:>8} {:>6}",
            if row.carriers { "yes" } else { "no" },
            row.depth,
            row.settled,
            row.late
        );
    }
    rows
}

/// What the sweep claims.
pub(crate) fn depth_criteria(rows: &[DepthRow]) -> Vec<Criterion> {
    let carried = |row: &&DepthRow| row.carriers;
    vec![
        // The claim, counted from the failing side: a depth through which a scale change
        // did not land in the event it was made in is a frame drawn half-scaled.
        Criterion {
            name: "a_scale_change_lands_in_one_event_at_any_depth",
            claim: "through carriers, a scale change lands in one event at every depth swept",
            kind: Kind::Counter,
            measured: rows
                .iter()
                .filter(carried)
                .filter(|row| !row.settled || row.late > 0)
                .count() as f64,
            bound: 1.0,
            unit: "depths that needed another frame",
        },
        // And the sweep reaches the effect: without carriers some depth must spill over,
        // or the sweep stops short of where the claim could fail (§24.1, §25.4).
        Criterion {
            name: "the_depth_sweep_reaches_the_pass_budget",
            claim: "the sweep goes deep enough for the slow path to spill into another frame",
            kind: Kind::Counter,
            measured: f64::from(u8::from(!rows.iter().any(|row| !row.carriers && !row.settled))),
            bound: 1.0,
            unit: "sweeps that never spill",
        },
    ]
}

/// The sweep, archived.
pub(crate) fn depth_record(rows: &[DepthRow]) -> ScenarioRecord {
    let deepest_settled = |carriers: bool| {
        rows.iter()
            .filter(|row| row.carriers == carriers && row.settled && row.late == 0)
            .map(|row| row.depth)
            .max()
            .unwrap_or(0) as f64
    };
    ScenarioRecord {
        name: "depth",
        frames: 1,
        mean_ms: 0.0,
        worst_ms: 0.0,
        materialised: 0,
        detail: format!("scale change through 0..={DEEPEST} containers"),
        child_layouts_per_frame: 0.0,
        builds_per_frame: 0.0,
        far_repaints_per_frame: 0.0,
        extra: vec![
            ("deepest_settled_without_carriers", deepest_settled(false)),
            ("deepest_settled_with_carriers", deepest_settled(true)),
        ],
    }
}
