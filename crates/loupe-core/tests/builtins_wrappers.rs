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
fn builtins_toplevel_namespace_is_usable_from_a_user_bzl() {
    // Vendored Bazel `builtins_bzl/*.bzl` files use the
    // `_builtins.toplevel.X` pattern instead of bare `X` to reach
    // for the host-registered primitives. Loupe re-publishes the
    // Rust globals through `_builtins.toplevel` (defined in
    // wrappers.bzl) so vendored upstream files Just Work without
    // edits. Demo: construct a provider via `_builtins.toplevel.provider`
    // and assert the result is usable.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
# Use the `_builtins.toplevel.provider` pattern Bazel's own
# builtins.bzl files use, instead of the bare `provider` global.
ViaBuiltins = _builtins.toplevel.provider(fields = ["msg"])

def _impl(ctx):
    return [_builtins.toplevel.DefaultInfo(), ViaBuiltins(msg = "ok")]

probe = _builtins.toplevel.rule(implementation = _impl)
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
        .find(|p| p.id.symbol == "ViaBuiltins")
        .expect("ViaBuiltins provider missing");
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
fn user_bzl_can_load_vendored_skylib_helpers() {
    // Loupe ships seven self-contained bazel-skylib helpers
    // (collections, dicts, paths, shell, structs, types, versions)
    // verbatim from upstream. Each is reachable from user `.bzl`s
    // through the `@_builtins//skylib/<name>.bzl` virtual repo
    // path. This test pulls a representative slice of them and
    // exercises one method from each to verify they actually work
    // end-to-end (parse, freeze, accessible struct, callable
    // function).
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
load("@_builtins//skylib/paths.bzl", "paths")
load("@_builtins//skylib/dicts.bzl", "dicts")
load("@_builtins//skylib/collections.bzl", "collections")
load("@_builtins//skylib/types.bzl", "types")
load("@_builtins//skylib/structs.bzl", "structs")

ProbeInfo = provider(fields = ["base", "merged_keys", "uniq", "is_str", "as_dict"])

def _impl(ctx):
    base = paths.basename("a/b/c.txt")
    merged = dicts.add({"a": 1, "b": 2}, {"b": 3, "c": 4})
    uniq = collections.uniq(["x", "y", "x", "z", "y"])
    is_str = types.is_string("hello")
    as_dict = structs.to_dict(struct(k = "v"))
    return [
        DefaultInfo(),
        ProbeInfo(
            base = base,
            merged_keys = sorted(merged.keys()),
            uniq = uniq,
            is_str = is_str,
            as_dict = sorted(as_dict.keys()),
        ),
    ]

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
        .expect("ProbeInfo missing");
    let str_field = |k: &str| -> String {
        p.fields
            .get(k)
            .unwrap()
            .to_value()
            .unpack_str()
            .unwrap()
            .to_string()
    };
    let bool_field =
        |k: &str| -> bool { p.fields.get(k).unwrap().to_value().unpack_bool().unwrap() };
    assert_eq!(str_field("base"), "c.txt");
    assert!(bool_field("is_str"), "types.is_string should return True");
    // merged_keys / uniq / as_dict are lists; check via to_value().to_string()
    let list_str = |k: &str| -> String { p.fields.get(k).unwrap().to_value().to_string() };
    assert_eq!(list_str("merged_keys"), r#"["a", "b", "c"]"#);
    assert_eq!(list_str("uniq"), r#"["x", "y", "z"]"#);
    assert_eq!(list_str("as_dict"), r#"["k"]"#);
}

#[test]
fn user_bzl_can_load_from_at_builtins_virtual_repo() {
    // Vendoring Bazel's actual `builtins_bzl/` tree means user-
    // facing `.bzl`s sometimes `load("@_builtins//common/foo.bzl",
    // "x")` to pick up things that aren't auto-injected. Loupe's
    // load resolver short-circuits for the `@_builtins` apparent
    // name, serving from the embedded tree directly. Demo: pull the
    // vendored bazel-skylib paths struct via the virtual repo and
    // call `paths.basename`.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
load("@_builtins//skylib/paths.bzl", "paths")

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
