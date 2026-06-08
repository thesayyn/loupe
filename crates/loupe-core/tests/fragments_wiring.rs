//! End-to-end tests for `rule(fragments=[…])` wiring.
//!
//! Declaring `fragments` on a rule makes the engine inject hidden
//! `__configuration` / `__fragments` label deps pointing at the
//! `@bazel_tools//fragments` targets. The native ctx
//! (`starlark_types/ctx.rs`) reads those deps back so `ctx.fragments.<name>`
//! and `ctx.configuration` resolve to real `FragmentInfo` providers — the
//! engine never reads a fragment by symbol.

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
fn declared_fragment_resolves_to_fragment_info() {
    // `fragments = ["cpp"]` → `ctx.fragments.cpp` is the `FragmentInfo`
    // produced by `@bazel_tools//fragments:cpp`. Reading a field off it
    // (here `copts`, a list defaulting to empty) proves the dep was
    // injected, analysed, and read back natively.
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
            .is_some(),
        "ctx.fragments.java.use_ijars() should resolve to a bool"
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
    // — `hasattr` is False, matching Bazel. `ctx.configuration` still
    // falls through to the native host-struct method.
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
}
