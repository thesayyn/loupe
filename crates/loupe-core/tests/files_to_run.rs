//! `default_info.files_to_run` — Bazel synthesises a per-target
//! launcher-metadata struct (`executable`, `runfiles_manifest`,
//! `repo_mapping_manifest`) on every `DefaultInfo`. v1 builds it on
//! demand from the user-supplied `executable`; the two manifest fields
//! stay `None` since v1 doesn't model action execution.
//!
//! The motivating case is `aspect_bazel_lib`'s `mtree_spec`, which
//! reads `default_info.files_to_run.runfiles_manifest` and guards
//! `if not <manifest>:` — None falls through correctly.

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

fn engine(dir: &std::path::Path) -> Engine {
    Engine::new(dir.to_path_buf(), Configuration::default()).unwrap()
}

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

#[test]
fn files_to_run_runfiles_manifest_is_none_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ProbeInfo = provider(fields = ["has_manifest"])

def _probe_impl(ctx):
    info = ctx.attr.dep[DefaultInfo]
    has = info.files_to_run.runfiles_manifest != None
    return [ProbeInfo(has_manifest = has)]

def _leaf_impl(ctx):
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.write(out, "ok\n")
    return [DefaultInfo(files = [out])]

leaf  = rule(implementation = _leaf_impl)
probe = rule(implementation = _probe_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "probe")
leaf(name = "a")
probe(name = "world", dep = ":a")"#,
    );
    let analyzed = engine(tmp.path()).analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ProbeInfo")
        .expect("ProbeInfo missing");
    let has = p
        .fields
        .get("has_manifest")
        .unwrap()
        .to_value()
        .unpack_bool()
        .unwrap();
    assert!(!has, "runfiles_manifest should be None for default targets");
}

#[test]
fn files_to_run_executable_mirrors_default_info_executable() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
PathInfo = provider(fields = ["path"])

def _bin_impl(ctx):
    out = ctx.actions.declare_file("tool.sh")
    ctx.actions.write(out, "shebang\n")
    return [DefaultInfo(executable = out)]

def _probe_impl(ctx):
    exe = ctx.attr.dep[DefaultInfo].files_to_run.executable
    return [PathInfo(path = exe.short_path)]

bin   = rule(implementation = _bin_impl,   executable = True)
probe = rule(implementation = _probe_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "bin", "probe")
bin(name = "tool")
probe(name = "world", dep = ":tool")"#,
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
    assert_eq!(path, "tool.sh");
}

#[test]
fn label_list_accepts_allow_rules_kwarg() {
    // rules_shell's sh_library declares
    // `attr.label_list(allow_rules = ["sh_library"])`. v1 doesn't
    // enforce kind-typing on label-typed attrs, but the kwarg has to
    // be accepted at the parse boundary.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return [DefaultInfo()]

shell_like = rule(
    implementation = _impl,
    attrs = {"deps": attr.label_list(allow_rules = ["shell_like"])},
)
"#,
        r#"load(":defs.bzl", "shell_like")
shell_like(name = "world")"#,
    );
    let analyzed = engine(tmp.path()).analyze_target(&label("world")).unwrap();
    let names: Vec<&str> = analyzed
        .providers
        .iter()
        .map(|p| p.id.symbol.as_str())
        .collect();
    assert!(names.contains(&"DefaultInfo"));
}
