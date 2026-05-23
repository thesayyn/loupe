//! Source-file POSIX `+x` flows through DICE via `FileMetadataKey`.
//!
//! End-to-end: a source file at `<workspace>/<name>` is `chmod +x`'d,
//! the engine auto-discovers it via a label reference, and the
//! synthetic `File` carries `is_executable == true`. The other half:
//! a non-executable source file reports `false`.
//!
//! This is the source-file side of the `is_executable` plumbing PR
//! (#90 added the `File` field + the explicit `write(is_executable=…)`
//! flow; this test exercises the DICE-routed disk-read closing the
//! source-file STUB).

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;

use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

fn write_workspace(dir: &std::path::Path, defs_bzl: &str, build: &str) {
    fs::write(
        dir.join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(dir.join("defs.bzl"), defs_bzl).unwrap();
    fs::write(dir.join("BUILD.bazel"), build).unwrap();
}

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

#[test]
fn executable_source_file_reports_is_executable_true() {
    let tmp = tempfile::tempdir().unwrap();
    // Drop a real `+x` script on disk; the engine's auto-discovery
    // path stats it via `FileMetadataKey`.
    fs::write(tmp.path().join("tool.sh"), "#!/bin/sh\necho hi\n").unwrap();
    let mut perms = fs::metadata(tmp.path().join("tool.sh"))
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(tmp.path().join("tool.sh"), perms).unwrap();

    // Trivial workspace; the source-file label is referenced by an
    // attr.label_list so analysis flows through the auto-discovery
    // path (`compute_analysis`'s `pkg.targets.get(...)` miss branch
    // synthesises a source-file target).
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return []
consumer = rule(implementation = _impl, attrs = {"srcs": attr.label_list(allow_files = True)})
"#,
        r#"load(":defs.bzl", "consumer")
consumer(name = "c", srcs = ["tool.sh"])"#,
    );

    let engine = Engine::new(tmp.path().to_path_buf(), Configuration::default()).unwrap();
    let analyzed = engine.analyze_target(&label("tool.sh")).unwrap();
    let f = analyzed.default_files().into_iter().next().unwrap();
    assert!(
        f.is_executable(),
        "FileMetadataKey should have read mode 0o755 → is_executable=true; got {f:?}"
    );
}

#[test]
fn non_executable_source_file_reports_is_executable_false() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("data.txt"), "payload\n").unwrap();
    // Default file mode is 0o644 — no +x bit. The engine should stat
    // and report `false`.
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return []
consumer = rule(implementation = _impl, attrs = {"srcs": attr.label_list(allow_files = True)})
"#,
        r#"load(":defs.bzl", "consumer")
consumer(name = "c", srcs = ["data.txt"])"#,
    );

    let engine = Engine::new(tmp.path().to_path_buf(), Configuration::default()).unwrap();
    let analyzed = engine.analyze_target(&label("data.txt")).unwrap();
    let f = analyzed.default_files().into_iter().next().unwrap();
    assert!(
        !f.is_executable(),
        "0o644 source must report is_executable=false; got {f:?}"
    );
}

#[test]
fn missing_source_file_reports_is_executable_false() {
    // Auto-discovered labels regularly point at paths that aren't on
    // disk (the `pkg.targets.get(...)` miss path doesn't verify
    // existence). `FileMetadataKey` returns `FileMetadata::MISSING`
    // → `is_executable=false`. No analysis-poisoning error.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return []
consumer = rule(implementation = _impl, attrs = {"srcs": attr.label_list(allow_files = True)})
"#,
        r#"load(":defs.bzl", "consumer")
consumer(name = "c", srcs = ["ghost.txt"])"#,
    );

    let engine = Engine::new(tmp.path().to_path_buf(), Configuration::default()).unwrap();
    let analyzed = engine.analyze_target(&label("ghost.txt")).unwrap();
    let f = analyzed.default_files().into_iter().next().unwrap();
    assert!(
        !f.is_executable(),
        "missing source files must report is_executable=false"
    );
}
