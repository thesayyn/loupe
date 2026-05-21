//! Integration tests for `RuleClass` invoked directly from a macro
//! (i.e. `mymacro = macro(implementation = _impl); _impl(...)` calls
//! `mymacro_rule(...)`). rules_oci uses this shape pervasively.

use std::fs;

use loupe_core::analysis::CapturedProvider;
use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

fn field_str(p: &CapturedProvider, name: &str) -> String {
    p.fields
        .get(name)
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string()
}

#[test]
fn macro_calling_rule_class_records_target() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("defs.bzl"),
        r#"
TagInfo = provider(fields = ["tag"])

def _impl(ctx):
    return [TagInfo(tag = ctx.attr.tag)]

stamp_rule = rule(implementation = _impl, attrs = {"tag": attr.string()})

# A macro that wraps the rule class with extra defaults. Bazel evaluates
# this by running the def at BUILD-eval time; the `stamp_rule(...)`
# invocation is a bare rule-class call that needs to record a target.
def stamp(name, tag = "default"):
    stamp_rule(name = name, tag = tag)
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("BUILD.bazel"),
        r#"load(":defs.bzl", "stamp")
stamp(name = "world", tag = "from-macro")"#,
    )
    .unwrap();

    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "TagInfo")
        .expect("TagInfo missing");
    assert_eq!(field_str(p, "tag"), "from-macro");
}

#[test]
fn rule_class_invoked_directly_in_build_uses_stamped_kind() {
    // BUILD loads the rule-class (no Rule wrap because the alias would
    // do that — direct symbol load skips it for the hello_provider
    // happy path is wrapped, but we want the bare-call path. Trigger
    // it through a macro that re-exports under a different symbol.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("defs.bzl"),
        r#"
KindInfo = provider(fields = ["k"])

def _impl(ctx):
    return [KindInfo(k = ctx.attr.k)]

inner_rule = rule(implementation = _impl, attrs = {"k": attr.string()})

def call_inner(name, k):
    inner_rule(name = name, k = k)
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("BUILD.bazel"),
        r#"load(":defs.bzl", "call_inner")
call_inner(name = "world", k = "stamped")"#,
    )
    .unwrap();

    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "KindInfo")
        .unwrap();
    assert_eq!(field_str(p, "k"), "stamped");
}
