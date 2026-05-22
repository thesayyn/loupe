//! `rule(initializer = fn)` — kwargs-rewrite hook run when the rule
//! is invoked from BUILD. Vendored upstream uses this to remap
//! `config_setting(values={...})` into the underlying
//! `flag_values={...}` schema; this test exercises the same pattern
//! against a user-defined rule.

use std::fs;

use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

#[test]
fn rule_initializer_rewrites_kwargs_before_attr_validation() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("defs.bzl"),
        r#"
SeenInfo = provider(fields = ["v"])

def _impl(ctx):
    return [SeenInfo(v = ctx.attr.canonical)]

def _init(**kwargs):
    # Remap a user-friendly `nice` kwarg into the underlying
    # `canonical` attr so the rule schema stays clean.
    nice = kwargs.pop("nice", None)
    if nice != None:
        kwargs["canonical"] = "@_built/" + nice
    return kwargs

remapped = rule(
    implementation = _impl,
    attrs = {"canonical": attr.string()},
    initializer = _init,
)
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("BUILD.bazel"),
        r#"
load(":defs.bzl", "remapped")
remapped(name = "world", nice = "hello")
"#,
    )
    .unwrap();
    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    let v = p
        .fields
        .get("v")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(v, "@_built/hello");
}

#[test]
fn rule_without_initializer_passes_kwargs_through_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("defs.bzl"),
        r#"
SeenInfo = provider(fields = ["v"])

def _impl(ctx):
    return [SeenInfo(v = ctx.attr.canonical)]

plain = rule(
    implementation = _impl,
    attrs = {"canonical": attr.string()},
)
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("BUILD.bazel"),
        r#"
load(":defs.bzl", "plain")
plain(name = "world", canonical = "passthrough")
"#,
    )
    .unwrap();
    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    let v = p
        .fields
        .get("v")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(v, "passthrough");
}
