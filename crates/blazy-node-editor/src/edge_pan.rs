//! Scrolling the view at the edge of an area while an operator holds a node, a link or a
//! band — Blender's edge pan, and a choice the application makes rather than a behaviour.
//!
//! Two switches, and the view scrolls only when both are on:
//!
//! * [`EditorSession::edge_pan`](crate::EditorSession::edge_pan) — the application's: how wide the band at the edge is,
//!   how fast the view goes, and `None` for no edge pan anywhere. Off by default: with it off, a gesture carries on
//!   past the area's border and the view stands still, which is what every editor did before.
//! * [`EDGE_PAN`] on a binding — which gestures scroll, as data: the default keymap puts it on `node.move` and
//!   `link.drag`, and a keymap file can take it off one of them without touching the others. A binding without it does
//!   not scroll.
//!
//! The driver does the scrolling, not the operator, because the interesting case is a
//! pointer standing still in the band: it sends no events, so no operator hears anything.
//! The editor asks for an animation frame while the gesture's pointer is in the band and
//! for none otherwise — a window that keeps asking never idles (§36) — moves the view on
//! each frame, and hands the running operator a move at the same screen position, which
//! is a new position in canvas coordinates: the node being dragged travels with the view.
//! The arithmetic is Blender's (`view2d_edge_pan.cc`) and lives here, apart from any
//! widget, so a test can check it with no tree; the numbers are in §55.

use masonry::core::keyboard::Modifiers;
use masonry::kurbo::{Point, Rect, Size, Vec2};

/// The binding property that asks for edge pan: `edge_pan = true` on a modal operator's
/// binding, as `view2d_edge_pan` is on Blender's `transform.translate`.
pub const EDGE_PAN: &str = "edge_pan";

/// How the view scrolls at the edge of an area while an operator holds the pointer.
///
/// It scrolls only when both switches are on: the session holds one of these
/// ([`EditorSession::edge_pan`](crate::EditorSession::edge_pan), `None` by default), and the
/// running operator's binding carries [`EDGE_PAN`]. The view moves on animation frames,
/// asked for only while the gesture's pointer is in the band, so a pointer standing still
/// there keeps scrolling and a window with no such gesture idles (§55).
///
/// Distances are in the editor's logical pixels and speeds in logical pixels per second
/// on screen, whatever the zoom: the defaults are Blender's node editor's
/// (`NODE_EDGE_PAN_*`), at its 20 pixels to a UI unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgePan {
    /// How far inside the area's edge the band begins.
    pub inside: f64,
    /// How far outside the area's edge the pointer may be and still scroll, or `None` for
    /// anywhere — Blender's node editor uses the whole screen.
    pub outside: Option<f64>,
    /// The distance past the band's inner edge over which the speed rises to
    /// [`max_speed`](Self::max_speed).
    pub ramp: f64,
    /// The speed at the end of the ramp, in pixels per second.
    pub max_speed: f64,
    /// Seconds from entering the band to full speed, eased in, per axis.
    ///
    /// What keeps a drag that brushes the band from throwing the view.
    pub delay: f64,
    /// How much the zoom changes the speed, from 0 (none) to 1 (in proportion): zoomed in,
    /// the view scrolls faster on screen, so it covers a similar distance of the graph.
    pub zoom_influence: f64,
}

impl Default for EdgePan {
    fn default() -> Self {
        Self {
            inside: 40.0,
            outside: None,
            ramp: 20.0,
            max_speed: 520.0,
            delay: 0.5,
            zoom_influence: 0.5,
        }
    }
}

impl EdgePan {
    /// Which way the view should move, per axis (`-1`, `0` or `1`), for a pointer at
    /// `pointer` in an area of `size`.
    ///
    /// Towards the edge the pointer is past: a pointer beyond the right edge moves the view
    /// right, which slides the graph left under it.
    pub fn direction(&self, pointer: Point, size: Size) -> (i8, i8) {
        if let Some(outside) = self.outside
            && !Rect::from_origin_size(Point::ORIGIN, size)
                .inflate(outside, outside)
                .contains(pointer)
        {
            return (0, 0);
        }
        let inner = self.inner(size);
        let axis = |at: f64, min: f64, max: f64| {
            if at > max {
                1
            } else if at < min {
                -1
            } else {
                0
            }
        };
        (axis(pointer.x, inner.x0, inner.x1), axis(pointer.y, inner.y0, inner.y1))
    }

    /// The part of an area of `size` where the pointer does not scroll.
    ///
    /// Collapsed to the middle when the area is narrower than two bands, so that a small
    /// area still has a place to stand still in.
    pub fn inner(&self, size: Size) -> Rect {
        let x = self.inside.min(size.width / 2.0);
        let y = self.inside.min(size.height / 2.0);
        Rect::new(x, y, size.width - x, size.height - y)
    }

    /// The speed along one axis, in pixels per second, for a pointer `past` pixels beyond
    /// the band's inner edge, `held` seconds after it entered the band, at view scale
    /// `zoom`.
    pub fn speed(&self, past: f64, held: f64, zoom: f64) -> f64 {
        let distance = if self.ramp > 0.0 {
            (past / self.ramp).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let delay = if self.delay > 0.01 {
            smootherstep(held / self.delay)
        } else {
            1.0
        };
        let zoom = 1.0 + self.zoom_influence.clamp(0.0, 1.0) * (zoom - 1.0);
        distance * delay * zoom * self.max_speed
    }
}

/// Blender's ease: 0 at 0, 1 at 1, flat at both ends.
fn smootherstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * x * (x * (x * 6.0 - 15.0) + 10.0)
}

/// What edge pan has done in one editor, summed over its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EdgePanCounters {
    /// Animation frames the editor asked for to scroll by.
    ///
    /// The number that says the window can idle: it does not move while no gesture's
    /// pointer is in the band, and a test that sees it move afterwards has found a window
    /// that keeps itself awake.
    pub frames: u64,
    /// Frames that moved the view.
    pub pans: u64,
    /// Gestures cancelled after scrolling, whose view was put back where it was.
    pub restores: u64,
}

/// One gesture's edge pan, as the editor keeps it between events and frames.
#[derive(Debug, Default)]
pub(crate) struct EdgePanState {
    /// Whether a gesture that may scroll is running.
    pub(crate) gesture: bool,
    /// Whether the pointer has been inside the band's inner edge during this gesture.
    ///
    /// Blender's rule, and the reason is the same: a drag that starts in the band — a node
    /// grabbed near the edge — would otherwise throw the view before the user moved at all.
    pub(crate) enabled: bool,
    /// Where the gesture's pointer last was, in the editor's own space.
    pub(crate) pointer: Option<Point>,
    /// The modifiers it last came with, for the move a frame hands the operator.
    pub(crate) mods: Modifiers,
    /// Nanoseconds of animation frames since the gesture began: the clock the delay runs on.
    pub(crate) clock_ns: u64,
    /// When the pointer entered the band, per axis, on that clock.
    pub(crate) entered: (Option<u64>, Option<u64>),
    /// How far this gesture has moved the view, so a cancel can put it back.
    pub(crate) total: Vec2,
    /// The runtime's cancel count when the gesture began.
    pub(crate) cancels: u64,
    /// Whether a frame has been asked for and not yet received.
    pub(crate) frame_pending: bool,
    pub(crate) counters: EdgePanCounters,
}

impl EdgePanState {
    /// Starts a gesture: everything but the counters goes back to nothing.
    pub(crate) fn begin(&mut self, cancels: u64) {
        let counters = self.counters;
        *self = Self {
            gesture: true,
            cancels,
            counters,
            ..Self::default()
        };
    }

    /// Notes where the pointer is, and says whether it is in the band of a gesture
    /// allowed to scroll.
    pub(crate) fn track(&mut self, config: &EdgePan, pointer: Option<Point>, size: Size) -> bool {
        if let Some(pointer) = pointer {
            self.pointer = Some(pointer);
            if config.inner(size).contains(pointer) {
                self.enabled = true;
            }
        }
        self.in_band(config, size)
    }

    /// Whether the gesture's pointer is where the view should scroll.
    pub(crate) fn in_band(&self, config: &EdgePan, size: Size) -> bool {
        self.enabled
            && self
                .pointer
                .is_some_and(|pointer| config.direction(pointer, size) != (0, 0))
    }

    /// Advances the clock by one frame and says how far the view moves in it, as a pan of
    /// the canvas: the opposite of the way the view goes.
    pub(crate) fn step(&mut self, config: &EdgePan, size: Size, zoom: f64, interval_ns: u64) -> Vec2 {
        let Some(pointer) = self.pointer.filter(|_| self.enabled) else {
            return Vec2::ZERO;
        };
        self.clock_ns += interval_ns;
        let (dx, dy) = config.direction(pointer, size);
        let inner = config.inner(size);
        let clock = self.clock_ns;
        let held = |entered: &mut Option<u64>, dir: i8| {
            if dir == 0 {
                *entered = None;
                return 0.0;
            }
            let since = *entered.get_or_insert(clock);
            (clock - since) as f64 * 1e-9
        };
        let seconds = interval_ns as f64 * 1e-9;
        let held_x = held(&mut self.entered.0, dx);
        let held_y = held(&mut self.entered.1, dy);
        let past_x = (pointer.x - inner.x1).max(inner.x0 - pointer.x);
        let past_y = (pointer.y - inner.y1).max(inner.y0 - pointer.y);
        let vx = f64::from(dx) * config.speed(past_x, held_x, zoom);
        let vy = f64::from(dy) * config.speed(past_y, held_y, zoom);
        -Vec2::new(vx, vy) * seconds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Size = Size::new(400.0, 300.0);

    #[test]
    fn the_band_is_inside_the_edge_and_the_direction_points_past_it() {
        let pan = EdgePan::default();
        assert_eq!(pan.direction(Point::new(200.0, 150.0), AREA), (0, 0));
        assert_eq!(pan.direction(Point::new(390.0, 150.0), AREA), (1, 0));
        assert_eq!(pan.direction(Point::new(-50.0, 290.0), AREA), (-1, 1));
        assert_eq!(pan.direction(Point::new(5000.0, 150.0), AREA), (1, 0), "no outer limit");
        let limited = EdgePan {
            outside: Some(10.0),
            ..pan
        };
        assert_eq!(limited.direction(Point::new(5000.0, 150.0), AREA), (0, 0));
    }

    #[test]
    fn the_speed_ramps_with_distance_eases_in_with_time_and_follows_the_zoom() {
        let pan = EdgePan::default();
        assert_eq!(pan.speed(10.0, 10.0, 1.0), pan.max_speed / 2.0, "half way up the ramp");
        assert_eq!(pan.speed(1000.0, 10.0, 1.0), pan.max_speed, "past it, full speed");
        assert_eq!(pan.speed(1000.0, 0.0, 1.0), 0.0, "nothing on entering the band");
        assert_eq!(
            pan.speed(1000.0, 0.25, 1.0),
            pan.max_speed / 2.0,
            "half way through the delay"
        );
        assert_eq!(pan.speed(1000.0, 10.0, 3.0), pan.max_speed * 2.0, "zoomed in, faster");
    }

    /// A pointer that starts in the band does not scroll until it has been inside.
    #[test]
    fn a_gesture_begun_in_the_band_waits_for_the_pointer_to_come_inside() {
        let pan = EdgePan::default();
        let mut state = EdgePanState::default();
        state.begin(0);
        assert!(!state.track(&pan, Some(Point::new(395.0, 150.0)), AREA));
        assert_eq!(state.step(&pan, AREA, 1.0, 1_000_000_000), Vec2::ZERO);
        assert!(!state.track(&pan, Some(Point::new(200.0, 150.0)), AREA));
        assert!(state.track(&pan, Some(Point::new(395.0, 150.0)), AREA));
        state.step(&pan, AREA, 1.0, 0);
        let moved = state.step(&pan, AREA, 1.0, 1_000_000_000);
        assert!(moved.x < 0.0 && moved.y == 0.0, "the graph slides left: {moved:?}");
    }
}
