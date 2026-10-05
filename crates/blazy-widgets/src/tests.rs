//! What the two channels of §46 do, and what they cost.

use blazy_areas::{AreaContent, UiScale};
use masonry::core::{NewWidget, WidgetId};
use masonry::dpi::PhysicalSize;
use masonry::kurbo::Size;
use masonry::layout::Length;
use masonry::properties::Padding;
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;
use masonry::widgets::Button;

use crate::{Bar, Label, Metrics};

/// Height of the header region the bars are tested in.
const HEADER: f64 = 40.0;
/// Size of the area the header sits in.
const AREA: (u32, u32) = (600, 400);

/// A header of the given content over an empty main region.
fn area_with(header: NewWidget<dyn masonry::core::Widget>) -> TestHarness<AreaContent> {
    let content = AreaContent::header_and_main(HEADER, header, NewWidget::new(Button::with_text("main")).erased());
    let mut harness = TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(content),
        PhysicalSize::new(AREA.0, AREA.1),
    );
    let _ = harness.redraw();
    harness
}

/// The box the text itself occupies: the border box less the padding pushed onto it.
///
/// Which is the whole point of measuring it separately — the box channel moves the
/// padding, so a border box grows with the scale whether or not the text did.
fn content_box(harness: &TestHarness<AreaContent>, id: WidgetId) -> Size {
    harness.get_widget_with_id(id).ctx().content_box().size()
}

/// A stock Masonry widget follows the box properties pushed onto it.
///
/// The measurement the whole crate rests on, and the one that cancelled the widget set
/// this plan item used to be (§46): `masonry_widgets_do_not_follow_ui_scale` is true, but
/// only because nobody hands them the scale — their geometry *is* reachable from outside,
/// as ordinary properties, without the widget knowing what a scale is.
#[test]
fn a_stock_widget_follows_pushed_box_properties() {
    let mut harness = area_with(NewWidget::new(Button::with_text("scale me")).erased());
    let button = harness.root_widget().region_ids()[0];

    let insets = |h: &TestHarness<AreaContent>| {
        let ctx = h.get_widget_with_id(button);
        let (border, content) = (ctx.ctx().border_box().size(), ctx.ctx().content_box().size());
        Size::new(border.width - content.width, border.height - content.height)
    };
    let before = insets(&harness);

    harness.edit_widget_with_id(button, |mut widget| {
        widget.insert_prop(Padding::all(Length::px(20.0)));
    });
    let _ = harness.redraw();

    let after = insets(&harness);
    assert!(
        after.width > before.width && after.height > before.height,
        "a stock button ignored a pushed padding: {before:?} -> {after:?}"
    );
}

/// The box channel alone leaves the text where it was.
///
/// Why this crate has a [`Label`] at all. A font size is a parley style rather than a
/// property, so nothing pushed from outside can move it — and an application should not
/// have to downcast a stock widget to make its own caption follow the interface scale.
#[test]
fn the_box_channel_does_not_reach_the_text() {
    let bar = Bar::new(Metrics::default()).with(
        NewWidget::new(masonry::widgets::Label::new("caption")).erased(),
        Metrics::default(),
    );
    let mut harness = area_with(NewWidget::new(bar).erased());
    let bar_id = harness.root_widget().region_ids()[0];
    let label = harness.get_widget_with_id(bar_id).children()[0].ctx().widget_id();

    let before = content_box(&harness, label);
    harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 0, 2.0));
    let _ = harness.redraw();
    let after = content_box(&harness, label);

    // Width, not height: the bar stretches its children to the row, and the row follows
    // the scale because the header's own height does (§22). What the text costs is its
    // width, and that is the number a property cannot move.
    assert_eq!(
        after.width, before.width,
        "a stock label has no box to scale: its width is its text, and the text is not a property"
    );
}

/// A caption of this crate follows the region's scale, through the channel only it can
/// reach.
#[test]
fn a_label_of_this_crate_follows_the_region_scale() {
    let bar = Bar::new(Metrics::default()).with(NewWidget::new(Label::new("caption")).erased(), Metrics::default());
    let mut harness = area_with(NewWidget::new(bar).erased());
    let bar_id = harness.root_widget().region_ids()[0];
    let label = harness.get_widget_with_id(bar_id).children()[0].ctx().widget_id();

    let before = content_box(&harness, label);
    harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 0, 2.0));
    let _ = harness.redraw();
    let after = content_box(&harness, label);

    // Width again, for the reason the test above gives: the height is the row's.
    assert!(
        after.width > before.width * 1.5,
        "the caption did not follow the scale: {before:?} -> {after:?}"
    );
}

/// The bar carries the scale once per change, not once per frame.
///
/// The counter §22.1 asked for without pricing: carrying a scale with no inherited
/// properties costs one property write per control, so a bar that re-pushed on every
/// layout would put the region through the property pass forever — and nothing in a
/// counter of *sizes* would show it (§28.4).
#[test]
fn the_bar_carries_the_scale_once_per_change() {
    let bar = Bar::new(Metrics::default())
        .with(NewWidget::new(Label::new("a")).erased(), Metrics::default())
        .with(NewWidget::new(Button::with_text("b")).erased(), Metrics::default());
    let mut harness = area_with(NewWidget::new(bar).erased());
    let bar_id = harness.root_widget().region_ids()[0];

    let counters = |h: &TestHarness<AreaContent>| {
        h.get_widget_with_id(bar_id)
            .downcast::<Bar>()
            .expect("the header is a bar")
            .counters()
    };
    let first = counters(&harness);
    assert_eq!(first.scalings, 1, "the first layout is the first scale");
    assert_eq!(first.children_scaled, 2);

    // Frames in which the scale did not change cost nothing.
    for _ in 0..3 {
        let _ = harness.redraw();
    }
    assert_eq!(counters(&harness), first, "an unchanged scale is not carried again");

    harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 0, 2.0));
    let _ = harness.redraw();
    let second = counters(&harness);
    assert_eq!(second.scalings, 2);
    assert_eq!(second.children_scaled, 4, "one property write per control per change");
}

/// The scale reaches the children as a property, so a widget of this crate nested deeper
/// still sees it.
#[test]
fn the_scale_reaches_a_child_as_a_property() {
    let bar = Bar::new(Metrics::default()).with(NewWidget::new(Label::new("caption")).erased(), Metrics::default());
    let mut harness = area_with(NewWidget::new(bar).erased());
    let bar_id = harness.root_widget().region_ids()[0];
    let label = harness.get_widget_with_id(bar_id).children()[0].ctx().widget_id();

    harness.edit_root_widget(|mut content| AreaContent::set_ui_scale(&mut content, 0, 1.5));
    let _ = harness.redraw();

    let applied = harness
        .get_widget_with_id(label)
        .downcast::<Label>()
        .expect("the child is a label")
        .applied_scale();
    assert_eq!(applied, 1.5, "the label read the scale the bar pushed onto it");
    assert_eq!(
        harness.get_widget_with_id(label).get_prop::<UiScale>().0,
        1.5,
        "and it is there as a property, which is what a deeper widget would read"
    );
}

/// Depth: how a scale change travels through containers between a region's root and its
/// controls (`issues/rewrite pass budget.md`).
mod depth {
    use std::sync::Arc;

    use blazy_areas::{AreaContent, CarriesScale, ScaleCarrier, UiScale, push_ui_scale};
    use masonry::accesskit::{Node, Role};
    use masonry::app::{RenderRoot, RenderRootOptions, WindowSizePolicy};
    use masonry::core::{
        AccessCtx, ChildrenIds, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PropertiesRef, RegisterCtx,
        UpdateCtx, Widget, WidgetMut, WidgetPod,
    };
    use masonry::dpi::PhysicalSize;
    use masonry::imaging::Painter;
    use masonry::kurbo::{Axis, Point, Size};
    use masonry::layout::{LayoutSize, LenDef, LenReq, Length, SizeDef};
    use masonry::theme::default_property_set;
    use masonry::widgets::Button;

    use crate::{Bar, Label, Metrics, carry_ui_scale};

    /// A container with one child, carrying the scale both ways: in its layout (the
    /// slow path, always) and in the pass it was given one (the fast path, when it
    /// carries a [`ScaleCarrier`]).
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

    /// Changes the header's scale to 2 through `depth` containers and reports whether
    /// the event settled, and how many more frames the caption at the bottom needed.
    ///
    /// On a `RenderRoot` rather than a harness: past its four passes Masonry warns and
    /// carries the rest into the next frame, which is what a window does, where the
    /// harness panics instead.
    fn scale_through(depth: usize, carriers: bool) -> (bool, usize) {
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
        let content = AreaContent::header_and_main(
            super::HEADER,
            header,
            NewWidget::new(Button::with_text("main")).erased(),
        );
        let mut root = RenderRoot::new(NewWidget::new(content).erased(), |_signal| {}, RenderRootOptions {
            default_properties: Arc::new(default_property_set()),
            use_system_fonts: false,
            size_policy: WindowSizePolicy::User,
            size: PhysicalSize::new(super::AREA.0, super::AREA.1),
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
        let mut frames = 0;
        while applied(&root) != Some(2.0) && frames < 10 {
            let _ = root.redraw();
            frames += 1;
        }
        (settled, frames)
    }

    /// Through carriers, a scale change lands in the event it was made in, at any depth.
    #[test]
    fn a_scale_change_lands_in_one_event_through_any_depth_of_carriers() {
        for depth in 0..=8 {
            assert_eq!(scale_through(depth, true), (true, 0), "{depth} containers deep");
        }
    }

    /// Without them it is still correct, and late: Masonry runs four rewrite passes an
    /// event, the slow path spends one per container, and the rest spills into a frame
    /// drawn half-scaled.
    ///
    /// Pinned so that it fails loudly if upstream ever changes the budget — at which
    /// point the carriers may be worth less than they are now.
    #[test]
    fn without_carriers_a_third_container_spills_into_the_next_frame() {
        assert_eq!(scale_through(2, false), (true, 0));
        assert_eq!(scale_through(3, false), (false, 1));
    }
}
