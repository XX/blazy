//! The Phase 0 measurements, and the criteria they decide.
//!
//! ```text
//! cargo make bench                             # every scenario
//! cargo make bench -- --quick                  # only what the criteria need
//! cargo make bench -- --nodes 20000
//! cargo make bench-ci                          # + JSON report for CI to archive
//! ```
//!
//! Exits non-zero if a criterion fails, which is what makes `rnd/architecture.md`
//! §20.5 a gate rather than a paragraph someone has to remember to re-read. What is
//! gated and what is merely reported is argued in [`node_canvas::criteria`].
//!
//! `harness = false`: the criteria are counter thresholds with a pass/fail verdict,
//! not a sampled timing distribution, so libtest's harness has nothing to offer and
//! its `#[bench]` attribute is nightly-only besides. Criterion is the wrong shape
//! for a different reason — it chooses the iteration count itself, and these
//! scenarios are stateful, so letting it pan ten thousand times would walk the
//! viewport clean off the graph and quietly measure something else.

use std::path::Path;
use std::process::ExitCode;
use std::{env, fs};

use node_canvas::{DEFAULT_NODES, flag_value, node_count};

use self::bench::Options;

mod bench;

fn main() -> ExitCode {
    // Cargo passes `--bench` to a `harness = false` target, along with anything
    // after `--`. Ignored here: this target has nothing else to be.
    let args: Vec<String> = env::args().skip(1).filter(|a| a != "--bench").collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "Phase 0 measurements\n\n\
             USAGE:\n    \
             cargo bench -p node-canvas -- [--quick] [--report FILE] [--nodes N]\n\n\
             OPTIONS:\n    \
             --quick          run only the scenarios the criteria are decided on;\n                     \
             same verdict, fewer numbers\n    \
             --report FILE    also write the run as JSON to FILE; a relative path is\n                     \
             resolved against this package, not the workspace, because that\n                     \
             is the working directory cargo gives a bench target\n    \
             --nodes N        number of nodes (default {DEFAULT_NODES})\n\n\
             Exits non-zero if a Phase 0 criterion fails.\n"
        );
        return ExitCode::SUCCESS;
    }

    let outcome = bench::run(&Options {
        count: node_count(&args),
        quick: args.iter().any(|a| a == "--quick"),
    });

    // Written after the report is printed, so a failure to write cannot cost us the
    // numbers — the console output is the primary record and the file is the archive.
    if let Some(path) = flag_value(&args, "--report") {
        match write_report(path, &outcome.to_json()) {
            Ok(()) => println!("report written to {path}"),
            // Worth failing the run over: a CI job asked for an artifact and did not
            // get one, and a silently missing report is discovered weeks later when
            // someone goes looking for the history.
            Err(err) => {
                eprintln!("could not write report to {path}: {err}");
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
fn write_report(path: &str, json: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json)
}
