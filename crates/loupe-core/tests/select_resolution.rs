//! End-to-end tests for `select()` resolution against the configured
//! platform / `config_setting`s. Wires together:
//!
//! - `select()` survives loading as `AttrValue::Selector`.
//! - `compute_analysis` pre-resolves branch keys against the
//!   configured target platform's `constraint_values`.
//! - The materialised attr feeds both `ctx.attr.<name>` and the
//!   recursive label-dep resolution for `attr.label_list` selectors.

use std::fs;
use std::path::PathBuf;

use loupe_core::analysis::CapturedProvider;
use loupe_core::label::{CanonicalRepo, Label, PackagePath};
use loupe_core::{Configuration, Engine};

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

fn engine(dir: PathBuf, config: Configuration) -> Engine {
    Engine::new(dir, config).unwrap()
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
fn select_falls_back_to_default_when_no_platform_set() {
    // Without a target platform, no branch can match — every
    // selector resolves to `//conditions:default`. Preserves
    // backwards-compat with callers that haven't opted into platform
    // configuration.
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
PickInfo = provider(fields = ["v"])

def _impl(ctx):
    return [PickInfo(v = ctx.attr.who)]

mk = rule(
    implementation = _impl,
    attrs = {"who": attr.string()},
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")

mk(
    name = "world",
    who = select({
        "//:linux": "linux_branch",
        "//conditions:default": "default_branch",
    }),
)
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf(), Configuration::default())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "PickInfo")
        .expect("PickInfo missing");
    assert_eq!(field_str(p, "v"), "default_branch");
}

#[test]
fn select_picks_branch_when_constraint_value_matches_platform() {
    // Branch keyed on `:linux`; platform's constraint_values
    // includes `:linux` → branch picked.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "linux_plat");
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
PickInfo = provider(fields = ["v"])

def _impl(ctx):
    return [PickInfo(v = ctx.attr.who)]

mk = rule(
    implementation = _impl,
    attrs = {"who": attr.string()},
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_value(name = "mac",   constraint_setting = ":os_setting")

platform(
    name = "linux_plat",
    constraint_values = ["//:linux"],
)

mk(
    name = "world",
    who = select({
        "//:mac":   "mac_branch",
        "//:linux": "linux_branch",
        "//conditions:default": "default_branch",
    }),
)
"#,
            ),
        ],
    );
    let config = Configuration::default().with_target_platform(target_platform_label);
    let analyzed = engine(tmp.path().to_path_buf(), config)
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "PickInfo")
        .expect("PickInfo missing");
    assert_eq!(field_str(p, "v"), "linux_branch");
}

#[test]
fn select_picks_config_setting_with_satisfied_constraint_values() {
    // Branch keyed on a `config_setting` whose `constraint_values`
    // are all satisfied by the platform → branch picked.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "host");
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
PickInfo = provider(fields = ["v"])

def _impl(ctx):
    return [PickInfo(v = ctx.attr.who)]

mk = rule(
    implementation = _impl,
    attrs = {"who": attr.string()},
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_setting(name = "arch_setting")
constraint_value(name = "x86_64", constraint_setting = ":arch_setting")
constraint_value(name = "arm64",  constraint_setting = ":arch_setting")

config_setting(
    name = "linux_x86",
    constraint_values = ["//:linux", "//:x86_64"],
)
config_setting(
    name = "linux_arm",
    constraint_values = ["//:linux", "//:arm64"],
)

platform(
    name = "host",
    constraint_values = ["//:linux", "//:x86_64"],
)

mk(
    name = "world",
    who = select({
        "//:linux_arm": "arm_build",
        "//:linux_x86": "x86_build",
        "//conditions:default": "default_branch",
    }),
)
"#,
            ),
        ],
    );
    let config = Configuration::default().with_target_platform(target_platform_label);
    let analyzed = engine(tmp.path().to_path_buf(), config)
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "PickInfo")
        .expect("PickInfo missing");
    assert_eq!(field_str(p, "v"), "x86_build");
}

#[test]
fn select_inside_label_list_routes_to_correct_dep() {
    // `attr.label_list(default = select(...))` — the picked branch
    // contains a label, the resolver follows it through to
    // `force_analysis` so the dep's provider is available on
    // `ctx.attr.deps[0]`.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "linux_plat");
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
LeafInfo = provider(fields = ["who"])
SeenInfo = provider(fields = ["who"])

def _leaf(ctx):
    return [LeafInfo(who = ctx.attr.label_str)]

def _consumer(ctx):
    return [SeenInfo(who = ctx.attr.deps[0][LeafInfo].who)]

leaf = rule(
    implementation = _leaf,
    attrs = {"label_str": attr.string()},
)
consumer = rule(
    implementation = _consumer,
    attrs = {"deps": attr.label_list()},
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "leaf", "consumer")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_value(name = "mac",   constraint_setting = ":os_setting")

platform(
    name = "linux_plat",
    constraint_values = ["//:linux"],
)

leaf(name = "lib_linux", label_str = "linux_lib")
leaf(name = "lib_mac",   label_str = "mac_lib")

consumer(
    name = "world",
    deps = select({
        "//:linux": [":lib_linux"],
        "//:mac":   [":lib_mac"],
        "//conditions:default": [],
    }),
)
"#,
            ),
        ],
    );
    let config = Configuration::default().with_target_platform(target_platform_label);
    let analyzed = engine(tmp.path().to_path_buf(), config)
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "who"), "linux_lib");
}

#[test]
fn select_with_no_match_and_no_default_errors_with_no_match_error() {
    // No branch matches and no `//conditions:default`; resolver
    // surfaces the `no_match_error` to the caller.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "mac_plat");
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
    return []

mk = rule(implementation = _impl, attrs = {"who": attr.string()})
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_value(name = "mac",   constraint_setting = ":os_setting")

platform(name = "mac_plat", constraint_values = ["//:mac"])

mk(
    name = "world",
    who = select(
        {"//:linux": "linux_branch"},
        no_match_error = "must run on linux",
    ),
)
"#,
            ),
        ],
    );
    let config = Configuration::default().with_target_platform(target_platform_label);
    let err = engine(tmp.path().to_path_buf(), config)
        .analyze_target(&label("world"))
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("must run on linux"),
        "expected no_match_error, got: {msg}"
    );
}
