//! The Phase 0 pass criteria, as machine-checked assertions.
//!
//! `bench.rs` produces numbers; this module decides whether they still hold, and
//! renders the answer twice: as a human report on stdout, and as JSON for CI to
//! archive and diff across commits.
//!
//! **What is gated and what is not.** Every criterion but one is a *counter* —
//! how many children were laid out, how many widgets are in the tree, how many
//! times the far-field scene was re-recorded. Counters are deterministic: the same
//! commit gives the same number on a laptop and on a shared CI runner, so a
//! threshold on one either holds or reports a real regression. Wall-clock
//! milliseconds are not: a noisy runner moves them by a factor that would force
//! any honest threshold so wide it stops catching anything. They are reported —
//! trends matter — but they are never a pass/fail bound.
//!
//! The exception is [`Kind::Timing`]: "frame cost does not follow the graph size"
//! is a claim about time and cannot be restated as a counter. It survives as a
//! gate because it compares two times measured in the same process minutes apart,
//! and because the margin is enormous — §20.5 measured 1.1x against a bound of
//! half the node ratio, which for a 16x sweep is 8x. Runner noise does not span
//! that. Its counter sibling, `live_set_independent_of_graph_size`, is gated
//! strictly and would catch the same regression a frame earlier.

use std::fmt::Write as _;

/// Whether a criterion is bounded by a deterministic counter or by wall time.
///
/// See the module docs: the distinction is what decides how tight the bound may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Timing,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Timing => "timing",
        }
    }
}

/// One pass criterion, evaluated.
pub struct Criterion {
    /// Stable identifier, for diffing JSON reports across commits.
    pub name: &'static str,
    /// What the criterion asserts, in words.
    pub claim: &'static str,
    pub kind: Kind,
    pub measured: f64,
    /// The value `measured` must stay below.
    pub bound: f64,
    pub unit: &'static str,
}

impl Criterion {
    pub fn passed(&self) -> bool {
        self.measured < self.bound
    }
}

/// One scenario's numbers. Informational: nothing here is a gate.
pub struct ScenarioRecord {
    pub name: &'static str,
    pub frames: usize,
    pub mean_ms: f64,
    pub worst_ms: f64,
    pub materialised: usize,
    pub detail: String,
    pub child_layouts_per_frame: f64,
    pub builds_per_frame: f64,
    pub far_repaints_per_frame: f64,
}

/// One point of the scaling sweep.
pub struct SweepRecord {
    pub nodes: usize,
    pub visible: usize,
    pub idle_ms: f64,
    pub pan_ms: f64,
}

/// Everything one benchmark run produced.
pub struct Outcome {
    pub nodes: usize,
    pub viewport: (u32, u32),
    /// Whether this was the reduced set CI runs.
    pub quick: bool,
    pub criteria: Vec<Criterion>,
    pub scenarios: Vec<ScenarioRecord>,
    pub sweep: Vec<SweepRecord>,
}

impl Outcome {
    /// False if any criterion failed. This is what sets the process exit code.
    ///
    /// An empty list is a failure, not a pass. Vacuous truth is the wrong default
    /// here: a run that evaluated no criteria is a broken run, and the failure mode
    /// it guards against — a scenario quietly renamed, so `evaluate` stops finding
    /// it — is exactly the one that would otherwise turn CI green by removing the
    /// checks rather than by passing them.
    pub fn passed(&self) -> bool {
        !self.criteria.is_empty() && self.criteria.iter().all(Criterion::passed)
    }

    /// Prints the criteria, pass or fail, with the number each was decided on.
    ///
    /// A criterion prints its measurement and its bound even when it passes: a
    /// number drifting towards its bound is the warning that a hard failure is
    /// not, and the only way to see the drift is to log it while it still passes.
    pub fn report(&self) {
        println!("\nPhase 0 criteria{}", if self.quick { " (quick set)" } else { "" });
        for criterion in &self.criteria {
            println!(
                "  [{}] {:<52} {:>8.2} < {:>8.2} {} [{}]",
                mark(criterion.passed()),
                criterion.claim,
                criterion.measured,
                criterion.bound,
                criterion.unit,
                criterion.kind.as_str(),
            );
        }

        let failed = self.criteria.iter().filter(|c| !c.passed()).count();
        if failed == 0 {
            println!("\n{} criteria pass.", self.criteria.len());
        } else {
            println!("\n{failed} of {} criteria FAIL.", self.criteria.len());
        }
    }

    /// The same run as JSON, for CI to archive and compare against later commits.
    ///
    /// Hand-rolled rather than via serde: this is the only structure in the
    /// workspace that needs serialising, and a build-time dependency on a git-pinned
    /// tree is a cost we take only where it buys something (see `rnd/architecture.md`
    /// §15.1 on keeping the dependency surface small).
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        let _ = writeln!(out, "  \"nodes\": {},", self.nodes);
        let _ = writeln!(out, "  \"viewport\": [{}, {}],", self.viewport.0, self.viewport.1);
        let _ = writeln!(out, "  \"quick\": {},", self.quick);
        let _ = writeln!(out, "  \"passed\": {},", self.passed());

        out.push_str("  \"criteria\": [\n");
        for (i, c) in self.criteria.iter().enumerate() {
            let _ = write!(
                out,
                "    {{\"name\": {}, \"claim\": {}, \"kind\": {}, \"passed\": {}, \
                 \"measured\": {}, \"bound\": {}, \"unit\": {}}}",
                quote(c.name),
                quote(c.claim),
                quote(c.kind.as_str()),
                c.passed(),
                num(c.measured),
                num(c.bound),
                quote(c.unit),
            );
            out.push_str(sep(i, self.criteria.len()));
        }
        out.push_str("  ],\n");

        out.push_str("  \"scenarios\": [\n");
        for (i, s) in self.scenarios.iter().enumerate() {
            let _ = write!(
                out,
                "    {{\"name\": {}, \"frames\": {}, \"mean_ms\": {}, \"worst_ms\": {}, \
                 \"materialised\": {}, \"detail\": {}, \"child_layouts_per_frame\": {}, \
                 \"builds_per_frame\": {}, \"far_repaints_per_frame\": {}}}",
                quote(s.name),
                s.frames,
                num(s.mean_ms),
                num(s.worst_ms),
                s.materialised,
                quote(&s.detail),
                num(s.child_layouts_per_frame),
                num(s.builds_per_frame),
                num(s.far_repaints_per_frame),
            );
            out.push_str(sep(i, self.scenarios.len()));
        }
        out.push_str("  ],\n");

        out.push_str("  \"sweep\": [\n");
        for (i, p) in self.sweep.iter().enumerate() {
            let _ = write!(
                out,
                "    {{\"nodes\": {}, \"visible\": {}, \"idle_ms\": {}, \"pan_ms\": {}}}",
                p.nodes,
                p.visible,
                num(p.idle_ms),
                num(p.pan_ms),
            );
            out.push_str(sep(i, self.sweep.len()));
        }
        out.push_str("  ]\n}\n");
        out
    }
}

fn mark(ok: bool) -> &'static str {
    if ok { "pass" } else { "FAIL" }
}

fn sep(i: usize, len: usize) -> &'static str {
    if i + 1 < len { ",\n" } else { "\n" }
}

/// A JSON number, or `null` for a value that is not finite.
///
/// A division by zero frames would otherwise emit bare `NaN`, which is not JSON and
/// would make the whole report unparseable — losing the numbers that *are* valid.
fn num(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.6}")
    } else {
        "null".to_string()
    }
}

/// A JSON string literal.
///
/// Every value passed here is an ASCII identifier this crate wrote itself, but
/// escaping is three lines and means the report cannot be broken by a future name.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            },
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criterion(measured: f64, bound: f64) -> Criterion {
        Criterion {
            name: "example",
            claim: "example",
            kind: Kind::Counter,
            measured,
            bound,
            unit: "units",
        }
    }

    fn outcome(criteria: Vec<Criterion>) -> Outcome {
        Outcome {
            nodes: 5000,
            viewport: (1100, 750),
            quick: false,
            criteria,
            scenarios: Vec::new(),
            sweep: Vec::new(),
        }
    }

    /// The gate itself. Without this the benchmark can silently stop failing, and a
    /// green CI would mean "the criteria did not run" rather than "the criteria hold".
    #[test]
    fn one_failing_criterion_fails_the_run() {
        assert!(outcome(vec![criterion(1.0, 2.0)]).passed());
        assert!(!outcome(vec![criterion(1.0, 2.0), criterion(3.0, 2.0)]).passed());
    }

    /// A criterion is `measured < bound`, so sitting exactly on the bound fails.
    /// Stated as a test because the other reading is equally plausible from the
    /// field names alone.
    #[test]
    fn the_bound_is_exclusive() {
        assert!(!criterion(2.0, 2.0).passed());
        assert!(criterion(1.999, 2.0).passed());
    }

    /// An empty criteria list must not read as success: it is what a benchmark that
    /// measured nothing would produce.
    #[test]
    fn a_run_that_measured_nothing_is_not_a_pass() {
        assert!(!outcome(Vec::new()).passed());
    }

    /// Divisions by a zero frame count would otherwise put bare `NaN` in the report,
    /// which is not JSON — losing every number in the file, not just that one.
    #[test]
    fn non_finite_measurements_stay_valid_json() {
        let json = outcome(vec![criterion(f64::NAN, f64::INFINITY)]).to_json();
        assert!(json.contains("\"measured\": null"), "{json}");
        assert!(json.contains("\"bound\": null"), "{json}");
        assert!(!json.contains("NaN") && !json.contains("inf"), "{json}");
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(quote("a \"b\" \\c"), "\"a \\\"b\\\" \\\\c\"");
        let control = format!("a{}b", char::from_u32(1).unwrap());
        assert_eq!(quote(&control), "\"a\\u0001b\"");
    }
}
