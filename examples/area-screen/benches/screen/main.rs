//! The Phase 0.5 measurements, and the criteria they decide.
//!
//! ```text
//! cargo make bench-areas                       # every scenario
//! cargo make bench-areas --quick            # only what the criteria need
//! cargo make bench-areas --areas 16
//! ```
//!
//! Exits non-zero if a criterion fails, on the same terms as the Phase 0 benchmark:
//! everything gated is a deterministic counter, the milliseconds are reported and
//! never bounded. That argument lives in [`bench_utils::criteria`].

use std::path::Path;
use std::process::ExitCode;
use std::{fs, io};

use area_screen::DEFAULT_AREAS;
use clap::Parser;
use node_canvas::DEFAULT_NODES;

use self::bench::Options;

mod bench;

#[derive(Parser)]
#[command(
    name = "screen",
    bin_name = "cargo bench -p area-screen --",
    about = "Phase 0.5 measurements. Exits non-zero if a criterion fails."
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
    report: Option<Box<Path>>,

    /// Number of areas the window is tiled into.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_AREAS)]
    areas: usize,

    /// Number of nodes in the graph every area shows.
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
        areas: args.areas,
        nodes: args.nodes,
        quick: args.quick,
    });

    if let Some(path) = &args.report {
        match write_report(path, &outcome.to_json()) {
            Ok(()) => println!("report written to {}", path.display()),
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
fn write_report(path: &Path, json: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json)
}
