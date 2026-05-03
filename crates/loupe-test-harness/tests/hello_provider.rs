//! End-to-end harness test for fixture #1.
//!
//! Skips (with a printed notice) when `bazel` is unavailable. The
//! Phase 1 expectation is that Loupe produces nothing while Bazel
//! produces a real provider list — i.e. the harness reports a
//! divergence.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use loupe_test_harness::{Fixture, bazel, diff, loupe};

const PROVIDERS_EXPR: &str = "[str(p) for p in providers(target).keys()]";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("hello_provider")
}

fn bazel_available() -> bool {
    Command::new("bazel")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn fixture1_loupe_diverges_from_bazel() {
    if !bazel_available() {
        eprintln!("skipping: `bazel` not on PATH");
        return;
    }
    let fx = Fixture::at(fixture_path());
    let bazel_out = bazel::cquery(&fx, "//:world", Some(PROVIDERS_EXPR))
        .expect("bazel cquery should succeed on fixture #1");
    let loupe_out =
        loupe::cquery(&fx, "//:world", Some(PROVIDERS_EXPR)).expect("loupe stub never fails");
    let report = diff::compare(&bazel_out, &loupe_out);
    eprintln!("{report}");
    assert!(
        !bazel_out.trim().is_empty(),
        "bazel cquery returned empty output"
    );
    assert!(
        bazel_out.contains("//:defs.bzl%HelloInfo"),
        "bazel cquery should expose the user's HelloInfo provider; got: {bazel_out}"
    );
    assert!(
        report.is_divergent(),
        "Phase 1: Loupe stub should diverge from Bazel"
    );
}
