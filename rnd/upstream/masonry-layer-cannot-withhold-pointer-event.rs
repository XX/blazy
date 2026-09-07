//! Reproducer: a layer root sees every pointer event before the tree, and has no way to
//! keep one out of it.
//!
//! Drop this file into a crate whose only dependency is
//!
//! ```toml
//! masonry = { git = "https://github.com/linebender/xilem.git", rev = "b81d8d7", default-features = false, features = ["testing"] }
//! ```
//!
//! and run it with `cargo run` (a debug build: two of the five scenarios show a
//! `debug_panic!`, which is a silent no-op in release).
//!
//! The scene is one layer root (`Gate`) over two leaf widgets side by side (`Probe`).
//! Every scenario sends the same gesture and prints who saw how many events.

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use masonry::accesskit::{Node as AccessNode, Role};
use masonry::core::{
    AccessCtx, ChildrenIds, EventCtx, Layer, LayoutCtx, MeasureCtx, NewWidget, NoAction, PaintCtx,
    PointerEvent, PropertiesMut, PropertiesRef, RegisterCtx, Widget, WidgetPod,
};
use masonry::dpi::PhysicalSize;
use masonry::imaging::Painter;
use masonry::kurbo::{Axis, Point, Size};
use masonry::layout::{LenReq, Length, SizeDef};
use masonry::testing::TestHarness;
use masonry::theme::default_property_set;
use masonry::ui_events::pointer::PointerButton;

const WINDOW: PhysicalSize<u32> = PhysicalSize::new(800, 400);
/// A gesture: one press, twenty moves, one release.
const MOVES: usize = 20;

/// A counter shared with the harness, so a scenario can read it without a `WidgetMut`.
#[derive(Clone, Default)]
struct Count(Rc<Cell<usize>>);

impl Count {
    fn bump(&self) {
        self.0.set(self.0.get() + 1);
    }

    fn get(&self) -> usize {
        self.0.get()
    }
}

/// When a widget should try to take pointer capture.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureOn {
    Down,
    Move,
}

/// What the layer root does with an event it has already dealt with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// Count it, which is all a layer can do today.
    Count,
    /// Count it and mark it handled — the flag goes out with the discarded context.
    SetHandled,
    /// Count it and try to take pointer capture from the pre-tree seat.
    Capture,
}

/// A leaf widget that counts the pointer events reaching it.
struct Probe {
    seen: Count,
    capture_on: Option<CaptureOn>,
    moves: usize,
}

impl Probe {
    fn new(seen: Count, capture_on: Option<CaptureOn>) -> Self {
        Self {
            seen,
            capture_on,
            moves: 0,
        }
    }
}

impl Widget for Probe {
    type Action = NoAction;

    fn on_pointer_event(
        &mut self,
        ctx: &mut EventCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        event: &PointerEvent,
    ) {
        self.seen.bump();
        match event {
            PointerEvent::Down(_) if self.capture_on == Some(CaptureOn::Down) => {
                ctx.capture_pointer()
            }
            PointerEvent::Move(_) if self.capture_on == Some(CaptureOn::Move) => {
                self.moves += 1;
                // The first move after the press — the moment a gesture becomes a drag,
                // and the moment a click/drag distinction would want to grab the pointer.
                if self.moves == 2 {
                    ctx.capture_pointer();
                }
            }
            _ => {}
        }
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

    fn paint(
        &mut self,
        _ctx: &mut PaintCtx<'_>,
        _props: &PropertiesRef<'_>,
        _painter: &mut Painter<'_>,
    ) {
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(
        &mut self,
        _ctx: &mut AccessCtx<'_>,
        _props: &PropertiesRef<'_>,
        _node: &mut AccessNode,
    ) {
    }
}

/// The layer root: two probes side by side, and a pre-tree seat over both.
struct Gate {
    left: WidgetPod<Probe>,
    right: WidgetPod<Probe>,
    seen: Count,
    policy: Policy,
}

impl Gate {
    fn new(seen: Count, policy: Policy, left: Probe, right: Probe) -> Self {
        Self {
            left: WidgetPod::new(left),
            right: WidgetPod::new(right),
            seen,
            policy,
        }
    }
}

impl Layer for Gate {
    /// The pre-tree seat. It is called for every pointer event, including those outside
    /// this widget's rectangle — and there is nothing it can do with one.
    fn capture_pointer_event(
        &mut self,
        ctx: &mut EventCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        _event: &PointerEvent,
    ) {
        self.seen.bump();
        match self.policy {
            Policy::Count => {}
            // `is_handled` lives in the context this pass throws away.
            Policy::SetHandled => ctx.set_handled(),
            // `allow_pointer_capture` is false here, always.
            Policy::Capture => ctx.capture_pointer(),
        }
    }
}

impl Widget for Gate {
    type Action = NoAction;

    fn as_layer(&mut self) -> Option<&mut dyn Layer> {
        Some(self)
    }

    fn register_children(&mut self, ctx: &mut RegisterCtx<'_>) {
        ctx.register_child(&mut self.left);
        ctx.register_child(&mut self.right);
    }

    fn measure(
        &mut self,
        _ctx: &mut MeasureCtx<'_>,
        _props: &PropertiesRef<'_>,
        axis: Axis,
        len_req: LenReq,
        _cross_length: Option<Length>,
    ) -> Length {
        let natural = match axis {
            Axis::Horizontal => f64::from(WINDOW.width),
            Axis::Vertical => f64::from(WINDOW.height),
        };
        match len_req {
            LenReq::MinContent | LenReq::MaxContent => Length::px(natural),
            LenReq::FitContent(space) => space,
        }
    }

    fn layout(&mut self, ctx: &mut LayoutCtx<'_>, _props: &PropertiesRef<'_>, size: Size) {
        let half = Size::new(size.width / 2.0, size.height);
        for (child, x) in [(&mut self.left, 0.0), (&mut self.right, half.width)] {
            let child_size = ctx.compute_size(child, SizeDef::fixed(half), half.into());
            ctx.run_layout(child, child_size);
            ctx.place_child(child, Point::new(x, 0.0));
        }
    }

    fn paint(
        &mut self,
        _ctx: &mut PaintCtx<'_>,
        _props: &PropertiesRef<'_>,
        _painter: &mut Painter<'_>,
    ) {
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::from_slice(&[self.left.id(), self.right.id()])
    }

    fn accessibility_role(&self) -> Role {
        Role::GenericContainer
    }

    fn accessibility(
        &mut self,
        _ctx: &mut AccessCtx<'_>,
        _props: &PropertiesRef<'_>,
        _node: &mut AccessNode,
    ) {
    }
}

/// The three counters a scenario reports.
struct Seats {
    layer: Count,
    left: Count,
    right: Count,
}

fn harness(policy: Policy, capture_on: Option<CaptureOn>) -> (TestHarness<Gate>, Seats) {
    let seats = Seats {
        layer: Count::default(),
        left: Count::default(),
        right: Count::default(),
    };
    let gate = Gate::new(
        seats.layer.clone(),
        policy,
        Probe::new(seats.left.clone(), capture_on),
        Probe::new(seats.right.clone(), None),
    );
    let mut harness =
        TestHarness::create_with_size(default_property_set(), NewWidget::new(gate), WINDOW);
    let _ = harness.redraw();
    (harness, seats)
}

/// The left half and the right half, in window coordinates.
fn left_of(step: usize) -> Point {
    Point::new(100.0 + step as f64, 200.0)
}

fn right_of(step: usize) -> Point {
    Point::new(500.0 + step as f64, 200.0)
}

/// A press on the left probe and twenty moves that stay on it: the shape of a drag that
/// a widget would want to grab the pointer for once it knows it is one.
fn drag_in_place(harness: &mut TestHarness<Gate>) {
    harness.mouse_move(left_of(0));
    harness.mouse_button_press(Some(PointerButton::Primary));
    for step in 1..=MOVES {
        harness.mouse_move(left_of(step));
    }
    harness.mouse_button_release(Some(PointerButton::Primary));
}

/// A press on the left probe, twenty moves that cross into the right one, a release.
fn drag_across(harness: &mut TestHarness<Gate>) {
    harness.mouse_move(left_of(0));
    harness.mouse_button_press(Some(PointerButton::Primary));
    for step in 0..MOVES {
        harness.mouse_move(right_of(step));
    }
    harness.mouse_button_release(Some(PointerButton::Primary));
}

/// Runs `f`, returning the `debug_panic!` message if it panicked.
///
/// The harness is left unusable afterwards and is dropped by the caller; that is fine,
/// each scenario builds its own.
fn catch(f: impl FnOnce()) -> Option<String> {
    let message: Arc<Mutex<Option<String>>> = Arc::default();
    let sink = message.clone();
    // The hook has to be `Send + Sync` even though it only ever runs on this thread;
    // installing it keeps the default hook's backtrace out of the output.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let text = info
            .payload()
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                info.payload()
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
            });
        *sink.lock().unwrap() = text;
    }));
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(f));
    std::panic::set_hook(previous);
    match outcome {
        Ok(()) => None,
        Err(_) => Some(
            message
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "<no message>".into()),
        ),
    }
}

fn row(what: &str, seats: &Seats) {
    println!(
        "{what:<52} layer {:>3}   left {:>3}   right {:>3}",
        seats.layer.get(),
        seats.left.get(),
        seats.right.get()
    );
}

fn main() {
    let events = MOVES + 3; // the hover, the press, the moves, the release
    println!(
        "masonry rev b81d8d7, one layer root over two leaf widgets, {events} pointer events per gesture"
    );
    println!("debug_assertions: {}\n", cfg!(debug_assertions));

    // 1. The seat exists and is genuinely ahead of the tree — and cannot use that.
    let (mut h, seats) = harness(Policy::SetHandled, None);
    drag_across(&mut h);
    row("1. layer sets handled on every event", &seats);
    assert_eq!(seats.layer.get(), events, "the layer sees every event");
    assert_eq!(
        seats.left.get() + seats.right.get(),
        events,
        "and every one of them reaches the tree anyway"
    );

    // 2. The one lever that does withhold an event: capture, taken on the press.
    let (mut h, seats) = harness(Policy::Count, Some(CaptureOn::Down));
    drag_across(&mut h);
    row("2. left probe captures on Down", &seats);
    assert_eq!(
        seats.right.get(),
        0,
        "capture keeps the moves from the right probe"
    );

    // 3. The same widget, one event later. This is the case a click/drag distinction
    //    needs: at the press nobody knows yet whether the gesture will become a drag.
    let (mut h, seats) = harness(Policy::Count, Some(CaptureOn::Move));
    let panic_on_move = catch(|| drag_in_place(&mut h));
    row(
        "3. left probe captures on the first Move after Down",
        &seats,
    );
    match panic_on_move {
        Some(message) => println!("   refused: {message}"),
        None => println!("   no panic (release build): capture_pointer returned without capturing"),
    }
    drop(h);

    // 4. Capture from the pre-tree seat, which is where a keymap would want it.
    let (mut h, seats) = harness(Policy::Capture, None);
    let panic_in_seat = catch(|| drag_across(&mut h));
    row("4. layer calls capture_pointer in its seat", &seats);
    match panic_in_seat {
        Some(message) => println!("   refused: {message}"),
        None => println!("   no panic (release build): capture_pointer returned without capturing"),
    }
    drop(h);

    // 5. A gesture with no press at all — a modal operator started by a key. There is
    //    no press to capture on, so every event of it reaches the tree.
    let (mut h, seats) = harness(Policy::SetHandled, None);
    for step in 0..=MOVES {
        h.mouse_move(left_of(step));
    }
    row("5. keyboard-started gesture: moves only", &seats);
    assert_eq!(
        seats.left.get(),
        MOVES + 1,
        "every event of a keyboard-started gesture reaches the tree"
    );

    println!(
        "\nrows 1 and 5: the layer saw everything first and the tree saw it too.\n\
         row 2: capture is the only thing that withholds an event.\n\
         rows 3 and 4: capture is available on the press, in the tree, and nowhere else."
    );
}
