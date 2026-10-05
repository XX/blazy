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
