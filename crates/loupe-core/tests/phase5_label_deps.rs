//! Integration tests for label-typed attrs and recursive analysis.
//!
//! v1 pieces under test:
//!
//! - `attr.label` / `attr.label_list` — capture label values into
//!   `AttrValue::Label` / `AttrValue::LabelList` on
//!   `UnconfiguredTarget`.
//! - `compute_analysis` forces `AnalysisKey` for every label-typed
//!   attr, so dep providers are available to the consumer.
//! - `ctx.attr.<dep>` returns a `Target` (label) or `[Target]`
//!   (label_list), with `target.label` and `target[Provider]`
//!   working in the impl.

use std::fs;
use std::path::PathBuf;

use loupe_core::analysis::CapturedProvider;
use loupe_core::package::AttrValue;
use loupe_core::{CanonicalRepo, Configuration, Engine, Label, PackagePath};

/// Extract a captured provider field as a Rust `String`. Provider
/// fields are `FrozenValue`s into the target's analysis heap; this
/// is the test-side bridge to a comparable type.
fn field_str(p: &CapturedProvider, name: &str) -> String {
    let v = p
        .fields
        .get(name)
        .unwrap_or_else(|| panic!("provider missing field `{name}`"));
    v.to_value()
        .unpack_str()
        .unwrap_or_else(|| {
            panic!(
                "field `{name}` is not a string: {}",
                v.to_value().get_type()
            )
        })
        .to_string()
}

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

#[test]
fn attr_label_captures_canonical_label() {
    // The captured shape is `AttrValue::Label`, not `AttrValue::String`
    // — analysis flows assume the convert-once invariant.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return []

leaf  = rule(implementation = _impl)
mid   = rule(implementation = _impl, attrs = {"dep": attr.label()})
"#,
        r#"
load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")
"#,
    );
    let pkg = engine(tmp.path().to_path_buf())
        .query_package(&loupe_core::PackageId::new(
            CanonicalRepo::MAIN,
            PackagePath::root(),
        ))
        .unwrap();
    let world = pkg.targets.get("world").unwrap();
    match world.attrs.get("dep") {
        Some(AttrValue::Label(l)) => {
            assert_eq!(l.to_string(), "@@//:a", "label resolved to canonical form");
        }
        other => panic!("expected AttrValue::Label, got {other:?}"),
    }
}

#[test]
fn attr_label_list_captures_each_canonical_label() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return []

leaf = rule(implementation = _impl)
mid  = rule(implementation = _impl, attrs = {"deps": attr.label_list()})
"#,
        r#"
load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
leaf(name = "b")
mid(name = "world", deps = [":a", ":b"])
"#,
    );
    let pkg = engine(tmp.path().to_path_buf())
        .query_package(&loupe_core::PackageId::new(
            CanonicalRepo::MAIN,
            PackagePath::root(),
        ))
        .unwrap();
    match pkg.targets.get("world").unwrap().attrs.get("deps") {
        Some(AttrValue::LabelList(ls)) => {
            let names: Vec<String> = ls.iter().map(Label::to_string).collect();
            assert_eq!(names, vec!["@@//:a", "@@//:b"]);
        }
        other => panic!("expected AttrValue::LabelList, got {other:?}"),
    }
}

#[test]
fn label_dep_is_recursively_analyzed_and_visible_via_ctx_attr() {
    // The middle rule reads `ctx.attr.dep.label` and stuffs it into
    // a provider field. This proves: (a) AnalysisKey was forced for
    // the dep before the impl ran, (b) ctx.attr.dep is a Target
    // value (not a string), (c) `target.label` returns the dep's
    // canonical label as a string.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
DepInfo = provider(fields = ["who"])

def _leaf_impl(ctx):
    return []

def _mid_impl(ctx):
    return [DepInfo(who = ctx.attr.dep.label)]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"
load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")
"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "DepInfo")
        .expect("DepInfo provided");
    assert_eq!(field_str(p, "who"), "@@//:a");
}

#[test]
fn label_list_dep_surfaces_as_list_of_targets() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
DepsInfo = provider(fields = ["who"])

def _leaf_impl(ctx):
    return []

def _mid_impl(ctx):
    return [DepsInfo(who = ",".join([t.label for t in ctx.attr.deps]))]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"deps": attr.label_list()})
"#,
        r#"
load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
leaf(name = "b")
mid(name = "world", deps = [":a", ":b"])
"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "DepsInfo")
        .expect("DepsInfo provided");
    assert_eq!(field_str(p, "who"), "@@//:a,@@//:b");
}

#[test]
fn target_provider_lookup_via_indexing_returns_dep_fields() {
    // `target[FooInfo].field` is the canonical Bazel pattern for
    // pulling typed data off a dep. This test proves the
    // `ProviderType`-keyed indexing works end-to-end: the mid rule
    // grabs `ctx.attr.dep[LeafInfo].name` and re-emits it.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
LeafInfo = provider(fields = ["name"])
MidInfo  = provider(fields = ["echoed"])

def _leaf_impl(ctx):
    return [LeafInfo(name = ctx.attr.label_str)]

def _mid_impl(ctx):
    return [MidInfo(echoed = ctx.attr.dep[LeafInfo].name)]

leaf = rule(
    implementation = _leaf_impl,
    attrs = {"label_str": attr.string()},
)
mid  = rule(
    implementation = _mid_impl,
    attrs = {"dep": attr.label()},
)
"#,
        r#"
load(":defs.bzl", "leaf", "mid")
leaf(name = "a", label_str = "leaf-a")
mid(name = "world", dep = ":a")
"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "MidInfo")
        .expect("MidInfo provided");
    assert_eq!(
        field_str(p, "echoed"),
        "leaf-a",
        "target[Provider][\"field\"] should round-trip the dep's value"
    );
}

#[test]
fn target_provider_lookup_errors_when_provider_missing() {
    // Looking up a provider the dep doesn't supply must fail loudly
    // — silently returning `None` would mask analysis bugs.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
LeafInfo = provider(fields = ["x"])

def _leaf_impl(ctx):
    return []

def _mid_impl(ctx):
    return [ctx.attr.dep[LeafInfo]]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"
load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")
"#,
    );
    let err = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap_err();
    assert!(
        err.to_string().contains("does not provide"),
        "expected provider-missing diagnostic, got: {err}"
    );
}

#[test]
fn provider_membership_test_works() {
    // `Provider in target` is the membership variant of the same
    // identity rule.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
A = provider(fields = ["x"])
B = provider(fields = ["x"])

def _leaf_impl(ctx):
    return [A(x = "1")]

def _mid_impl(ctx):
    has_a = "yes" if A in ctx.attr.dep else "no"
    has_b = "yes" if B in ctx.attr.dep else "no"
    return [A(x = has_a + "/" + has_b)]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"
load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")
"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "A")
        .unwrap();
    assert_eq!(field_str(p, "x"), "yes/no");
}
