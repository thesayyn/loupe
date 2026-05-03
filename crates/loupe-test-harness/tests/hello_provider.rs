//! End-to-end harness test for fixture #1.
//!
//! Skips (with a printed notice) when `bazel` is unavailable. The
//! Phase 5 expectation is convergence on the default `cquery <label>`
//! output (just the canonical label, one per line) — that's the
//! `PLAN.md` Phase 5 gate.
//!
//! `--starlark:expr=providers(target)` still diverges (the harness's
//! Loupe side returns a `STUB:` marker for `expr`); a follow-up phase
//! brings the expr eval surface up.

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
fn fixture1_default_cquery_matches_bazel() {
    if !bazel_available() {
        eprintln!("skipping: `bazel` not on PATH");
        return;
    }
    let fx = Fixture::at(fixture_path());
    let bazel_out =
        bazel::cquery(&fx, "//:world", None).expect("bazel cquery should succeed on fixture #1");
    let loupe_out = loupe::cquery(&fx, "//:world", None).expect("loupe cquery should succeed");
    let report = diff::compare(&bazel_out, &loupe_out);
    eprintln!("{report}");
    assert!(
        !report.is_divergent(),
        "Phase 5: default cquery output should converge"
    );
    assert_eq!(loupe_out.trim(), "@@//:world");
}

#[test]
fn fixture1_provider_expr_still_diverges_for_now() {
    if !bazel_available() {
        eprintln!("skipping: `bazel` not on PATH");
        return;
    }
    let fx = Fixture::at(fixture_path());
    let bazel_out = bazel::cquery(&fx, "//:world", Some(PROVIDERS_EXPR))
        .expect("bazel cquery should succeed on fixture #1");
    let loupe_out =
        loupe::cquery(&fx, "//:world", Some(PROVIDERS_EXPR)).expect("loupe stub never fails");
    assert!(
        bazel_out.contains("//:defs.bzl%HelloInfo"),
        "bazel cquery should expose HelloInfo; got: {bazel_out}"
    );
    let report = diff::compare(&bazel_out, &loupe_out);
    assert!(
        report.is_divergent(),
        "Phase 5 stop-gate scope: --starlark:expr is a STUB"
    );
    assert!(loupe_out.contains("STUB:"));
}
