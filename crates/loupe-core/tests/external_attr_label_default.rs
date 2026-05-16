//! `attr.label(default = "@external_apparent//...")` defaults declared
//! in an external .bzl must resolve through the .bzl's own repo
//! mapping, not the consumer BUILD's. Without this, defaults like
//! rules_oci's `attr.label(default = "@oci_crane_toolchains//:current_toolchain")`
//! fall through to `Label::resolve`'s `<apparent>+` fallback — which on
//! Bazel 7's `~`-separator workspaces points at a non-existent
//! canonical and the package walk fails with `locating BUILD file in
//! /tmp/.../oci_crane_toolchains+/`.

use std::fs;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::{CanonicalRepo, Label, PackagePath};

#[test]
fn external_bzl_label_default_resolves_through_defining_bzl_repo_mapping() {
    let tmp = tempfile::tempdir().unwrap();

    // Main module: depends on `external_lib` and uses its rule. The
    // BUILD does NOT supply a value for the rule's `dep` attr, so the
    // .bzl-side default is what gets resolved.
    fs::write(
        tmp.path().join("MODULE.bazel"),
        r#"module(name = "main_repo", version = "0.0.0")
bazel_dep(name = "external_lib", version = "1.0.0", repo_name = "ext")"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("BUILD.bazel"),
        r#"load("@ext//:defs.bzl", "demo_rule")
demo_rule(name = "world")"#,
    )
    .unwrap();

    // External vendored repo: declares a rule whose `dep` attr's
    // default refers to `@dep_provider//:tool` — an apparent name only
    // visible from inside `external_lib~`'s repo mapping. The probed
    // default must resolve through that mapping.
    let ext_root = tmp.path().join("external");
    let ext_lib = ext_root.join("external_lib~");
    fs::create_dir_all(&ext_lib).unwrap();
    fs::write(
        ext_lib.join("MODULE.bazel"),
        r#"module(name = "external_lib", version = "1.0.0")
bazel_dep(name = "dep_provider", version = "1.0.0")"#,
    )
    .unwrap();
    fs::write(ext_lib.join("BUILD.bazel"), r#"# empty"#).unwrap();
    fs::write(
        ext_lib.join("defs.bzl"),
        r#"
LabelInfo = provider(fields = ["repo"])

def _impl(ctx):
    # Round-trip the resolved canonical of the default dep so the
    # test can assert on it without needing a real target on disk.
    return [LabelInfo(repo = str(ctx.attr.dep))]

demo_rule = rule(
    implementation = _impl,
    attrs = {"dep": attr.label(default = "@dep_provider//:tool")},
)
"#,
    )
    .unwrap();

    // Vendored dep_provider: a real repo on disk so the package
    // walk for the default dep doesn't fail at PackageKey time. The
    // canonical name uses `~` to match the lockfile separator.
    let dep_provider = ext_root.join("dep_provider~");
    fs::create_dir_all(&dep_provider).unwrap();
    fs::write(
        dep_provider.join("MODULE.bazel"),
        r#"module(name = "dep_provider", version = "1.0.0")"#,
    )
    .unwrap();
    fs::write(
        dep_provider.join("BUILD.bazel"),
        r#"filegroup(name = "tool", srcs = [])"#,
    )
    .unwrap();

    // Hand-write a `~`-separator lockfile so Loupe picks the right
    // separator without us shelling out to bazel.
    fs::write(
        tmp.path().join("MODULE.bazel.lock"),
        r#"{
  "lockFileVersion": 11,
  "registryFileHashes": {},
  "selectedYankedVersions": {},
  "moduleExtensions": {
    "@@external_lib~//some:ext.bzl%marker": {
      "general": {
        "bzlTransitiveDigest": "",
        "usagesDigest": "",
        "recordedFileInputs": {},
        "recordedDirentsInputs": {},
        "envVariables": {},
        "generatedRepoSpecs": {}
      }
    }
  }
}
"#,
    )
    .unwrap();

    let engine = EngineBuilder::new(tmp.path().to_path_buf(), Configuration::default())
        .external_repo_root(ext_root.clone())
        .bzlmod_lockfile_path(tmp.path().join("MODULE.bazel.lock"))
        .build()
        .unwrap();

    let analyzed = engine
        .analyze_target(&Label::new(
            CanonicalRepo::MAIN,
            PackagePath::root(),
            "world",
        ))
        .unwrap();

    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "LabelInfo")
        .expect("LabelInfo missing");
    let repo = p
        .fields
        .get("repo")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    // Pre-fix this asserted `dep_provider+` (the `<apparent>+`
    // fallback). Post-fix it must resolve through external_lib~'s
    // mapping, which knows `dep_provider` → `dep_provider~`.
    assert!(
        repo.contains("@@dep_provider~//"),
        "expected dep canonical via external_lib~'s mapping, got: {repo}"
    );
}
