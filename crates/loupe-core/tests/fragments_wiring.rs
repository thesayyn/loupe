//! End-to-end tests for `rule(fragments=[…])` wiring.
//!
//! `ctx.fragments.<name>` and `ctx.configuration` are built natively in
//! Rust off the analyzed target's `Configuration`
//! (`starlark_types/configuration.rs`). Declaring `fragments=` on a rule
//! records the fragment names on the rule class; the engine threads them
//! into the native `ctx` (`CtxGen::fragment_names`) and synthesises each
//! `ctx.fragments.<name>` view directly — there is no
//! `@bazel_tools//fragments` target to analyse and no `FragmentInfo`
//! provider to read back.

use std::fs;

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

#[test]
fn declared_fragment_resolves_natively() {
    // `fragments = ["java"]` → `ctx.fragments.java` is a native fragment
    // view. Reading `use_ijars()` off it (Bazel's `--use_ijars` default,
    // `True`) proves the name was threaded onto the rule and the view
    // built natively. `ctx.configuration.is_tool_configuration()` is
    // `False` in the default (target) configuration.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ProbeInfo = provider(fields = ["use_ijars", "is_tool"])

def _impl(ctx):
    return [ProbeInfo(
        use_ijars = ctx.fragments.java.use_ijars(),
        is_tool = ctx.configuration.is_tool_configuration(),
    )]

probe = rule(
    implementation = _impl,
    fragments = ["java"],
)
"#,
        r#"load(":defs.bzl", "probe")
probe(name = "world")"#,
    );
    let analyzed = match Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
    {
        Ok(a) => a,
        Err(e) => panic!("analysis failed: {e:#}"),
    };
    let probe = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ProbeInfo")
        .expect("ProbeInfo captured");
    assert!(
        probe
            .fields
            .get("use_ijars")
            .unwrap()
            .to_value()
            .unpack_bool()
            .unwrap(),
        "ctx.fragments.java.use_ijars() should resolve to True (Bazel default)"
    );
    assert!(
        !probe
            .fields
            .get("is_tool")
            .unwrap()
            .to_value()
            .unpack_bool()
            .unwrap(),
        "default (target) configuration is not a tool configuration"
    );
}

#[test]
fn fragments_unbound_when_not_declared() {
    // A rule that declares no `fragments` must not expose `ctx.fragments`
    // — `hasattr` is False, matching Bazel. `ctx.configuration` is still
    // available (Bazel exposes it to every rule), so reading
    // `host_path_separator` off the native view works.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ProbeInfo = provider(fields = ["has_fragments", "sep"])

def _impl(ctx):
    return [ProbeInfo(
        has_fragments = hasattr(ctx, "fragments"),
        sep = ctx.configuration.host_path_separator,
    )]

probe = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "probe")
probe(name = "world")"#,
    );
    let analyzed = Engine::new(tmp.path().to_path_buf(), Configuration::default())
        .unwrap()
        .analyze_target(&label("world"))
        .unwrap();
    let probe = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ProbeInfo")
        .expect("ProbeInfo captured");
    assert!(
        !probe
            .fields
            .get("has_fragments")
            .unwrap()
            .to_value()
            .unpack_bool()
            .unwrap(),
        "ctx.fragments must be unbound when no fragments are declared"
    );
    assert_eq!(
        probe.fields.get("sep").unwrap().to_value().unpack_str(),
        Some(if cfg!(windows) { ";" } else { ":" }),
        "ctx.configuration.host_path_separator is the host OS separator"
    );
}
