//! Smoke test for `loupe analyze` — builds a tiny workspace with one
//! clean target, one rule kind we ask the CLI to skip, and one
//! `fail("…")` rule, then asserts the report and exit code.

use std::process::Command;

const LOUPE_BIN: &str = env!("CARGO_BIN_EXE_loupe");

fn write_fixture(dir: &std::path::Path) {
    std::fs::write(
        dir.join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("defs.bzl"),
        r#"
def _ok_impl(ctx):
    return []

def _broken_impl(ctx):
    fail("on purpose")

ok_rule = rule(implementation = _ok_impl, attrs = {})
broken_rule = rule(implementation = _broken_impl, attrs = {})
image_index = rule(implementation = _ok_impl, attrs = {})
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("BUILD.bazel"),
        r#"
load(":defs.bzl", "ok_rule", "broken_rule", "image_index")
ok_rule(name = "ok")
broken_rule(name = "broken")
image_index(name = "skipme")
"#,
    )
    .unwrap();
}

#[test]
fn analyze_reports_buckets_and_exits_non_zero_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path());

    let out = Command::new(LOUPE_BIN)
        .arg("analyze")
        .arg(dir.path())
        .arg("--skip-kind=image_index")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(stdout.contains("analyzed: 1"), "stdout was:\n{stdout}");
    assert!(stdout.contains("skipped:  1"), "stdout was:\n{stdout}");
    assert!(stdout.contains("@@//:skipme (image_index)"));
    assert!(stdout.contains("failed:   1"));
    assert!(stdout.contains("@@//:broken (broken_rule)"));
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 on failed targets; stderr was:\n{stderr}"
    );
}

#[test]
fn analyze_exits_zero_when_everything_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("defs.bzl"),
        r#"
def _ok_impl(ctx):
    return []
ok_rule = rule(implementation = _ok_impl, attrs = {})
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("BUILD.bazel"),
        r#"
load(":defs.bzl", "ok_rule")
ok_rule(name = "a")
ok_rule(name = "b")
"#,
    )
    .unwrap();

    let out = Command::new(LOUPE_BIN)
        .arg("analyze")
        .arg(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("analyzed: 2"), "stdout was:\n{stdout}");
    assert!(stdout.contains("failed:   0"));
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn analyze_help_prints_usage() {
    let out = Command::new(LOUPE_BIN).arg("--help").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE:"), "stdout was:\n{stdout}");
    assert!(stdout.contains("--skip-kind"));
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn analyze_unknown_subcommand_errors() {
    let out = Command::new(LOUPE_BIN).arg("frobnicate").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown subcommand"),
        "stderr was:\n{stderr}"
    );
}
