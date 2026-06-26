//! `crate::builtins::wrappers.bzl` end-to-end tests.
//!
//! Exercises the user-space Bazel-compat surface that previously
//! lived as Rust namespace registrations: `coverage_common`,
//! `platform_common`, `OutputGroupInfo`. Each test stands up a tiny
//! fixture and asserts that the symbol is reachable from a user
//! `.bzl` (or BUILD) without any `load(…)` — same shape as
//! Bazel's `builtins.zip` injection.

use std::fs;

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
    Label::new(CanonicalRepo::main(), PackagePath::root(), name)
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
fn builtins_toplevel_namespace_is_usable_from_a_user_bzl() {
    // Vendored Bazel `builtins_bzl/*.bzl` files reach for
    // `_builtins.toplevel.X` to pick up host-published primitives.
    // The vendored `builtins_core/exports.bzl` defines
    // `exported_toplevels["_builtins"].toplevel` as the struct of
    // all `exported_toplevels` — so things like `DefaultInfo` and
    // `coverage_common` are reachable through that path.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ProbeInfo = provider(fields = ["msg"])

def _impl(ctx):
    info = _builtins.toplevel.coverage_common.instrumented_files_info(
        ctx,
        source_attributes = [],
        dependency_attributes = [],
    )
    return [_builtins.toplevel.DefaultInfo(), info, ProbeInfo(msg = "ok")]

probe = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "probe")
probe(name = "world")"#,
    );
    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ProbeInfo")
        .expect("ProbeInfo provider missing");
    let msg = p
        .fields
        .get("msg")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(msg, "ok");
}

#[test]
fn user_bzl_can_load_from_at_builtins_virtual_repo() {
    // Vendoring upstream's actual `builtins_bzl/` tree means user-
    // facing `.bzl`s sometimes `load("@_builtins//builtins_bzl/<...>",
    // "x")` to pick up things that aren't auto-injected. Loupe's
    // load resolver short-circuits for the `@_builtins` apparent
    // name, serving from the embedded tree directly. Demo: pull the
    // vendored `paths` struct via the virtual repo and call
    // `paths.basename`.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
load("@_builtins//builtins_bzl/common/paths.bzl", "paths")

PathInfo = provider(fields = ["base"])

def _impl(ctx):
    return [PathInfo(base = paths.basename("a/b/c.txt")), DefaultInfo()]

probe = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "probe")
probe(name = "world")"#,
    );
    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "PathInfo")
        .expect("PathInfo missing");
    let base = p
        .fields
        .get("base")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(base, "c.txt");
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

#[test]
fn platform_common_platform_info_and_toolchain_info_are_bound() {
    // Verify name resolution succeeds for platform_common.PlatformInfo
    // and platform_common.ToolchainInfo — we don't construct
    // instances. STUB at v1.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ShapeInfo = provider(fields = ["pi", "ti"])

def _impl(ctx):
    return [ShapeInfo(
        pi = type(platform_common.PlatformInfo),
        ti = type(platform_common.ToolchainInfo),
    )]

mk = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
    );
    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ShapeInfo")
        .unwrap();
    let pi = p.fields.get("pi").unwrap().to_value().unpack_str().unwrap();
    let ti = p.fields.get("ti").unwrap().to_value().unpack_str().unwrap();
    assert_eq!(pi, "provider");
    assert_eq!(ti, "provider");
}
