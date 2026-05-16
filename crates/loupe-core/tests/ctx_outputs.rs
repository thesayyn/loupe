//! `ctx.outputs.<name>` for `attr.output(...)` / `attr.output_list(...)`.
//!
//! Three cases:
//!
//! - Output attr left unset: surfaces as `None` so the canonical
//!   `ctx.outputs.<name> or ctx.actions.declare_file(...)` fallback in
//!   real rulesets (e.g. aspect_bazel_lib's `mtree_spec`) goes through
//!   to `declare_file`.
//! - Output attr supplied: surfaces as a `File` whose `path` lands at
//!   `bazel-out/<config>/bin/<package>/<name>`.
//! - `output_list` supplied: surfaces as a `list[File]`, one per name.

use std::fs;

use loupe_labels::{CanonicalRepo, Label, PackagePath};
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

fn engine(dir: &std::path::Path) -> Engine {
    Engine::new(dir.to_path_buf(), Configuration::default()).unwrap()
}

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

#[test]
fn ctx_outputs_unsupplied_falls_through_to_declare_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
PathInfo = provider(fields = ["path"])

def _impl(ctx):
    out = ctx.outputs.out or ctx.actions.declare_file(ctx.attr.name + ".spec")
    ctx.actions.write(out, "ok\n")
    return [PathInfo(path = out.path)]

specced = rule(
    implementation = _impl,
    attrs = {"out": attr.output()},
)
"#,
        r#"load(":defs.bzl", "specced")
specced(name = "world")"#,
    );
    let analyzed = engine(tmp.path()).analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "PathInfo")
        .expect("PathInfo missing");
    let s = p
        .fields
        .get("path")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert!(
        s.ends_with("/world.spec"),
        "expected declare_file fallback path ending in /world.spec, got: {s}"
    );
}

#[test]
fn ctx_outputs_supplied_returns_file_in_target_package() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
PathInfo = provider(fields = ["path", "short"])

def _impl(ctx):
    out = ctx.outputs.out
    ctx.actions.write(out, "ok\n")
    return [PathInfo(path = out.path, short = out.short_path)]

specced = rule(
    implementation = _impl,
    attrs = {"out": attr.output()},
)
"#,
        r#"load(":defs.bzl", "specced")
specced(name = "world", out = "explicit.spec")"#,
    );
    let analyzed = engine(tmp.path()).analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "PathInfo")
        .expect("PathInfo missing");
    let path = p
        .fields
        .get("path")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    let short = p
        .fields
        .get("short")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(short, "explicit.spec");
    assert!(
        path.starts_with("bazel-out/") && path.ends_with("/bin/explicit.spec"),
        "expected bazel-out/<cfg>/bin/explicit.spec, got: {path}"
    );
}

#[test]
fn ctx_outputs_list_yields_one_file_per_supplied_name() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
PathsInfo = provider(fields = ["paths"])

def _impl(ctx):
    return [PathsInfo(paths = ",".join([f.short_path for f in ctx.outputs.outs]))]

multi = rule(
    implementation = _impl,
    attrs = {"outs": attr.output_list()},
)
"#,
        r#"load(":defs.bzl", "multi")
multi(name = "world", outs = ["a.txt", "b.txt"])"#,
    );
    let analyzed = engine(tmp.path()).analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "PathsInfo")
        .expect("PathsInfo missing");
    let s = p
        .fields
        .get("paths")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(s, "a.txt,b.txt");
}

#[test]
fn ctx_outputs_list_default_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
CountInfo = provider(fields = ["n"])

def _impl(ctx):
    return [CountInfo(n = len(ctx.outputs.outs))]

multi = rule(
    implementation = _impl,
    attrs = {"outs": attr.output_list()},
)
"#,
        r#"load(":defs.bzl", "multi")
multi(name = "world")"#,
    );
    let analyzed = engine(tmp.path()).analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "CountInfo")
        .expect("CountInfo missing");
    let n = p.fields.get("n").unwrap().to_value().unpack_i32().unwrap();
    assert_eq!(n, 0);
}
