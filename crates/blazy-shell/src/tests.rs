//! What the host promises, checked against a real widget tree.
//!
//! The unit tests next to each module check the pieces; these check the two claims
//! that only mean anything end to end — that a hole survives the trip from a widget
//! to the host, and that the device scale factor never reaches layout.

use std::cell::Cell;
use std::rc::Rc;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, EventCtx, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx, PaintLayerMode,
    PointerEvent, PropertiesMut, PropertiesRef, RegisterCtx, Widget, WindowEvent,
};
use masonry::dpi::PhysicalSize;
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Size};
use masonry::layout::{LenReq, Length};
use masonry::peniko::Color;
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;

use crate::{ExternalContent, Host};

/// A widget that counts its own layout passes, and can declare itself a hole once.
struct Counting {
    layouts: Rc<Cell<u64>>,
    /// Whether to declare an external layer when painting.
    external: bool,
}

/// A widget that counts the pointer events that reach it.
struct Probe {
    seen: Rc<Cell<u64>>,
}

impl Widget for Probe {
    type Action = NoAction;

    fn on_pointer_event(&mut self, _ctx: &mut EventCtx<'_>, _props: &mut PropertiesMut<'_>, _event: &PointerEvent) {
        self.seen.set(self.seen.get() + 1);
    }

    fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        _axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(100.0),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {}

    fn paint(&mut self, _ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, _painter: &mut Painter<'_>) {}

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(&mut self, _ctx: &mut AccessCtx<'_>, _props: &PropertiesRef<'_>, _node: &mut Node) {}
}

/// A widget that declares itself an isolated scene layer whenever it paints.
struct Layered {
    /// Paints performed, so a test can tell "did not paint" from "painted inline".
    paints: Rc<Cell<u64>>,
}

impl Widget for Layered {
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
            LenReq::MinContent | LenReq::MaxContent => Length::px(50.0),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {}

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        self.paints.set(self.paints.get() + 1);
        ctx.set_paint_layer_mode(PaintLayerMode::IsolatedScene);
        painter
            .fill(ctx.content_box(), Color::from_rgb8(0x20, 0x40, 0x60))
            .draw();
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

impl Widget for Counting {
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
            LenReq::MinContent | LenReq::MaxContent => Length::px(50.0),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {
        self.layouts.set(self.layouts.get() + 1);
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        if self.external {
            ctx.set_paint_layer_mode(PaintLayerMode::External);
        }
        painter
            .fill(ctx.content_box(), Color::from_rgb8(0x30, 0x30, 0x40))
            .draw();
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

/// One frame the way the window loop runs one: animation first, then draw.
///
/// The animation step is not decoration. An external widget keeps its declaration
/// alive by repainting (§26.1), and repainting is what its animation frame asks for,
/// so a host that never runs one loses every hole after the first frame — which is
/// what `a_declaration_without_a_repaint_is_lost` shows from the other side.
fn frame<W: Widget>(harness: &mut TestHarness<W>, host: &mut Host, size: Size) -> crate::Frame {
    harness.animate_ms(16);
    let (plan, _) = harness.redraw();
    host.render(&plan, size).unwrap()
}

fn harness<W: Widget>(widget: W) -> TestHarness<W> {
    TestHarness::create_with_size(
        default_property_set(),
        NewWidget::new(widget),
        PhysicalSize::new(200, 120),
    )
}

/// The requirement of §4.3 and §14, end to end: a widget says the host draws here,
/// and the host is told where.
#[test]
fn an_external_widget_reaches_the_host_as_a_hole() {
    let mut harness = harness(ExternalContent::new(Size::new(200.0, 120.0)));
    let mut host = Host::any().unwrap();

    let frame = frame(&mut harness, &mut host, Size::new(200.0, 120.0));

    assert_eq!(frame.holes.len(), 1, "the hole did not reach the host");
    assert_eq!(frame.holes[0].rect.width(), 200.0);
    assert_eq!(frame.holes[0].widget_id, harness.root_id());
}

/// And it keeps reaching it: the declaration lives one paint (§26.1), which is why
/// [`ExternalContent`] keeps painting.
#[test]
fn the_hole_survives_frames_in_which_nothing_changes() {
    let mut harness = harness(ExternalContent::new(Size::new(200.0, 120.0)));
    let mut host = Host::any().unwrap();
    // The harness has already painted once by the time it hands the tree over, and
    // that frame never reached this host: count from here.
    let declared_before = harness.root_widget().declarations();

    for index in 0..8 {
        let rendered = frame(&mut harness, &mut host, Size::new(200.0, 120.0));
        assert_eq!(rendered.holes.len(), 1, "hole lost on frame {index}");
    }

    let declared = harness.root_widget().declarations() - declared_before;
    assert_eq!(
        host.counters().holes,
        declared,
        "every declaration should have become a hole"
    );
}

/// The trap itself, pinned rather than left to be rediscovered: a widget that
/// declares a hole only while it happens to be repainting stops being a hole, and
/// its cached content is drawn inline instead.
#[test]
fn a_declaration_without_a_repaint_is_lost() {
    let mut harness = harness(Counting {
        layouts: Rc::new(Cell::new(0)),
        external: true,
    });
    let mut host = Host::any().unwrap();

    // The widget has to be made to paint once, because the harness has already run
    // the first paint by the time it hands the tree over.
    harness.edit_root_widget(|mut root| root.ctx.request_paint_only());
    assert_eq!(
        frame(&mut harness, &mut host, Size::new(200.0, 120.0)).holes.len(),
        1,
        "the frame it painted in carries the declaration"
    );

    assert_eq!(
        frame(&mut harness, &mut host, Size::new(200.0, 120.0)).holes.len(),
        0,
        "the next frame is clean, so the widget does not paint and the hole vanishes"
    );
}

/// A frame reaches the screen as opaque pixels, so it has to *be* opaque.
///
/// A widget tree is under no obligation to cover the window: `AreaScreen` paints its
/// splitter bars and leaves the rest to its areas, and an area that paints no
/// background leaves the frame transparent underneath. Presenting that means
/// flattening away the alpha — and with it the anti-aliasing the rasteriser just
/// computed, which shows up as staircased curves and bright specks where a low
/// alpha's unpremultiplied colour runs to white. Measured on the area screen before
/// this was fixed: 11 260 partly transparent pixels in an 800x600 frame, 220 of them
/// near-white. The base colour is what makes the composition opaque instead.
#[test]
fn a_window_frame_is_opaque() {
    let mut harness = harness(PartialCover);
    let mut host = Host::any().unwrap();

    let (plan, _) = harness.redraw();
    let bare = host.render(&plan, Size::new(200.0, 120.0)).unwrap();
    assert!(
        transparent_pixels(&bare.image) > 0,
        "the tree does not cover the window, so the frame starts out transparent"
    );

    let mut host = Host::any().unwrap().with_background(Color::from_rgb8(0x14, 0x14, 0x18));
    let frame = host.render(&plan, Size::new(200.0, 120.0)).unwrap();
    assert_eq!(
        transparent_pixels(&frame.image),
        0,
        "a base colour makes every pixel opaque, edges included"
    );
}

/// Pixels that are not fully opaque.
fn transparent_pixels(image: &masonry::imaging::RgbaImage) -> usize {
    image
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[3] != 255)
        .count()
}

/// A widget that paints a shape in the middle and leaves the rest of the window bare.
struct PartialCover;

impl Widget for PartialCover {
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
            LenReq::MinContent | LenReq::MaxContent => Length::px(50.0),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, _ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, _size: Size) {}

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, painter: &mut Painter<'_>) {
        // A circle, so the frame has anti-aliased edges to lose.
        let box_rect = ctx.content_box();
        painter
            .fill(
                masonry::kurbo::Circle::new(box_rect.center(), box_rect.height() / 3.0),
                Color::from_rgb8(0x80, 0x60, 0xa0),
            )
            .draw();
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

/// §9's third multiplier, held to the same line as the other two: the device scale
/// belongs to composition. Going through the host changes the pixels and nothing
/// else; going through the render root's own rescale is what a relayout looks like.
#[test]
fn the_device_scale_does_not_reach_layout() {
    let layouts = Rc::new(Cell::new(0));
    let mut harness = harness(Counting {
        layouts: layouts.clone(),
        external: false,
    });
    let mut host = Host::any().unwrap();

    let (plan, _) = harness.redraw();
    let before = layouts.get();

    let mut sizes = Vec::new();
    for scale in [1.0, 1.5, 2.0, 4.0] {
        host.set_device_scale(scale);
        let frame = host.render(&plan, Size::new(200.0, 120.0)).unwrap();
        sizes.push(frame.image.width);
    }

    assert_eq!(layouts.get(), before, "composing at a new scale laid something out");
    assert_eq!(sizes, vec![200, 300, 400, 800], "the frame follows the scale");

    // The contrast, so that "no layouts" is not just an accident of a quiet tree:
    // resizing the window is a relayout, and the device scale deliberately is not.
    harness.process_window_event(WindowEvent::Resize(PhysicalSize::new(300, 200)));
    let _ = harness.redraw();
    assert!(layouts.get() > before, "resizing the window relayouts the tree");
}

// --- MARK: GPU PATH

/// The frame path that keeps the frame on the GPU, checked where there is one.
///
/// Skipped rather than failed on a machine with no usable device: whether a runner has
/// a GPU is a fact about the runner, and §27.5 says what that costs in coverage.
#[cfg(feature = "vello")]
#[test]
fn the_gpu_path_draws_without_touching_main_memory() {
    use masonry::dpi::PhysicalSize;

    use crate::gpu::GpuFrames;

    let Ok(mut frames) = GpuFrames::offscreen(PhysicalSize::new(200, 120)) else {
        eprintln!("no graphics device here; skipping the GPU path");
        return;
    };

    let mut harness = harness(ExternalContent::new(Size::new(200.0, 120.0)));
    for _ in 0..4 {
        harness.animate_ms(16);
        let (plan, _) = harness.redraw();
        frames.draw(&plan, Size::new(200.0, 120.0), 1.0).expect("the GPU draws");
    }
    frames.wait();

    let counters = frames.counters();
    assert_eq!(counters.frames, 4);
    assert_eq!(counters.cpu_bytes, 0, "the frame went through main memory");
    assert_eq!(counters.readbacks, 0, "the frame was copied back from the GPU");
    assert_eq!(frames.holes().len(), 1, "holes survive the GPU path (§4.3)");
}

/// The instrument a benchmark checks its own GPU timings with (§32.4).
///
/// Two claims, and the raster table leans on both: what comes back is the frame that
/// was drawn, and reading it back is *counted* — the zero in
/// `the_gpu_path_does_not_read_the_frame_back` says something only because this
/// number moves when a readback really happens.
#[cfg(feature = "vello")]
#[test]
fn a_frame_read_back_is_the_frame_that_was_drawn() {
    use crate::gpu::GpuFrames;

    let Ok(mut frames) = GpuFrames::offscreen(PhysicalSize::new(200, 120)) else {
        eprintln!("no graphics device here; skipping the GPU path");
        return;
    };

    let mut harness = harness(Counting {
        layouts: Rc::new(Cell::new(0)),
        external: false,
    });
    let (plan, _) = harness.redraw();
    frames.draw(&plan, Size::new(200.0, 120.0), 1.0).expect("the GPU draws");

    let pixels = frames.read_pixels();
    assert_eq!(pixels.len(), 200 * 120 * 4, "tightly packed, padding dropped");
    assert!(
        pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0),
        "the frame came back empty"
    );
    assert_eq!(frames.counters().readbacks, 1, "a readback went uncounted");

    // A strip is the same pixels, which is what lets a per-frame check read half a
    // megabyte instead of thirteen.
    let row = 200 * 4;
    let strip = frames.read_rows(10, 4);
    assert_eq!(strip, pixels[10 * row..14 * row], "a strip is part of the frame");
    assert_eq!(frames.counters().readbacks, 2);

    // Rows past the end are clamped rather than panicking: a caller reading a strip
    // does not want to know the texture's height.
    assert!(frames.read_rows(119, 8).len() == row);
    assert!(frames.read_rows(200, 4).is_empty());
}

/// The device scale reaches the GPU path the same way it reaches the other one.
#[cfg(feature = "vello")]
#[test]
fn the_gpu_texture_follows_the_device_scale() {
    use masonry::dpi::PhysicalSize;

    use crate::gpu::GpuFrames;

    let Ok(mut frames) = GpuFrames::offscreen(PhysicalSize::new(200, 120)) else {
        eprintln!("no graphics device here; skipping the GPU path");
        return;
    };

    let mut harness = harness(Counting {
        layouts: Rc::new(Cell::new(0)),
        external: false,
    });
    let (plan, _) = harness.redraw();

    frames.draw(&plan, Size::new(200.0, 120.0), 2.0).expect("the GPU draws");
    assert_eq!(frames.size(), PhysicalSize::new(400, 240));
    assert_eq!(frames.texture().width(), 400, "the texture was reallocated");
}

/// The trap of §26.1, on the mode per-area caching is built out of (§36.1).
///
/// `PaintLayerMode::IsolatedScene` lives exactly one paint, like `External` — the paint
/// pass sets every widget's mode back to `Inline` before deciding whether to paint it at
/// all, and a clean widget is not painted. So the layer a cache would key on **vanishes
/// on the first frame in which its owner has nothing to redraw**, which is precisely the
/// frame the cache exists for. Measured here rather than argued: one layer on the frame
/// it painted in, none on the next.
#[test]
fn an_isolated_layer_lasts_one_paint() {
    let paints = Rc::new(Cell::new(0));
    let mut harness = harness(Layered { paints: paints.clone() });

    harness.edit_root_widget(|mut root| root.ctx.request_paint_only());
    let (plan, _) = harness.redraw();
    let painted = plan.layers.len();

    let before = paints.get();
    let (plan, _) = harness.redraw();
    let clean = plan.layers.len();

    assert_eq!(painted, 1, "the frame it painted in has its layer");
    assert_eq!(clean, 1, "a plan always has at least the root layer");
    assert_eq!(paints.get(), before, "the clean frame did not paint it");
}

/// The host seat: what a driver in front of `RenderRoot` can do (§39.5).
mod host_seat {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;

    use masonry::app::{RenderRoot, RenderRootOptions, WindowSizePolicy};
    use masonry::core::{Handled, NewWidget, PointerEvent, PointerInfo, PointerType, PointerUpdate};
    use masonry::dpi::{PhysicalPosition, PhysicalSize};
    use masonry::theme::default_property_set;
    use masonry::ui_events::pointer::PointerState;

    use super::Probe;
    use crate::window::{ShellDriver, deliver_pointer};

    /// A driver that takes everything, or nothing.
    struct Seat {
        withhold: bool,
        seen: Rc<Cell<u64>>,
    }

    impl ShellDriver for Seat {
        fn pointer_event(&mut self, _root: &mut RenderRoot, _event: &PointerEvent) -> Handled {
            self.seen.set(self.seen.get() + 1);
            if self.withhold { Handled::Yes } else { Handled::No }
        }
    }

    fn root(seen: &Rc<Cell<u64>>) -> RenderRoot {
        RenderRoot::new(
            NewWidget::new(Probe { seen: seen.clone() }),
            |_signal| {},
            RenderRootOptions {
                default_properties: Arc::new(default_property_set()),
                use_system_fonts: false,
                size_policy: WindowSizePolicy::User,
                size: PhysicalSize::new(200, 100),
                scale_factor: 1.0,
                test_font: None,
            },
        )
    }

    fn moved(x: f64, y: f64) -> PointerEvent {
        PointerEvent::Move(PointerUpdate {
            pointer: PointerInfo {
                pointer_id: None,
                persistent_device_id: None,
                pointer_type: PointerType::Mouse,
            },
            current: PointerState {
                position: PhysicalPosition::new(x, y),
                ..Default::default()
            },
            coalesced: vec![],
            predicted: vec![],
        })
    }

    /// The one thing this seat can do that no other can: the tree does not see it.
    #[test]
    fn a_withheld_event_never_reaches_the_tree() {
        let widget_seen = Rc::new(Cell::new(0));
        let seat_seen = Rc::new(Cell::new(0));
        let mut root = root(&widget_seen);
        let mut driver = Seat {
            withhold: true,
            seen: seat_seen.clone(),
        };

        for step in 0..5 {
            let handled = deliver_pointer(&mut driver, &mut root, moved(10.0 + f64::from(step), 10.0));
            assert!(handled.is_handled(), "the seat took it");
        }

        assert_eq!(seat_seen.get(), 5, "the seat saw every event");
        assert_eq!(widget_seen.get(), 0, "and the tree saw none of them");
    }

    /// And the other half of the switch: a seat that takes nothing changes nothing.
    #[test]
    fn what_the_seat_passes_reaches_the_tree() {
        let widget_seen = Rc::new(Cell::new(0));
        let seat_seen = Rc::new(Cell::new(0));
        let mut root = root(&widget_seen);
        let mut driver = Seat {
            withhold: false,
            seen: seat_seen.clone(),
        };

        for step in 0..5 {
            deliver_pointer(&mut driver, &mut root, moved(10.0 + f64::from(step), 10.0));
        }

        assert_eq!(seat_seen.get(), 5);
        assert!(widget_seen.get() > 0, "the tree got what the seat did not take");
    }
}

/// The cause of a failure has to survive the trip to the caller.
///
/// A library that wraps somebody else's error and drops it makes every downstream
/// `anyhow` chain end at our variant name (§15.1). Cheap to keep, invisible to lose,
/// so it is pinned here rather than trusted.
#[test]
fn an_error_hands_on_its_cause() {
    let backend = crate::BackendError::Unavailable {
        backend: crate::Backend::VelloCpu,
        reason: "no device".to_string(),
    };
    let host = crate::HostError::from(backend);
    let presented = crate::PresentError::from(host);
    let shell = crate::window::Error::from(presented);

    let mut causes = 0;
    let mut error: &(dyn std::error::Error + 'static) = &shell;
    while let Some(source) = error.source() {
        causes += 1;
        error = source;
    }
    assert_eq!(
        causes, 3,
        "shell -> present -> host -> backend, all the way to the leaf"
    );
    assert!(shell.to_string().contains("no device"), "{shell}");
}
