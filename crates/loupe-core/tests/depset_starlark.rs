//! Integration tests for the Starlark `depset` builtin and value type.
//! Covers construction, iteration, `to_list()`, all four orders, dedupe,
//! and order-incompatibility errors.

use std::fs;
use std::path::PathBuf;

use loupe_core::analysis::CapturedProvider;
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

fn run_with_csv(defs_bzl: &str) -> String {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        defs_bzl,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "OutInfo")
        .unwrap();
    field_str(p, "out")
}

#[test]
fn depset_direct_only_iterates_in_insertion_order() {
    // Smallest construct: `depset(direct=[...])`. No transitive.
    // Iteration order is just the direct list.
    let csv = run_with_csv(
        r#"
OutInfo = provider(fields = ["out"])

def _impl(ctx):
    d = depset(["a", "b", "c"])
    return [OutInfo(out = ",".join([x for x in d]))]

mk = rule(implementation = _impl)
"#,
    );
    assert_eq!(csv, "a,b,c");
}

#[test]
fn depset_to_list_works() {
    // Methods surface — to_list() should produce the same flatten as
    // iteration.
    let csv = run_with_csv(
        r#"
OutInfo = provider(fields = ["out"])

def _impl(ctx):
    d = depset(["x", "y"])
    return [OutInfo(out = ",".join(d.to_list()))]

mk = rule(implementation = _impl)
"#,
    );
    assert_eq!(csv, "x,y");
}

#[test]
fn depset_transitive_postorder_default_puts_children_first() {
    // Default order is postorder: parent's direct elements come *after*
    // each child's elements. `[a]` from the leaf appears before `[b]`
    // from the root.
    let csv = run_with_csv(
        r#"
OutInfo = provider(fields = ["out"])

def _impl(ctx):
    leaf = depset(["a"])
    root = depset(["b"], transitive = [leaf])
    return [OutInfo(out = ",".join(root.to_list()))]

mk = rule(implementation = _impl)
"#,
    );
    assert_eq!(csv, "a,b");
}

#[test]
fn depset_transitive_preorder_puts_parent_first() {
    // Preorder: parent before children. `[b]` (root) precedes `[a]`
    // (leaf).
    let csv = run_with_csv(
        r#"
OutInfo = provider(fields = ["out"])

def _impl(ctx):
    leaf = depset(["a"], order = "preorder")
    root = depset(["b"], transitive = [leaf], order = "preorder")
    return [OutInfo(out = ",".join(root.to_list()))]

mk = rule(implementation = _impl)
"#,
    );
    assert_eq!(csv, "b,a");
}

#[test]
fn depset_dedupe_keeps_first_occurrence_in_preorder() {
    // Shared element between two leaves; preorder is first-wins.
    let csv = run_with_csv(
        r#"
OutInfo = provider(fields = ["out"])

def _impl(ctx):
    a = depset(["x", "y"], order = "preorder")
    b = depset(["y", "z"], order = "preorder")
    root = depset(transitive = [a, b], order = "preorder")
    return [OutInfo(out = ",".join(root.to_list()))]

mk = rule(implementation = _impl)
"#,
    );
    // Preorder: a's elements first (x, y), then b's (y dropped, z).
    assert_eq!(csv, "x,y,z");
}

#[test]
fn depset_incompatible_orders_are_rejected() {
    // Bazel rejects mixing parents/children with incompatible orders.
    // `default` (postorder) and `preorder` are incompatible.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
OutInfo = provider(fields = ["out"])

def _impl(ctx):
    pre = depset(["x"], order = "preorder")
    # Default is postorder — not compatible with preorder children.
    bad = depset(["y"], transitive = [pre])
    return [OutInfo(out = "")]

mk = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
    );
    let err = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .expect_err("expected error from incompatible depset orders");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("incompatible depset orders"),
        "error should mention incompatible depset orders, got: {msg}",
    );
}

#[test]
fn depset_at_build_phase_is_available() {
    // Bazel exposes `depset` at BUILD time too (some macros use it).
    // Round-trip through a global so we know the registration ran.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
OutInfo = provider(fields = ["out"])

def _impl(ctx):
    return [OutInfo(out = ctx.attr.tag)]

mk = rule(
    implementation = _impl,
    attrs = {"tag": attr.string()},
)
"#,
        r#"load(":defs.bzl", "mk")
# At BUILD time: build a depset and read its element. If `depset` isn't
# registered at the BUILD phase this fails to load.
_d = depset(["build-time-ok"])
mk(name = "world", tag = _d.to_list()[0])"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "OutInfo")
        .unwrap();
    assert_eq!(field_str(p, "out"), "build-time-ok");
}

#[test]
fn depset_nested_transitive_flattens_recursively() {
    // depset(direct=[c], transitive=[depset([b], transitive=[depset([a])])])
    // Default postorder: a, b, c.
    let csv = run_with_csv(
        r#"
OutInfo = provider(fields = ["out"])

def _impl(ctx):
    leaf = depset(["a"])
    mid  = depset(["b"], transitive = [leaf])
    root = depset(["c"], transitive = [mid])
    return [OutInfo(out = ",".join(root.to_list()))]

mk = rule(implementation = _impl)
"#,
    );
    assert_eq!(csv, "a,b,c");
}
