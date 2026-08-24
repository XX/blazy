//! The `blazy-shell` measurements, and the criteria they decide.
//!
//! ```text
//! cargo make bench-shell            # every scenario
//! cargo make bench-shell --quick    # only what the criteria need
//! cargo make bench-shell-report     # + JSON report for CI to archive
//! ```
//!
//! Exits non-zero if a criterion fails, which is what makes `rnd/architecture.md`
//! §26 a gate rather than a paragraph. What is gated and what is merely reported is
//! argued in [`bench_utils::criteria`]; the short version is that everything here is
//! a counter, because a host is about which piece takes a decision and not about how
//! many microseconds it takes.
//!
//! `harness = false` for the same reason as the other two gates: the verdict is a set
//! of thresholds, not a sampled distribution.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{fs, io};

use clap::Parser;

use self::bench::Options;

mod bench;

#[derive(Parser)]
#[command(
    name = "shell",
    bin_name = "cargo bench -p blazy-shell --",
    about = "blazy-shell measurements. Exits non-zero if a criterion fails."
)]
struct Args {
    /// Run only the scenarios the criteria are decided on.
    #[arg(long)]
    quick: bool,

    /// Also write the run as JSON to FILE.
    #[arg(long, value_name = "FILE")]
    report: Option<PathBuf>,

    /// Frames per scenario.
    #[arg(long, value_name = "N", default_value_t = 120)]
    frames: usize,

    /// Cargo appends `--bench` to every bench target's argv.
    #[arg(long, hide = true)]
    bench: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let outcome = bench::run(&Options {
        quick: args.quick,
        frames: args.frames,
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

fn write_report(path: &Path, json: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json)
}
