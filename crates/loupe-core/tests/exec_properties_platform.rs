//! Tests for `Configuration::exec_properties` (platform-side
//! `Platform.properties`) and `Action::execution_requirements`
//! (per-action override). Both paths surface REAPI-shaped data Loupe
//! had been accepting-and-ignoring before this PR.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use loupe_core::action::ActionKind;
use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

fn write_workspace(dir: &Path, defs_bzl: &str, build: &str) {
    fs::write(
        dir.join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(dir.join("defs.bzl"), defs_bzl).unwrap();
    fs::write(dir.join("BUILD.bazel"), build).unwrap();
}

fn world_label() -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), "world")
}

fn p_label() -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), "p")
}

#[test]
fn platform_exec_properties_flow_onto_per_target_configuration() {
    // A target configured against a `platform(name="p",
    // exec_properties={"OSFamily": "Linux"})` sees the platform's
    // `exec_properties` on its analyzed Configuration. Mirrors
    // Bazel's "select platform → Action.platform.properties carries
    // the platform's `exec_properties`" wiring.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return []

trivial = rule(implementation = _impl)
"#,
        r#"
load(":defs.bzl", "trivial")

platform(
    name = "p",
    exec_properties = {"OSFamily": "Linux", "container-image": "docker://alpine"},
)
trivial(name = "world")
"#,
    );

    let cfg = Configuration::default().with_target_platform(p_label());
    let engine = Engine::new(tmp.path().to_path_buf(), cfg).unwrap();
    let analyzed = engine.analyze_target(&world_label()).unwrap();

    let props = analyzed.configuration.exec_properties();
    assert_eq!(
        props.get("OSFamily").map(String::as_str),
        Some("Linux"),
        "platform's exec_properties should land on the analyzed config; got {props:?}"
    );
    assert_eq!(
        props.get("container-image").map(String::as_str),
        Some("docker://alpine")
    );
    // BTreeMap iteration is sorted — REAPI requirement, sanity-check
    // the captured order so consumers can rely on it.
    let keys: Vec<&str> = props.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["OSFamily", "container-image"]);
}

#[test]
fn engine_level_exec_properties_override_platform_attr() {
    // The engine-level `Configuration::with_exec_properties` overlay
    // wins on key collision — matches Bazel's "command-line override
    // beats platform default" precedence.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return []

trivial = rule(implementation = _impl)
"#,
        r#"
load(":defs.bzl", "trivial")

platform(
    name = "p",
    exec_properties = {"OSFamily": "Linux", "Pool": "default"},
)
trivial(name = "world")
"#,
    );

    let mut overrides = BTreeMap::new();
    overrides.insert("Pool".to_string(), "highmem".to_string());
    let cfg = Configuration::default()
        .with_target_platform(p_label())
        .with_exec_properties(overrides);
    let engine = Engine::new(tmp.path().to_path_buf(), cfg).unwrap();
    let analyzed = engine.analyze_target(&world_label()).unwrap();

    let props = analyzed.configuration.exec_properties();
    // Platform-only key survives.
    assert_eq!(props.get("OSFamily").map(String::as_str), Some("Linux"));
    // Override wins on collision.
    assert_eq!(props.get("Pool").map(String::as_str), Some("highmem"));
}

#[test]
fn no_platform_means_only_engine_level_exec_properties_surface() {
    // Without a configured target platform, the analyzed config still
    // reflects whatever the engine-level overlay holds.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return []

trivial = rule(implementation = _impl)
"#,
        r#"
load(":defs.bzl", "trivial")
trivial(name = "world")
"#,
    );

    let mut overrides = BTreeMap::new();
    overrides.insert("dockerNetwork".to_string(), "off".to_string());
    let cfg = Configuration::default().with_exec_properties(overrides);
    let engine = Engine::new(tmp.path().to_path_buf(), cfg).unwrap();
    let analyzed = engine.analyze_target(&world_label()).unwrap();

    let props = analyzed.configuration.exec_properties();
    assert_eq!(props.get("dockerNetwork").map(String::as_str), Some("off"));
    assert_eq!(props.len(), 1, "only the engine override should be set");
}

#[test]
fn action_records_execution_requirements_kwarg() {
    // `ctx.actions.run(... execution_requirements={"no-cache": ""})`
    // surfaces on the recorded Action so AC-key / aquery consumers
    // can overlay it onto the configuration's `Platform.properties`.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    tool = ctx.actions.declare_file("tool.sh")
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.run(
        executable = tool,
        outputs = [out],
        execution_requirements = {"no-cache": "", "no-remote-exec": "1"},
        mnemonic = "Frobnicate",
    )
    return []

runner = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "runner")
runner(name = "world")"#,
    );

    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&world_label())
        .unwrap();

    assert_eq!(analyzed.actions.len(), 1);
    let a = &analyzed.actions[0];
    assert!(matches!(a.kind, ActionKind::Run { .. }));
    let er = &a.execution_requirements;
    assert_eq!(er.get("no-cache").map(String::as_str), Some(""));
    assert_eq!(er.get("no-remote-exec").map(String::as_str), Some("1"));
    assert_eq!(er.len(), 2);
    // BTreeMap iteration order is sorted — REAPI requirement.
    let keys: Vec<&str> = er.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["no-cache", "no-remote-exec"]);
}

#[test]
fn action_without_execution_requirements_records_empty_map() {
    // `ctx.actions.run` without `execution_requirements` should leave
    // the recorded Action's map empty — not crash, not panic, not
    // populate with anything spurious.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    tool = ctx.actions.declare_file("tool.sh")
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.run(executable = tool, outputs = [out])
    return []

runner = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "runner")
runner(name = "world")"#,
    );

    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&world_label())
        .unwrap();
    assert_eq!(analyzed.actions.len(), 1);
    assert!(analyzed.actions[0].execution_requirements.is_empty());
}

#[test]
fn run_shell_propagates_execution_requirements_through_vendored_rewrite() {
    // The vendored `wrappers.bzl::actions_run_shell` rewrites
    // `ctx.actions.run_shell(...)` into a `ctx.actions.run(
    // executable="/bin/bash", ...)`. The wrapper must forward
    // `execution_requirements` so the per-action override survives
    // the rewrite and lands on the recorded Run action.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.run_shell(
        command = "true",
        outputs = [out],
        execution_requirements = {"local": "1"},
    )
    return []

shellrun = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "shellrun")
shellrun(name = "world")"#,
    );

    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&world_label())
        .unwrap();
    assert_eq!(analyzed.actions.len(), 1);
    let a = &analyzed.actions[0];
    let er = &a.execution_requirements;
    assert_eq!(
        er.get("local").map(String::as_str),
        Some("1"),
        "execution_requirements should survive the run_shell → run rewrite; got {er:?}"
    );
}
