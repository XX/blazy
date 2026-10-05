//! Carrying a region's interface scale into a subtree of Masonry widgets.
//!
//! This crate is not a set of controls. Masonry already has label, button, checkbox,
//! slider, switch, text input, flex and grid, and measurement showed they need no
//! replacing: **a stock widget follows the box properties someone pushes onto it**, so
//! scaling one costs no code of its own (§46). What Masonry has no answer for is where
//! the scale comes from — it has no inherited properties (§22.1), so `UiScale` sits on
//! the region's root and reaches nobody below it.
//!
//! So this crate owns the carrying, and it is two channels rather than one, because
//! measurement says they have different shapes:
//!
//! * **The box is data.** [`Metrics`] is the unscaled padding, corner, border and gap of a control; [`scale_box`]
//!   multiplies it and pushes it as ordinary properties. It needs to know nothing about the widget it is applied to, so
//!   it works on a stock `Button` exactly as on a widget of yours.
//! * **The text is a call.** A font size is not a property but a parley style, reachable only through
//!   `WidgetMut<Label>`. An application should not have to downcast a stock widget to make its caption follow the
//!   interface scale, so [`Label`] holds that end: it reads [`UiScale`] and resizes its own text.
//!
//! [`Bar`] is what puts the two together: a row that reads the scale of the region it
//! sits in and applies both channels to its children.
//!
//! # What a caller has to promise
//!
//! [`Metrics`] is the size a control has **at scale 1**, and it is the caller's to
//! declare. Nothing here reads a widget's current padding and multiplies it: a value
//! read back after one scaling is already scaled, and multiplying it again is how an
//! interface grows every time the user changes anything. The baseline is data the caller
//! keeps, not state this crate hides.

#![warn(missing_docs, unreachable_pub)]

mod bar;
mod label;

use blazy_areas::UiScale;
use masonry::core::{Widget, WidgetMut};
use masonry::layout::Length;
use masonry::properties::{BorderWidth, CornerRadius, Gap, Padding};

pub use crate::bar::{Bar, BarCounters};
pub use crate::label::Label;

/// What a control measures at scale 1.
///
/// Everything here goes into the frame through ordinary properties, which is why it can
/// be applied to a widget whose type is unknown. Constructed with `..Default::default()`
/// and extended in place: it is configuration, not a counter (§40.4).
#[derive(Clone, Copy, Debug, PartialEq)]
#[expect(clippy::exhaustive_structs, reason = "configuration, constructed by the caller")]
pub struct Metrics {
    /// Padding inside the control, in logical pixels at scale 1.
    pub padding: f64,
    /// Corner radius, in logical pixels at scale 1.
    pub corner: f64,
    /// Border thickness, in logical pixels at scale 1.
    pub border: f64,
    /// Gap between a container's children, in logical pixels at scale 1.
    pub gap: f64,
}

impl Default for Metrics {
    /// The metrics of a dense control: small padding, small corner, hairline border.
    ///
    /// Dense on purpose — this is a Blender-style UI, where a panel holds tens of
    /// controls and Masonry's own defaults are sized for a document.
    fn default() -> Self {
        Self {
            padding: 4.0,
            corner: 3.0,
            border: 1.0,
            gap: 4.0,
        }
    }
}

impl Metrics {
    /// The same metrics at `scale`.
    #[must_use]
    pub fn at(self, scale: f64) -> Self {
        Self {
            padding: self.padding * scale,
            corner: self.corner * scale,
            border: self.border * scale,
            gap: self.gap * scale,
        }
    }
}

/// Hands `scale` to a child, if it is not the one the child already has.
///
/// The one call every container between a region's root and its controls has to make,
/// and the trap §22.1 warns about in one line: Masonry has no inherited properties, so a
/// container that forgets this leaves everything below it at scale 1 — and a region
/// *itself* laid out at the new scale looks exactly right until you measure a control
/// inside it (§46). Returns whether anything was carried, so a caller can count it.
///
/// `applied` is where the caller keeps what it last handed down; the value is written
/// back, so an unchanged scale costs nothing.
pub fn carry_ui_scale<W: Widget + masonry::core::FromDynWidget + ?Sized>(
    ctx: &mut masonry::core::LayoutCtx<'_>,
    child: &mut masonry::core::WidgetPod<W>,
    applied: &mut Option<f64>,
    scale: f64,
) -> bool {
    if *applied == Some(scale) {
        return false;
    }
    *applied = Some(scale);
    ctx.mutate_child_later(child, move |mut child| {
        child.insert_prop(UiScale(scale));
    });
    true
}

/// Pushes `metrics` scaled by `scale` onto `widget`, as ordinary properties.
///
/// The box channel of §46, and the half that needs no knowledge of the widget: a stock
/// Masonry control resolves `Padding`, `CornerRadius`, `BorderWidth` and `Gap` from its
/// own property stack before falling back to the theme, so pushing them here is enough
/// to move its geometry. Measured on a stock `Button`: its insets went from 34x14 to
/// 42x42 under a pushed padding, without the button knowing anything about scale.
///
/// Text is not here, and cannot be: see [`Label`].
pub fn scale_box(widget: &mut WidgetMut<'_, dyn Widget>, metrics: Metrics, scale: f64) {
    let at = metrics.at(scale);
    widget.insert_prop(Padding::all(Length::px(at.padding)));
    widget.insert_prop(CornerRadius {
        radius: Length::px(at.corner),
    });
    widget.insert_prop(BorderWidth {
        width: Length::px(at.border),
    });
    widget.insert_prop(Gap {
        gap: Length::px(at.gap),
    });
}

#[cfg(test)]
mod tests;
