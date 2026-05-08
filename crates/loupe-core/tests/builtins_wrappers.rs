//! `crate::builtins::wrappers.bzl` end-to-end tests.
//!
//! Exercises the user-space Bazel-compat surface that previously
//! lived as Rust namespace registrations: `coverage_common`,
//! `platform_common`, `OutputGroupInfo`. Each test stands up a tiny
//! fixture and asserts that the symbol is reachable from a user
//! `.bzl` (or BUILD) without any `load(…)` — same shape as
//! Bazel's `builtins.zip` injection.

use std::fs;

use loupe_core::label::{CanonicalRepo, Label, PackagePath};
use loupe_core::{Configuration, Engine};

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
fn coverage_common_is_visible_without_a_load() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    info = coverage_common.instrumented_files_info(
        ctx,
        source_attributes = ["srcs"],
        dependency_attributes = ["deps"],
    )
    return [DefaultInfo(), info]

shell_like = rule(
    implementation = _impl,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)
"#,
        r#"load(":defs.bzl", "shell_like")
shell_like(name = "world")"#,
    );
    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let names: Vec<&str> = analyzed
        .providers
        .iter()
        .map(|p| p.id.symbol.as_str())
        .collect();
    assert!(
        names.contains(&"InstrumentedFilesInfo"),
        "expected InstrumentedFilesInfo via wrappers.bzl, got {names:?}"
    );
}

#[test]
fn platform_common_provider_types_are_visible_without_a_load() {
    // Rule impl reads constraint values via
    // `target[platform_common.ConstraintValueInfo]`, exactly the
    // shape rules_oci's `_maybe_wrap_launcher_for_windows` uses.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ProbeInfo = provider(fields = ["has"])

def _impl(ctx):
    # Just reach for the type — exercise that platform_common is in
    # scope as a struct of provider types.
    return [ProbeInfo(has = type(platform_common.ConstraintValueInfo) == "provider")]

probe = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "probe")
probe(name = "world")"#,
    );
    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let probe = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ProbeInfo")
        .unwrap();
    let has = probe
        .fields
        .get("has")
        .unwrap()
        .to_value()
        .unpack_bool()
        .unwrap();
    assert!(
        has,
        "platform_common.ConstraintValueInfo should be a provider type"
    );
}

#[test]
fn output_group_info_constructable_without_a_load() {
    // Rules that emit named output groups (rules_oci's `oci_load`)
    // construct OutputGroupInfo at impl time. Verify the wrappers.bzl
    // version is in scope and the result lands as a real provider in
    // the captured providers list.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    out = ctx.actions.declare_file("o.txt")
    ctx.actions.write(out, "x")
    return [
        DefaultInfo(files = depset([out])),
        OutputGroupInfo(custom = depset([out])),
    ]

emitter = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "emitter")
emitter(name = "world")"#,
    );
    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let names: Vec<&str> = analyzed
        .providers
        .iter()
        .map(|p| p.id.symbol.as_str())
        .collect();
    assert!(
        names.contains(&"OutputGroupInfo"),
        "expected OutputGroupInfo via wrappers.bzl, got {names:?}"
    );
}
