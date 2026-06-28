//! `coverage_common.instrumented_files_info(...)` returns an
//! `InstrumentedFilesInfo` provider instance that survives the
//! providers-list type check.
//!
//! Without registering the namespace, `rules_shell`'s `sh_library`
//! impl errors with `Variable 'coverage_common' not found` — and that
//! impl is reached transitively from `oci_load` in the rules_oci
//! probe.

use std::fs;

use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

#[test]
fn instrumented_files_info_lands_in_providers_list() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("defs.bzl"),
        r#"
def _impl(ctx):
    instrumented = coverage_common.instrumented_files_info(
        ctx,
        source_attributes = ["srcs"],
        dependency_attributes = ["deps", "data"],
    )
    return [DefaultInfo(), instrumented]

shell_like = rule(
    implementation = _impl,
    attrs = {"srcs": attr.label_list(allow_files = True)},
)
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("BUILD.bazel"),
        r#"load(":defs.bzl", "shell_like")
shell_like(name = "world")"#,
    )
    .unwrap();

    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&Label::new(
            CanonicalRepo::main(),
            PackagePath::root(),
            "world",
        ))
        .unwrap();

    let names: Vec<&str> = analyzed
        .providers
        .iter()
        .map(|p| p.id.symbol.as_str())
        .collect();
    assert!(
        names.contains(&"InstrumentedFilesInfo"),
        "expected InstrumentedFilesInfo in providers, got {names:?}"
    );
    assert!(
        names.contains(&"DefaultInfo"),
        "expected DefaultInfo in providers, got {names:?}"
    );
}

/// `testing.TestEnvironment(environment, inherited_environment=[])` is a
/// deprecated Bazel alias that constructs a `RunEnvironmentInfo`. Test
/// rules across rulesets still call it, so it must resolve and produce
/// the provider.
#[test]
fn testing_test_environment_builds_run_environment_info() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("defs.bzl"),
        r#"
def _impl(ctx):
    env = testing.TestEnvironment(
        environment = {"KEY": "val"},
        inherited_environment = ["PATH"],
    )
    if env.environment["KEY"] != "val":
        fail("environment not preserved: %s" % env.environment)
    if env.inherited_environment != ["PATH"]:
        fail("inherited_environment not preserved: %s" % env.inherited_environment)
    return [DefaultInfo(), env]

probe = rule(implementation = _impl)
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("BUILD.bazel"),
        r#"load(":defs.bzl", "probe")
probe(name = "world")"#,
    )
    .unwrap();

    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&Label::new(
            CanonicalRepo::main(),
            PackagePath::root(),
            "world",
        ))
        .unwrap();

    let names: Vec<&str> = analyzed
        .providers
        .iter()
        .map(|p| p.id.symbol.as_str())
        .collect();
    assert!(
        names.contains(&"RunEnvironmentInfo"),
        "expected RunEnvironmentInfo from testing.TestEnvironment, got {names:?}"
    );
}
