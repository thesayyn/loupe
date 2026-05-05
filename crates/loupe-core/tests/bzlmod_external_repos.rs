//! Integration test for the external-repo `.bzl` load path.
//!
//! Sets up a tiny "vendored" bzlmod dep on disk and asserts that
//! Loupe loads `@fake_dep//:defs.bzl` through it. No real `rules_oci`
//! involved — this test pins the resolution mechanics so the Phase 6
//! fixture work has a known-good substrate.

use std::fs;

use loupe_core::analysis::CapturedProvider;
use loupe_core::label::{CanonicalRepo, Label, PackagePath};
use loupe_core::{Configuration, Engine};

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

fn field_str(p: &CapturedProvider, name: &str) -> String {
    let v = p.fields.get(name).expect("missing field");
    v.to_value().unpack_str().expect("not a string").to_string()
}

#[test]
fn external_bzl_loads_through_vendored_canonical_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path();
    let vendor = workspace.join("vendor");
    let dep_root = vendor.join("fake_dep+");

    fs::create_dir_all(&dep_root).unwrap();
    // The vendored repo: a single `defs.bzl` exporting one provider
    // plus one rule.
    fs::write(
        dep_root.join("defs.bzl"),
        r#"
HelloInfo = provider(fields = ["greeting"])

def _hello_impl(ctx):
    return [HelloInfo(greeting = ctx.attr.greeting)]

hello = rule(
    implementation = _hello_impl,
    attrs = {"greeting": attr.string(default = "hi")},
)
"#,
    )
    .unwrap();
    fs::write(dep_root.join("BUILD.bazel"), "").unwrap();

    // The main workspace pulls @fake_dep through `bazel_dep`. Loupe
    // canonicalises this to `fake_dep+`, matching Bazel 7 bzlmod.
    fs::write(
        workspace.join("MODULE.bazel"),
        r#"
module(name = "main", version = "0.0.0")
bazel_dep(name = "fake_dep", version = "1.0.0")
"#,
    )
    .unwrap();
    fs::write(
        workspace.join("BUILD.bazel"),
        r#"
load("@fake_dep//:defs.bzl", "hello")
hello(name = "world", greeting = "yo")
"#,
    )
    .unwrap();

    let engine = Engine::builder(workspace.to_path_buf(), Configuration::default())
        .external_repo_root(vendor)
        .build()
        .unwrap();

    let analyzed = engine.analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "HelloInfo")
        .expect("HelloInfo missing");
    assert_eq!(field_str(p, "greeting"), "yo");
}

#[test]
fn external_load_without_external_repo_root_errors_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path();
    fs::write(
        workspace.join("MODULE.bazel"),
        r#"
module(name = "main", version = "0.0.0")
bazel_dep(name = "missing_dep", version = "1.0.0")
"#,
    )
    .unwrap();
    fs::write(
        workspace.join("BUILD.bazel"),
        r#"
load("@missing_dep//:defs.bzl", "anything")
anything(name = "world")
"#,
    )
    .unwrap();

    let engine = Engine::new(workspace.to_path_buf(), Configuration::default()).unwrap();
    let err = engine.analyze_target(&label("world")).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing_dep+") && msg.contains("external_repo_root"),
        "expected error to point at the missing external_repo_root, got: {msg}"
    );
}

#[test]
fn lockfile_per_repo_mapping_resolves_loads_inside_external_repo() {
    // Two-hop chain: main → fake_dep+ → fake_lib+. main's MODULE.bazel
    // only declares fake_dep; fake_lib visibility comes from the
    // lockfile's REPO_MAPPING records (the form Bazel writes to
    // <output_base>/MODULE.bazel.lock).
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path();
    let vendor = workspace.join("vendor");

    fs::create_dir_all(vendor.join("fake_lib+")).unwrap();
    fs::write(vendor.join("fake_lib+").join("BUILD.bazel"), "").unwrap();
    fs::write(
        vendor.join("fake_lib+").join("util.bzl"),
        r#"
def lib_greet(name):
    return "lib:" + name
"#,
    )
    .unwrap();

    fs::create_dir_all(vendor.join("fake_dep+")).unwrap();
    fs::write(vendor.join("fake_dep+").join("BUILD.bazel"), "").unwrap();
    fs::write(
        vendor.join("fake_dep+").join("defs.bzl"),
        r#"
load("@fake_lib//:util.bzl", "lib_greet")

GreetInfo = provider(fields = ["msg"])

def _impl(ctx):
    return [GreetInfo(msg = lib_greet(ctx.attr.who))]

greeter = rule(implementation = _impl, attrs = {"who": attr.string()})
"#,
    )
    .unwrap();

    fs::write(
        workspace.join("MODULE.bazel"),
        r#"
module(name = "main", version = "0.0.0")
bazel_dep(name = "fake_dep", version = "1.0.0")
"#,
    )
    .unwrap();
    fs::write(
        workspace.join("BUILD.bazel"),
        r#"
load("@fake_dep//:defs.bzl", "greeter")
greeter(name = "world", who = "earth")
"#,
    )
    .unwrap();

    // Synthesise the lockfile records the way Bazel 7 would write
    // them to <output_base>/MODULE.bazel.lock.
    fs::write(
        workspace.join("MODULE.bazel.lock"),
        r#"{
          "lockFileVersion": 26,
          "moduleExtensions": {
            "@@fake_dep+//:noop.bzl%noop_extension": {
              "general": {
                "recordedInputs": [
                  "REPO_MAPPING:fake_dep+,fake_lib fake_lib+"
                ],
                "generatedRepoSpecs": {}
              }
            }
          }
        }"#,
    )
    .unwrap();

    let engine = Engine::builder(workspace.to_path_buf(), Configuration::default())
        .external_repo_root(vendor)
        .build()
        .unwrap();

    let analyzed = engine.analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "GreetInfo")
        .unwrap();
    assert_eq!(field_str(p, "msg"), "lib:earth");
}

#[test]
fn repo_name_override_changes_apparent_only() {
    // `bazel_dep(name = "rules_oci", repo_name = "oci")` — code says
    // `@oci//...`, on-disk it lives at `<vendor>/rules_oci+/`.
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path();
    let vendor = workspace.join("vendor");
    let dep_root = vendor.join("rules_oci+");

    fs::create_dir_all(&dep_root).unwrap();
    fs::write(
        dep_root.join("defs.bzl"),
        r#"
TagInfo = provider(fields = ["tag"])

def _impl(ctx):
    return [TagInfo(tag = ctx.attr.tag)]

stamp = rule(implementation = _impl, attrs = {"tag": attr.string()})
"#,
    )
    .unwrap();
    fs::write(dep_root.join("BUILD.bazel"), "").unwrap();

    fs::write(
        workspace.join("MODULE.bazel"),
        r#"
module(name = "main", version = "0.0.0")
bazel_dep(name = "rules_oci", version = "2.2.6", repo_name = "oci")
"#,
    )
    .unwrap();
    fs::write(
        workspace.join("BUILD.bazel"),
        r#"
load("@oci//:defs.bzl", "stamp")
stamp(name = "world", tag = "demo")
"#,
    )
    .unwrap();

    let engine = Engine::builder(workspace.to_path_buf(), Configuration::default())
        .external_repo_root(vendor)
        .build()
        .unwrap();
    let analyzed = engine.analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "TagInfo")
        .unwrap();
    assert_eq!(field_str(p, "tag"), "demo");
}
