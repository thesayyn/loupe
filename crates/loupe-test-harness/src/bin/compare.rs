//! `loupe-compare` — run the cquery comparison harness against a fixture.
//!
//! Usage: `loupe-compare <fixture-dir> <target> [--expr <starlark-expr>]`
//!
//! Exits 0 if Bazel and Loupe agree, 2 if they diverge, 1 on harness
//! error.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use loupe_test_harness::{Fixture, compare};

fn run() -> Result<bool> {
    let mut args = std::env::args().skip(1);
    let fixture = args
        .next()
        .ok_or_else(|| anyhow!("usage: loupe-compare <fixture-dir> <target> [--expr <expr>]"))?;
    let target = args.next().ok_or_else(|| anyhow!("missing <target>"))?;
    let mut expr: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--expr" => expr = args.next(),
            other => return Err(anyhow!("unknown arg: {other}")),
        }
    }

    let fx = Fixture::at(PathBuf::from(fixture));
    let report = compare(&fx, &target, expr.as_deref())?;
    print!("{report}");
    Ok(report.is_divergent())
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::from(2),
        Ok(false) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
