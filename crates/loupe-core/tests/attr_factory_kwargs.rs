//! Integration tests for accept-and-ignore kwargs on the `attr.*`
//! factories — Bazel-compat parse boundary, no enforcement at v1.

use std::fs;
use std::path::PathBuf;

use loupe_core::analysis::CapturedProvider;
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

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

fn field_str(p: &CapturedProvider, name: &str) -> String {
    let v = p
        .fields
        .get(name)
        .unwrap_or_else(|| panic!("provider missing field `{name}`"));
    v.to_value()
        .unpack_str()
        .unwrap_or_else(|| panic!("field `{name}` is not a string"))
        .to_string()
}

#[test]
fn attr_string_values_kwarg_is_accepted() {
    // STUB: we don't enforce the constraint. Verify the rule
    // construction succeeds and the attr round-trips.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
TagInfo = provider(fields = ["v"])

def _impl(ctx):
    return [TagInfo(v = ctx.attr.format)]

mk = rule(
    implementation = _impl,
    attrs = {"format": attr.string(values = ["docker", "oci"], default = "docker")},
)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world", format = "oci")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "TagInfo")
        .unwrap();
    assert_eq!(field_str(p, "v"), "oci");
}
