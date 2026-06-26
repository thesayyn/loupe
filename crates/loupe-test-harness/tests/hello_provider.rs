//! End-to-end harness test for fixture #1.
//!
//! Skips (with a printed notice) when `bazel` is unavailable. Two
//! convergence cases (default cquery output + the
//! `[str(target.label), [f.path …]]` files probe) plus one
//! intentionally-divergent case for an expr the harness mimic
//! doesn't know how to project (`providers(target)` —
//! native-provider repr differs between Bazel and Loupe).

use std::path::PathBuf;
use std::process::{Command, Stdio};

use loupe_test_harness::{Fixture, bazel, diff, loupe};

const PROVIDERS_EXPR: &str = "[str(p) for p in providers(target).keys()]";
const FILES_EXPR: &str = "[str(target.label), [f.path for f in target.files.to_list()]]";

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
fn fixture1_files_expr_converges() {
    if !bazel_available() {
        eprintln!("skipping: `bazel` not on PATH");
        return;
    }
    let fx = Fixture::at(fixture_path());
    let bazel_out = bazel::cquery(&fx, "//:world", Some(FILES_EXPR))
        .expect("bazel cquery should succeed on fixture #1");
    let loupe_out = loupe::cquery(&fx, "//:world", Some(FILES_EXPR))
        .expect("loupe cquery should succeed on fixture #1");
    let report = diff::compare(&bazel_out, &loupe_out);
    eprintln!("{report}");
    assert!(
        !report.is_divergent(),
        "harness mimic should match Bazel for the files-projection expr"
    );
    // The hello rule returns no files; both sides emit an empty list.
    assert_eq!(loupe_out.trim(), "[\"@@//:world\", []]");
}

// `--starlark:expr` is now a real evaluator (not a STUB): loupe binds
// `target` and runs the expression, so `providers(target)` enumerates the
// target's analyzed providers. It does NOT yet fully converge with Bazel
// for two understood reasons:
//   1. Bazel auto-attaches internal providers loupe doesn't model
//      (LicenseInfo, FileProvider, FilesToRunProvider, OutputGroupInfo).
//   2. loupe's provider-id repr keeps the `@@` canonical-repo prefix
//      (`@@//:defs.bzl%HelloInfo`) where Bazel prints the apparent form
//      (`//:defs.bzl%HelloInfo`) — the documented native/Loupe repr gap.
// This test pins that real, measurable state (loupe evaluates and surfaces
// HelloInfo) so a regression or a future convergence is visible.
#[test]
fn fixture1_provider_enumeration_surfaces_helloinfo() {
    if !bazel_available() {
        eprintln!("skipping: `bazel` not on PATH");
        return;
    }
    let fx = Fixture::at(fixture_path());
    let bazel_out = bazel::cquery(&fx, "//:world", Some(PROVIDERS_EXPR))
        .expect("bazel cquery should succeed on fixture #1");
    let loupe_out = loupe::cquery(&fx, "//:world", Some(PROVIDERS_EXPR))
        .expect("loupe cquery should evaluate the providers() expr");
    assert!(
        bazel_out.contains("//:defs.bzl%HelloInfo"),
        "bazel cquery should expose HelloInfo; got: {bazel_out}"
    );
    // The evaluator works (no STUB) and surfaces loupe's modeled providers,
    // including the user `HelloInfo`.
    assert!(
        !loupe_out.contains("STUB:"),
        "expr should evaluate, not STUB; got: {loupe_out}"
    );
    assert!(
        loupe_out.contains("//:defs.bzl%HelloInfo"),
        "loupe should enumerate HelloInfo; got: {loupe_out}"
    );
    // Still divergent on the internal-provider set + `@@` repr (see note).
    assert!(diff::compare(&bazel_out, &loupe_out).is_divergent());
}
