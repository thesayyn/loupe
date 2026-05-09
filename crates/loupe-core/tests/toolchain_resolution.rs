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
fn ctx_toolchains_raises_when_unregistered() {
    // The rule declares `toolchains = [tt]`, but nothing registers
    // a matching `toolchain(...)`. Lookup raises (matches Bazel).
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
    return [ctx.toolchains["//:no_such_tc_type"]]

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
    let err = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .expect_err("missing toolchain should raise");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no resolved toolchain"),
        "expected toolchain-miss error, got: {msg}"
    );
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

#[test]
fn register_toolchains_all_pattern_expands_to_every_toolchain_in_package() {
    // `register_toolchains("//toolchains:all")` is the form modern
    // rulesets use to register every toolchain in a package — needed
    // because module extensions generate one toolchain per platform
    // and the package can't enumerate them statically. The resolver
    // expands `:all` by walking the package and matching `toolchain`
    // kinds.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"
module(name = "demo", version = "0.0.0")
register_toolchains("//toolchains:all")
"#,
            ),
            (
                "defs.bzl",
                r#"
SeenInfo = provider(fields = ["brand"])

def _impl_a(ctx):
    return [platform_common.ToolchainInfo(brand = "a")]
def _impl_b(ctx):
    return [platform_common.ToolchainInfo(brand = "b")]

def _consumer_impl(ctx):
    return [SeenInfo(brand = ctx.toolchains["//toolchains:tt_b"].brand)]

tc_a = rule(implementation = _impl_a)
tc_b = rule(implementation = _impl_b)

consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//toolchains:tt_b"],
)
"#,
            ),
            (
                "toolchains/BUILD.bazel",
                r#"
load("//:defs.bzl", "tc_a", "tc_b")

toolchain_type(name = "tt_a")
toolchain_type(name = "tt_b")

tc_a(name = "tc_a_impl")
tc_b(name = "tc_b_impl")

toolchain(
    name = "tc_for_a",
    toolchain = ":tc_a_impl",
    toolchain_type = ":tt_a",
)
toolchain(
    name = "tc_for_b",
    toolchain = ":tc_b_impl",
    toolchain_type = ":tt_b",
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "consumer")
consumer(name = "world")"#,
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
    assert_eq!(field_str(p, "brand"), "b");
}

#[test]
fn transitive_register_toolchains_in_external_module_resolves() {
    // The main module doesn't `register_toolchains` itself — the
    // registration lives in `dep_a+`'s MODULE.bazel, parsed by the
    // transitive walker. The toolchain rule + impl also live in the
    // external repo; the consumer rule lives in main and imports the
    // toolchain type.
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path();
    let vendor = workspace.join("vendor");

    // External repo `dep_a+` carries the toolchain machinery.
    let dep = vendor.join("dep_a+");
    fs::create_dir_all(&dep).unwrap();
    fs::write(
        dep.join("MODULE.bazel"),
        r#"
module(name = "dep_a", version = "1.0")
register_toolchains(":my_tc")
"#,
    )
    .unwrap();
    fs::write(
        dep.join("defs.bzl"),
        r#"
def _impl_impl(ctx):
    return [platform_common.ToolchainInfo(brand = "vendored")]

tc_impl = rule(implementation = _impl_impl)
"#,
    )
    .unwrap();
    fs::write(
        dep.join("BUILD.bazel"),
        r#"
load(":defs.bzl", "tc_impl")

toolchain_type(name = "tt")
tc_impl(name = "my_tc_impl")

toolchain(
    name = "my_tc",
    toolchain = ":my_tc_impl",
    toolchain_type = ":tt",
)
"#,
    )
    .unwrap();

    // Main workspace declares the dep and uses its toolchain type.
    fs::write(
        workspace.join("MODULE.bazel"),
        r#"
module(name = "main_mod", version = "0.0.0")
bazel_dep(name = "dep_a", version = "1.0")
"#,
    )
    .unwrap();
    fs::write(
        workspace.join("defs.bzl"),
        r#"
SeenInfo = provider(fields = ["brand"])

def _consumer_impl(ctx):
    tc = ctx.toolchains["@dep_a//:tt"]
    return [SeenInfo(brand = tc.brand)]

consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["@dep_a//:tt"],
)
"#,
    )
    .unwrap();
    fs::write(
        workspace.join("BUILD.bazel"),
        r#"
load(":defs.bzl", "consumer")
consumer(name = "world")
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
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "brand"), "vendored");
}

#[test]
fn target_compatible_with_picks_platform_compatible_toolchain() {
    // Two toolchains for the same type; one declares
    // `target_compatible_with = [linux]`, the other `[mac]`. With
    // target_platform = linux, the resolver must skip the mac one
    // and pick linux even though it's registered second.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(
        CanonicalRepo::MAIN,
        loupe_core::label::PackagePath::new("platforms").unwrap(),
        "linux_x86_64",
    );
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"
module(name = "demo", version = "0.0.0")
register_toolchains("//:tc_mac", "//:tc_linux")
"#,
            ),
            (
                "defs.bzl",
                r#"
SeenInfo = provider(fields = ["os"])

def _impl_mac(ctx):
    return [platform_common.ToolchainInfo(os = "mac")]
def _impl_linux(ctx):
    return [platform_common.ToolchainInfo(os = "linux")]

def _consumer_impl(ctx):
    return [SeenInfo(os = ctx.toolchains["//:tt"].os)]

tc_mac = rule(implementation = _impl_mac)
tc_linux = rule(implementation = _impl_linux)
consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//:tt"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "tc_mac", "tc_linux", "consumer")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_value(name = "mac",   constraint_setting = ":os_setting")

toolchain_type(name = "tt")

tc_mac(name = "tc_mac_impl")
tc_linux(name = "tc_linux_impl")

toolchain(
    name = "tc_mac",
    toolchain = ":tc_mac_impl",
    toolchain_type = ":tt",
    target_compatible_with = ["//:mac"],
)
toolchain(
    name = "tc_linux",
    toolchain = ":tc_linux_impl",
    toolchain_type = ":tt",
    target_compatible_with = ["//:linux"],
)

consumer(name = "world")
"#,
            ),
            (
                "platforms/BUILD.bazel",
                r#"
platform(
    name = "linux_x86_64",
    constraint_values = ["//:linux"],
)
"#,
            ),
        ],
    );
    let config = Configuration::default().with_target_platform(target_platform_label);
    let engine = Engine::new(tmp.path().to_path_buf(), config).unwrap();
    let analyzed = engine.analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "os"), "linux");
}

#[test]
fn target_compatible_with_raises_when_no_candidate_matches() {
    // Single toolchain demands `:linux`, target platform claims
    // `:mac`. No compatible candidate → resolver returns empty map
    // for that type, so the lookup raises.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(
        CanonicalRepo::MAIN,
        loupe_core::label::PackagePath::root(),
        "mac_only",
    );
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"
module(name = "demo", version = "0.0.0")
register_toolchains("//:tc_linux")
"#,
            ),
            (
                "defs.bzl",
                r#"
ShapeInfo = provider(fields = ["t"])

def _impl_linux(ctx):
    return [platform_common.ToolchainInfo(os = "linux")]

def _consumer_impl(ctx):
    tc = ctx.toolchains["//:tt"]
    return [ShapeInfo(t = type(tc))]

tc_linux = rule(implementation = _impl_linux)
consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//:tt"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "tc_linux", "consumer")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_value(name = "mac",   constraint_setting = ":os_setting")

toolchain_type(name = "tt")

tc_linux(name = "tc_linux_impl")

toolchain(
    name = "tc_linux",
    toolchain = ":tc_linux_impl",
    toolchain_type = ":tt",
    target_compatible_with = ["//:linux"],
)

platform(
    name = "mac_only",
    constraint_values = ["//:mac"],
)

consumer(name = "world")
"#,
            ),
        ],
    );
    let config = Configuration::default().with_target_platform(target_platform_label);
    let engine = Engine::new(tmp.path().to_path_buf(), config).unwrap();
    let err = engine
        .analyze_target(&label("world"))
        .expect_err("incompatible toolchain should leave the lookup unresolved");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no resolved toolchain"),
        "expected toolchain-miss error, got: {msg}"
    );
}

#[test]
fn no_target_platform_keeps_first_match_wins_behaviour() {
    // Without target_platform set, even a toolchain with strict
    // `target_compatible_with` matches — the constraint is simply
    // not checked. Preserves the v1 default behaviour for callers
    // that haven't opted into platform configuration.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"
module(name = "demo", version = "0.0.0")
register_toolchains("//:tc_linux")
"#,
            ),
            (
                "defs.bzl",
                r#"
SeenInfo = provider(fields = ["os"])

def _impl_linux(ctx):
    return [platform_common.ToolchainInfo(os = "linux")]

def _consumer_impl(ctx):
    return [SeenInfo(os = ctx.toolchains["//:tt"].os)]

tc_linux = rule(implementation = _impl_linux)
consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//:tt"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "tc_linux", "consumer")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")

toolchain_type(name = "tt")

tc_linux(name = "tc_linux_impl")

toolchain(
    name = "tc_linux",
    toolchain = ":tc_linux_impl",
    toolchain_type = ":tt",
    target_compatible_with = ["//:linux"],
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
    assert_eq!(field_str(p, "os"), "linux");
}

#[test]
fn exec_compatible_with_filters_against_target_platform_stub() {
    // v1 simplification: exec platform = target platform. A toolchain
    // that declares `exec_compatible_with = [linux]` is incompatible
    // when target_platform claims `mac`, even if its
    // `target_compatible_with` would otherwise match. Two toolchains
    // for one type — one exec-incompatible, one exec-compatible —
    // and the resolver picks the latter.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(
        CanonicalRepo::MAIN,
        loupe_core::label::PackagePath::root(),
        "linux_plat",
    );
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"
module(name = "demo", version = "0.0.0")
register_toolchains("//:tc_exec_mac", "//:tc_exec_linux")
"#,
            ),
            (
                "defs.bzl",
                r#"
SeenInfo = provider(fields = ["who"])

def _impl_a(ctx):
    return [platform_common.ToolchainInfo(who = "exec_mac")]
def _impl_b(ctx):
    return [platform_common.ToolchainInfo(who = "exec_linux")]

def _consumer_impl(ctx):
    return [SeenInfo(who = ctx.toolchains["//:tt"].who)]

tc_exec_mac   = rule(implementation = _impl_a)
tc_exec_linux = rule(implementation = _impl_b)
consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//:tt"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "tc_exec_mac", "tc_exec_linux", "consumer")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_value(name = "mac",   constraint_setting = ":os_setting")

toolchain_type(name = "tt")

tc_exec_mac(name = "tc_exec_mac_impl")
tc_exec_linux(name = "tc_exec_linux_impl")

# Registered first; both target- and exec-compatible-with say "mac".
# Skipped because target_platform = linux_plat doesn't claim "mac".
toolchain(
    name = "tc_exec_mac",
    toolchain = ":tc_exec_mac_impl",
    toolchain_type = ":tt",
    exec_compatible_with = ["//:mac"],
)

# Registered second; exec_compatible_with = linux. Picked.
toolchain(
    name = "tc_exec_linux",
    toolchain = ":tc_exec_linux_impl",
    toolchain_type = ":tt",
    exec_compatible_with = ["//:linux"],
)

platform(
    name = "linux_plat",
    constraint_values = ["//:linux"],
)

consumer(name = "world")
"#,
            ),
        ],
    );
    let config = Configuration::default().with_target_platform(target_platform_label);
    let engine = Engine::new(tmp.path().to_path_buf(), config).unwrap();
    let analyzed = engine.analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "who"), "exec_linux");
}

#[test]
fn target_and_exec_compatible_with_are_both_required() {
    // Both attrs must hold simultaneously. Toolchain A satisfies
    // target_compatible_with but fails exec_compatible_with;
    // toolchain B (registered second) satisfies both. B wins.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(
        CanonicalRepo::MAIN,
        loupe_core::label::PackagePath::root(),
        "host",
    );
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
SeenInfo = provider(fields = ["who"])

def _impl_a(ctx):
    return [platform_common.ToolchainInfo(who = "a")]
def _impl_b(ctx):
    return [platform_common.ToolchainInfo(who = "b")]

def _consumer_impl(ctx):
    return [SeenInfo(who = ctx.toolchains["//:tt"].who)]

tc_a = rule(implementation = _impl_a)
tc_b = rule(implementation = _impl_b)
consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//:tt"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "tc_a", "tc_b", "consumer")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_setting(name = "fast_setting")
constraint_value(name = "fast", constraint_setting = ":fast_setting")
constraint_value(name = "slow", constraint_setting = ":fast_setting")

toolchain_type(name = "tt")

tc_a(name = "tc_a_impl")
tc_b(name = "tc_b_impl")

# A: target ok (linux), exec wrong (slow but platform claims fast).
toolchain(
    name = "tc_a",
    toolchain = ":tc_a_impl",
    toolchain_type = ":tt",
    target_compatible_with = ["//:linux"],
    exec_compatible_with   = ["//:slow"],
)
# B: both ok.
toolchain(
    name = "tc_b",
    toolchain = ":tc_b_impl",
    toolchain_type = ":tt",
    target_compatible_with = ["//:linux"],
    exec_compatible_with   = ["//:fast"],
)

platform(
    name = "host",
    constraint_values = ["//:linux", "//:fast"],
)

consumer(name = "world")
"#,
            ),
        ],
    );
    let config = Configuration::default().with_target_platform(target_platform_label);
    let engine = Engine::new(tmp.path().to_path_buf(), config).unwrap();
    let analyzed = engine.analyze_target(&label("world")).unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "who"), "b");
}
