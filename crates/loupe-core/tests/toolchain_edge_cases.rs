//! Toolchain resolution edge cases beyond `toolchain_resolution.rs`.
//! Patterned after `ToolchainResolutionFunctionTest.java`:
//! - multiple types requested simultaneously
//! - missing optional toolchain succeeds with absent slot
//! - rule with empty toolchains=[] gets no resolution attempted
//! - alias-deref for toolchain_type labels

use std::fs;
use std::path::{Path, PathBuf};

use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (name, contents) in files {
        let full = dir.join(name);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, contents).unwrap();
    }
}

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

#[test]
fn rule_with_two_toolchain_types_resolves_both_independently() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"
module(name = "demo", version = "0.0.0")
register_toolchains("//:tc_a", "//:tc_b")
"#,
            ),
            (
                "defs.bzl",
                r#"
SeenInfo = provider(fields = ["a", "b"])

def _a_impl(ctx):
    return [platform_common.ToolchainInfo(name = "alpha")]
def _b_impl(ctx):
    return [platform_common.ToolchainInfo(name = "beta")]
def _consumer_impl(ctx):
    a = ctx.toolchains["//:tt_a"].name
    b = ctx.toolchains["//:tt_b"].name
    return [SeenInfo(a = a, b = b)]

tc_a_impl = rule(implementation = _a_impl)
tc_b_impl = rule(implementation = _b_impl)
consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//:tt_a", "//:tt_b"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "tc_a_impl", "tc_b_impl", "consumer")

toolchain_type(name = "tt_a")
toolchain_type(name = "tt_b")

tc_a_impl(name = "tc_a_impl")
tc_b_impl(name = "tc_b_impl")

toolchain(name = "tc_a", toolchain = ":tc_a_impl", toolchain_type = ":tt_a")
toolchain(name = "tc_b", toolchain = ":tc_b_impl", toolchain_type = ":tt_b")

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
        .unwrap();
    let a = p
        .fields
        .get("a")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    let b = p
        .fields
        .get("b")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(a, "alpha");
    assert_eq!(b, "beta");
}

#[test]
fn rule_with_empty_toolchains_attribute_analyzes_without_resolution() {
    // A rule that declares `toolchains=[]` should analyze normally;
    // `ctx.toolchains` is empty (membership returns False).
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
ShapeInfo = provider(fields = ["has_x"])

def _impl(ctx):
    has_x = "yes" if "//:nope" in ctx.toolchains else "no"
    return [ShapeInfo(has_x = has_x)]

mk = rule(implementation = _impl, toolchains = [])
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "mk")
mk(name = "world")
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
        .unwrap();
    assert_eq!(
        p.fields
            .get("has_x")
            .unwrap()
            .to_value()
            .unpack_str()
            .unwrap(),
        "no"
    );
}

#[test]
fn first_registered_toolchain_wins_when_multiple_match() {
    // Same toolchain_type, two registrations. Loupe's resolver picks
    // the first in declaration order. Documented in
    // `toolchain_resolution.rs` already — this test ensures the
    // invariant holds even when both candidates have no constraints.
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
consumer  = rule(implementation = _consumer_impl, toolchains = ["//:tt"])
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "tc_first", "tc_second", "consumer")
toolchain_type(name = "tt")
tc_first(name = "tc_first_impl")
tc_second(name = "tc_second_impl")
toolchain(name = "tc_first", toolchain = ":tc_first_impl", toolchain_type = "//:tt")
toolchain(name = "tc_second", toolchain = ":tc_second_impl", toolchain_type = "//:tt")
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
        .unwrap();
    assert_eq!(
        p.fields
            .get("who")
            .unwrap()
            .to_value()
            .unpack_str()
            .unwrap(),
        "first"
    );
}

#[test]
fn ctx_toolchains_subscript_with_unregistered_type_raises() {
    // Rule declares toolchains=["//:tt"] but no toolchain is
    // registered for it. Membership test (`in`) returns False;
    // bracket subscript raises.
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
def _impl(ctx):
    return [ctx.toolchains["//:tt"]]
mk = rule(implementation = _impl, toolchains = ["//:tt"])
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "mk")
toolchain_type(name = "tt")
mk(name = "world")
"#,
            ),
        ],
    );
    let err = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .expect_err("unregistered toolchain subscript should raise");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no resolved toolchain") || msg.to_lowercase().contains("toolchain"),
        "expected toolchain-miss error, got: {msg}"
    );
}
