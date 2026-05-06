//! End-to-end tests for `ctx.toolchains[T]` real resolution.
//!
//! Covers the resolver wiring landed in PR B: a rule declares
//! `toolchains=[T]`, MODULE.bazel registers a `toolchain(...)` whose
//! `toolchain_type` matches T, and the rule impl reads
//! `ctx.toolchains[T].field` — the value flowing in is the impl
//! target's `ToolchainInfo` provider.

use std::fs;
use std::path::PathBuf;

use loupe_core::analysis::CapturedProvider;
use loupe_core::label::{CanonicalRepo, Label, PackagePath};
use loupe_core::{Configuration, Engine};

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

fn field_str(p: &CapturedProvider, name: &str) -> String {
    p.fields
        .get(name)
        .unwrap_or_else(|| panic!("provider missing field `{name}`"))
        .to_value()
        .unpack_str()
        .unwrap_or_else(|| panic!("field `{name}` is not a string"))
        .to_string()
}

fn write(dir: &std::path::Path, files: &[(&str, &str)]) {
    for (name, contents) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }
}

#[test]
fn ctx_toolchains_resolves_to_real_toolchain_info_fields() {
    // The mid rule declares `toolchains = [tt]`; MODULE.bazel
    // registers a toolchain whose impl returns
    // `ToolchainInfo(brand="acme")`. The mid rule reads the brand and
    // surfaces it through its own provider so the test can assert on
    // it.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"
module(name = "demo", version = "0.0.0")
register_toolchains("//:my_tc")
"#,
            ),
            (
                "defs.bzl",
                r#"
SeenInfo = provider(fields = ["brand"])

def _impl_impl(ctx):
    return [platform_common.ToolchainInfo(brand = "acme")]

def _consumer_impl(ctx):
    tc = ctx.toolchains["//:my_tc_type"]
    return [SeenInfo(brand = tc.brand)]

tc_impl = rule(implementation = _impl_impl)

consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//:my_tc_type"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "tc_impl", "consumer")

toolchain_type(name = "my_tc_type")

tc_impl(name = "my_tc_impl")

toolchain(
    name = "my_tc",
    toolchain = ":my_tc_impl",
    toolchain_type = "//:my_tc_type",
)

consumer(name = "world")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "brand"), "acme");
}

#[test]
fn ctx_toolchains_falls_back_to_wildcard_when_unregistered() {
    // The rule declares `toolchains = [tt]`, but nothing registers
    // a matching `toolchain(...)`. Lookup falls through to the
    // WildcardStruct STUB so chained reads still work.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"module(name = "demo", version = "0.0.0")"#,
            ),
            (
                "defs.bzl",
                r#"
ShapeInfo = provider(fields = ["t", "deeper"])

def _impl(ctx):
    tc = ctx.toolchains["//:no_such_tc_type"]
    return [ShapeInfo(t = type(tc), deeper = type(tc.any.field.depth))]

mk = rule(
    implementation = _impl,
    toolchains = ["//:no_such_tc_type"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ShapeInfo")
        .expect("ShapeInfo missing");
    assert_eq!(field_str(p, "t"), "wildcard_struct");
    assert_eq!(field_str(p, "deeper"), "wildcard_struct");
}

#[test]
fn ctx_toolchains_membership_test_with_string_key() {
    // `T in ctx.toolchains` reflects whether the type was resolved.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"
module(name = "demo", version = "0.0.0")
register_toolchains("//:registered_tc")
"#,
            ),
            (
                "defs.bzl",
                r#"
ShapeInfo = provider(fields = ["yes", "no"])

def _impl_impl(ctx):
    return [platform_common.ToolchainInfo(x = "1")]

def _consumer_impl(ctx):
    has = "yes" if "//:registered_type" in ctx.toolchains else "no"
    has_other = "yes" if "//:other_type" in ctx.toolchains else "no"
    return [ShapeInfo(yes = has, no = has_other)]

tc_impl = rule(implementation = _impl_impl)

consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//:registered_type", "//:other_type"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "tc_impl", "consumer")

toolchain_type(name = "registered_type")
toolchain_type(name = "other_type")

tc_impl(name = "tc_impl_target")

toolchain(
    name = "registered_tc",
    toolchain = ":tc_impl_target",
    toolchain_type = "//:registered_type",
)

consumer(name = "world")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ShapeInfo")
        .expect("ShapeInfo missing");
    assert_eq!(field_str(p, "yes"), "yes");
    assert_eq!(field_str(p, "no"), "no");
}

#[test]
fn first_registered_match_wins_when_multiple_register_same_type() {
    // Two toolchains register for the same type; v1's resolver picks
    // the first in `register_toolchains` declaration order. Documented
    // divergence vs Bazel's constraint-based selection.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"
module(name = "demo", version = "0.0.0")
register_toolchains("//:tc_first", "//:tc_second")
"#,
            ),
            (
                "defs.bzl",
                r#"
SeenInfo = provider(fields = ["who"])

def _first_impl(ctx):
    return [platform_common.ToolchainInfo(who = "first")]
def _second_impl(ctx):
    return [platform_common.ToolchainInfo(who = "second")]

def _consumer_impl(ctx):
    return [SeenInfo(who = ctx.toolchains["//:tt"].who)]

tc_first  = rule(implementation = _first_impl)
tc_second = rule(implementation = _second_impl)
consumer  = rule(
    implementation = _consumer_impl,
    toolchains = ["//:tt"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "tc_first", "tc_second", "consumer")

toolchain_type(name = "tt")

tc_first(name = "tc_first_impl")
tc_second(name = "tc_second_impl")

toolchain(
    name = "tc_first",
    toolchain = ":tc_first_impl",
    toolchain_type = "//:tt",
)
toolchain(
    name = "tc_second",
    toolchain = ":tc_second_impl",
    toolchain_type = "//:tt",
)

consumer(name = "world")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "who"), "first");
}
