//! The Phase 0 measurements, and the criteria they decide.
//!
//! ```text
//! cargo make bench                             # every scenario
//! cargo make bench --quick                  # only what the criteria need
//! cargo make bench --nodes 20000
//! cargo make bench-report                          # + JSON report for CI to archive
//! ```
//!
//! Exits non-zero if a criterion fails, which is what makes `rnd/architecture.md`
//! §20.5 a gate rather than a paragraph someone has to remember to re-read. What is
//! gated and what is merely reported is argued in [`bench_utils::criteria`].
//!
//! `harness = false`: the criteria are counter thresholds with a pass/fail verdict,
//! not a sampled timing distribution, so libtest's harness has nothing to offer and
//! its `#[bench]` attribute is nightly-only besides. Criterion is the wrong shape
//! for a different reason — it chooses the iteration count itself, and these
//! scenarios are stateful, so letting it pan ten thousand times would walk the
//! viewport clean off the graph and quietly measure something else.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{fs, io};

use clap::Parser;
use node_canvas::DEFAULT_NODES;

use self::bench::Options;

mod bench;

#[derive(Parser)]
#[command(
    // Cargo runs this target through `cargo bench`, so that is the invocation the
    // usage line should show. `bin_name`, not `name`: the usage line is built from
    // the former, and the latter would leave the hashed path under `target/` there.
    name = "phase0",
    bin_name = "cargo bench -p node-canvas --",
    about = "Phase 0 measurements. Exits non-zero if a criterion fails."
)]
struct Args {
    /// Run only the scenarios the criteria are decided on: same verdict, fewer numbers.
    #[arg(long)]
    quick: bool,

    /// Also write the run as JSON to FILE.
    ///
    /// A relative path is resolved against this package rather than the workspace,
    /// because that is the working directory cargo gives a bench target.
    #[arg(long, value_name = "FILE")]
    report: Option<PathBuf>,

    /// Number of nodes in the generated graph.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_NODES)]
    nodes: usize,

    /// Cargo appends `--bench` to every bench target's argv. Declared so clap accepts
    /// it instead of rejecting the run; this target has nothing else it could be.
    #[arg(long, hide = true)]
    bench: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();

    let outcome = bench::run(&Options {
        count: args.nodes,
        quick: args.quick,
    });

    // Written after the report is printed, so a failure to write cannot cost us the
    // numbers — the console output is the primary record and the file is the archive.
    if let Some(path) = &args.report {
        match write_report(path, &outcome.to_json()) {
            Ok(()) => println!("report written to {}", path.display()),
            // Worth failing the run over: a CI job asked for an artifact and did not
            // get one, and a silently missing report is discovered weeks later when
            // someone goes looking for the history.
            Err(err) => {
                eprintln!("could not write report to {}: {err}", path.display());
                return ExitCode::FAILURE;
            },
        }
    }

    if outcome.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Writes the report, creating the directory it goes in.
///
/// The directory usually exists — cargo-make points `--report` into `target/` — but
/// not when someone asks for a path of their own, and losing a run's numbers to a
/// missing directory would be a silly way to spend a minute.
fn write_report(path: &Path, json: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json)
}
