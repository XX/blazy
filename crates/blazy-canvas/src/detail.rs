//! Level of detail: how much of a node a canvas builds, by readability and by cost (§20.7, §29).

use std::any::TypeId;

use masonry::core::{Property, UpdateCtx};
use strum::IntoStaticStr;

/// How much detail a canvas child should draw at the current zoom level.
///
/// Level of detail serves two purposes, and the second matters more. The obvious
/// one is fewer draw commands per node. The important one is that at
/// [`Detail::Box`] a node can stash its contents entirely — and layout, not
/// painting, is what makes a large graph expensive.
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
///
/// Policy, not mechanism: how small a node has to get before its controls stop being
/// usable depends on how the application draws it. It lives here, on the canvas,
/// rather than baked into the library — the alternative is editing this crate to
/// retune a demo, which is a smell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetailThresholds {
    /// Above this zoom, nodes are drawn in full and carry interactive controls.
    pub full: f64,
    /// Above this zoom (and below `full`), nodes keep widgets but drop their controls.
    /// Below it the canvas paints nodes itself and materialises nothing.
    pub simplified: f64,
}

impl Default for DetailThresholds {
    /// 0.1 and 0.02, lowered from 0.2 and 0.05 by eye (§53.6): the higher pair dropped a
    /// node's controls and then its widget at zooms where it still read well. Cost is not
    /// this rule's business — [`DetailBudget`] keeps the tree affordable whatever these
    /// say, and the stricter of the two wins.
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
/// The second half of the level-of-detail decision, and the one derived from a
/// measurement rather than from how a node looks. [`DetailThresholds`] asks whether a
/// control is still large enough to use; this asks whether the tree that would result
/// is still affordable. Both rules apply and the **stricter one wins**, because they
/// guard against different failures: a slider three pixels tall is useless however
/// cheap it is, and four thousand widgets are unaffordable however legible they are.
///
/// **Why widgets and not nodes.** §20.2 measured the frame as the cost of walking the
/// widget tree, and a node is not one widget: at [`Detail::Full`] the example's node
/// carries a slider and a checkbox (which carries a label) and costs four, at
/// [`Detail::Simplified`] it costs one. Measured across the whole zoom range and two
/// graph sizes, a panned frame costs the same per widget in the tree whatever level
/// produced them (§29.1, and [`DEFAULT_WIDGET_BUDGET`] is derived from that figure) —
/// so widgets are the unit the ceiling belongs in, and the per-level cost is what
/// converts a node count into it.
///
/// **Why the costs are given rather than counted.** The canvas builds a node through
/// [`NodeSource`](crate::NodeSource) and never looks inside the result; how many widgets a level costs is
/// the application's knowledge, like the thresholds themselves (§20.7). The defaults
/// are the example's 4 and 1.
///
/// **The budget is a window quantity, not a canvas one.** What a frame walks is the
/// whole window's tree, and a screen of areas holds one canvas per area (§21, §29.1):
/// eight canvases each honestly inside a budget of their own put eight times that in
/// one window, and no canvas can see it happening. An application that tiles canvases
/// should divide one window budget between them — see [`DetailBudget::split`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetailBudget {
    /// Widgets this canvas may put in the tree.
    pub widgets: usize,
    /// Widgets one node costs at [`Detail::Full`].
    pub full_cost: usize,
    /// Widgets one node costs at [`Detail::Simplified`].
    pub simplified_cost: usize,
    /// How far under the budget the estimate has to fall before a finer level is
    /// taken up again, as a fraction of it.
    ///
    /// Without it the visible set's own jitter drives the switch: a pan moves nodes
    /// in and out at the viewport edge, and the count wobbles by 10–22% from frame to
    /// frame at every zoom worth budgeting (§29.1). A policy that flips level on each
    /// wobble rebuilds every visible node twice a second and costs more than the
    /// widgets it saves, so the margin is taken from that measured spread rather than
    /// picked.
    pub hysteresis: f64,
}

/// Widgets one canvas may hold by default.
///
/// Derived, not chosen: a panned frame costs 6.5–8.5 us per widget in the tree
/// (§29.1), so 1200 widgets is about 8–9 ms — half a 60 Hz frame, leaving the rest for
/// the far field, the links and everything else in the window. The bottom end matters
/// as much as the top: ordinary work at zoom 1–2 holds 50–100 widgets, two orders
/// below the ceiling, so the policy never touches it.
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

    /// This budget divided between `ways` canvases sharing one window.
    ///
    /// The honest way to spend a window budget on a screen of areas: the frame walks
    /// the window's tree, so the sum is what has to fit, and dividing it is the
    /// smallest thing that makes each canvas's decision add up to the window's
    /// (§29.1). Even shares because an area's cost does not depend on its size — a
    /// small area at a small zoom holds as much as a large one.
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
            // The far field builds no widgets at all; its cost is a scene, and the
            // budget cannot bid it lower — there is no coarser level to fall to
            // (§29.4).
            Detail::Box => 0,
        }
    }

    /// The finest level whose widgets fit, given how many nodes are on screen.
    ///
    /// `current` is the level in force, and it is what makes this hysteretic: staying
    /// where we are only has to fit the budget, while moving to a finer level has to
    /// clear it by [`hysteresis`](Self::hysteresis).
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

/// The detail level the canvas as a whole is showing.
///
/// Note this is the *global* level, not the level the individual node was built at:
/// a node under the pointer keeps its controls while everything around it is
/// simplified. Use it to decide how much effort a painted stand-in deserves — at
/// [`Detail::Simplified`] there are hundreds of nodes on screen and each draw command
/// is multiplied by that count, while at [`Detail::Full`] there are few and the
/// stand-in has to resemble the real controls closely enough that swapping them in on
/// hover is not jarring.
///
/// The canvas sets this property on every child when the zoom crosses a threshold.
/// Children opt in by reading it in `layout`/`paint` and handling it in
/// [`Widget::property_changed`](masonry::core::Widget::property_changed); children that ignore it simply always draw in
/// full.
///
/// A property rather than a trait method, so the canvas can host heterogeneous
/// children. It is also the same mechanism `rnd/architecture.md` §9 earmarks for
/// per-region `ui_scale`, which has the same shape: a value that flows down a
/// subtree and invalidates layout when it changes.
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
