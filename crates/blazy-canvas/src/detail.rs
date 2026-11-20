//! Level of detail: how much of a node a canvas builds, by readability and by cost.

use std::any::TypeId;

use masonry::core::{Property, UpdateCtx};
use strum::IntoStaticStr;

/// How much detail a canvas child should draw at the current zoom level: fewer widgets,
/// not only fewer draw commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Detail {
    /// Full contents: header, body and interactive controls.
    Full,
    /// Header only; controls are stashed.
    Simplified,
    /// A flat filled rectangle. Contents are stashed and not laid out.
    Box,
}

impl Detail {
    /// The level's name, for a status line or a report.
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// The zoom levels at which a canvas switches between [`Detail`] levels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetailThresholds {
    /// Above this zoom, nodes are drawn in full and carry interactive controls.
    pub full: f64,
    /// Above this zoom (and below `full`), nodes keep widgets but drop their controls.
    /// Below it the canvas paints nodes itself and materialises nothing.
    pub simplified: f64,
}

impl Default for DetailThresholds {
    /// 0.1 and 0.02; cost is [`DetailBudget`]'s business.
    fn default() -> Self {
        Self {
            full: 0.1,
            simplified: 0.02,
        }
    }
}

impl DetailThresholds {
    /// Chooses a detail level for an effective scale factor.
    pub fn for_scale(&self, scale: f64) -> Detail {
        if scale > self.full {
            Detail::Full
        } else if scale > self.simplified {
            Detail::Simplified
        } else {
            Detail::Box
        }
    }
}

/// Widgets one canvas may keep in the tree, and how a level is chosen to stay under it.
///
/// The cost half of the level decision; [`DetailThresholds`] is the readability half, and
/// the stricter wins. In widgets, because a frame costs the same per widget whatever level
/// produced them; what a node costs at each level is the application's to say (defaults 4
/// and 1). A frame walks the window's tree, so canvases sharing a window should share one
/// budget ([`DetailBudget::split`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetailBudget {
    /// Widgets this canvas may put in the tree.
    pub widgets: usize,
    /// Widgets one node costs at [`Detail::Full`].
    pub full_cost: usize,
    /// Widgets one node costs at [`Detail::Simplified`].
    pub simplified_cost: usize,
    /// How far under the budget the estimate has to fall before a finer level is taken up
    /// again, as a fraction of it: larger than the visible set's jitter during a pan.
    pub hysteresis: f64,
}

/// Widgets one canvas may hold by default: about half a 60 Hz frame at the measured cost
/// per widget.
pub const DEFAULT_WIDGET_BUDGET: usize = 1200;

impl Default for DetailBudget {
    fn default() -> Self {
        Self {
            widgets: DEFAULT_WIDGET_BUDGET,
            full_cost: 4,
            simplified_cost: 1,
            hysteresis: 0.25,
        }
    }
}

impl DetailBudget {
    /// A budget that never binds, leaving the zoom thresholds as the only rule.
    pub const fn unlimited() -> Self {
        Self {
            widgets: usize::MAX,
            full_cost: 4,
            simplified_cost: 1,
            hysteresis: 0.0,
        }
    }

    /// This budget divided evenly between `ways` canvases sharing one window.
    pub fn split(self, ways: usize) -> Self {
        Self {
            widgets: self.widgets / ways.max(1),
            ..self
        }
    }

    /// Widgets one node costs at `level`.
    pub fn cost_of(&self, level: Detail) -> usize {
        match level {
            Detail::Full => self.full_cost,
            Detail::Simplified => self.simplified_cost,
            // The far field builds no widgets.
            Detail::Box => 0,
        }
    }

    /// The finest level whose widgets fit, given how many nodes are on screen; a finer
    /// level than `current` has to clear the budget by [`hysteresis`](Self::hysteresis).
    pub fn level_for(&self, visible: usize, current: Option<Detail>) -> Detail {
        let margin = 1.0 - self.hysteresis.clamp(0.0, 1.0);
        for level in [Detail::Full, Detail::Simplified] {
            // `Detail` is ordered finest-first, so `level < cur` is "finer than now".
            let finer = current.is_some_and(|cur| level < cur);
            let ceiling = if finer {
                (self.widgets as f64 * margin) as usize
            } else {
                self.widgets
            };
            if visible.saturating_mul(self.cost_of(level)) <= ceiling {
                return level;
            }
        }
        Detail::Box
    }
}

/// The detail level the canvas as a whole is showing (a node under the pointer may be
/// built finer), for a node to decide how much effort its painted stand-in deserves.
///
/// Set as a property on every child; read it in `layout`/`paint` and handle it in
/// [`Widget::property_changed`](masonry::core::Widget::property_changed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanvasDetail(pub Detail);

impl Property for CanvasDetail {
    fn static_default() -> &'static Self {
        static DEFAULT: CanvasDetail = CanvasDetail(Detail::Full);
        &DEFAULT
    }
}

impl Default for CanvasDetail {
    fn default() -> Self {
        *Self::static_default()
    }
}

impl CanvasDetail {
    /// Helper for [`Widget::property_changed`](masonry::core::Widget::property_changed): requests a relayout when the
    /// detail level changed.
    pub fn prop_changed(ctx: &mut UpdateCtx<'_>, property_type: TypeId) {
        if property_type == TypeId::of::<Self>() {
            ctx.request_layout();
        }
    }
}
