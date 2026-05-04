//! Phase 6 fixture skeleton — `rules_oci` minimum viable workspace.
//!
//! This file pins the Bazel-side reference output: `bazel cquery
//! //:img` and `//:load` against the vendored fixture. Loupe-side
//! comparison lands in a follow-up commit; the fixture has to be real
//! and stable first.
//!
//! ## Running
//!
//! - First run fetches `rules_oci` and its transitive deps (1–2 min).
//! - Subsequent runs hit the Bazel disk cache and complete in seconds.
//! - The fixture pins Bazel 7.4.1 via `.bazelversion`. Bazel 9 trips
//!   over `aspect_bazel_lib`'s `@local_config_platform` reference.
//! - On restricted-cert hosts, set
//!   `LOUPE_BAZEL_STARTUP_ARGS="--host_jvm_args=-Djavax.net.ssl.trustStore=/etc/ssl/certs/java/cacerts"`.
//!
//! Tests skip silently when `bazel` isn't on `PATH`.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use loupe_test_harness::{Fixture, bazel};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("rules_oci_basic")
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
fn bazel_cquery_oci_image_returns_directory_output() {
    if !bazel_available() {
        eprintln!("skipping: `bazel` not on PATH");
        return;
    }
    let fx = Fixture::at(fixture_path());
    let out = bazel::cquery(
        &fx,
        "//:img",
        Some("[str(target.label), [f.path for f in target.files.to_list()]]"),
    )
    .expect("bazel cquery on rules_oci fixture (//:img)");
    // Bazel collapses oci_image's output to a single tree-artifact
    // path: `bazel-out/<config>/bin/<package>/<name>`. The config
    // segment is host-dependent (`k8-fastbuild`, `darwin_arm64-opt`,
    // …) so we assert the suffix only.
    let trimmed = out.trim();
    assert!(
        trimmed.starts_with("[\"@@//:img\","),
        "expected label prefix, got: {trimmed}"
    );
    assert!(
        trimmed.ends_with("/bin/img\"]]"),
        "expected output path to end with /bin/img, got: {trimmed}"
    );
}

#[test]
fn bazel_cquery_oci_load_returns_executable_shell_wrapper() {
    if !bazel_available() {
        eprintln!("skipping: `bazel` not on PATH");
        return;
    }
    let fx = Fixture::at(fixture_path());
    let out = bazel::cquery(
        &fx,
        "//:load",
        Some("[str(target.label), [f.path for f in target.files.to_list()]]"),
    )
    .expect("bazel cquery on rules_oci fixture (//:load)");
    let trimmed = out.trim();
    assert!(
        trimmed.starts_with("[\"@@//:load\","),
        "expected label prefix, got: {trimmed}"
    );
    assert!(
        trimmed.ends_with("/bin/load.sh\"]]"),
        "oci_load produces a load.sh shell wrapper, got: {trimmed}"
    );
}
