//! Counting what a frame actually costs: draw commands in the layer plan.
//!
//! §29 measured the frame as the cost of walking the widget tree and bounded it in
//! widgets. That left the other half unbounded: below the far-field threshold a canvas
//! holds no widgets at all and was still the most expensive thing on screen (§31 has
//! the figures), because the paint pass rebuilds the whole [`VisualLayerPlan`] every
//! frame and re-appends every widget's cached scene into it
//! (`masonry_core/src/passes/paint.rs`). What it appends is **draw commands**, and a
//! command is charged in every frame it sits in the scene whether or not anything about
//! it changed — an idle area pays the same as a busy one.
//!
//! So the quantity to count is commands, and the right place to count them is the plan
//! itself:
//!
//! * it is the **window's** total, which is the unit the cost belongs in — eight areas over one graph share one frame,
//!   and no single canvas can see the sum;
//! * it needs no cooperation from the application, which draws the far field and could otherwise under-report it;
//! * it is exact and machine-independent, so a threshold on it either holds or reports a real regression (§20.9).

use masonry::app::{VisualLayerKind, VisualLayerPlan};

/// Draw commands in every scene layer of a plan.
///
/// Counts [`Command`](masonry::imaging::record::Command)s, which is what the paint
/// pass appends and what a rasteriser turns into work — not shapes and not path
/// elements. That distinction is the whole finding: ten thousand curves inside one
/// stroke command cost a fifteenth of ten thousand stroke commands (§31.1).
///
/// External layers hold no scene of their own: they are holes for the host to fill
/// (§26.1), and what goes in them is not this frame's cost.
pub fn commands(plan: &VisualLayerPlan) -> usize {
    plan.layers
        .iter()
        .map(|layer| match &layer.kind {
            VisualLayerKind::Scene(scene) => scene.commands().len(),
            _ => 0,
        })
        .sum()
}
