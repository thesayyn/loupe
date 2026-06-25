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
use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

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
fn none_select_branch_coerces_to_attr_default_for_any_kind() {
    // Bazel coerces a `None` select() branch to the attribute's default
    // for *every* attr kind — not just `label_list`. A `string` attr whose
    // matched branch is `None` must coerce to the empty string, not error.
    // (Regression: the coercion previously special-cased label_list only.)
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

mk = rule(implementation = _impl, attrs = {"who": attr.string()})
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")
mk(
    name = "world",
    # No platform set → the default branch (None) is selected; it must
    # coerce to the string attr's default ("") rather than erroring.
    who = select({"//conditions:default": None}),
)
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf(), Configuration::default())
        .analyze_target(&label("world"))
        .unwrap_or_else(|e| {
            panic!("None select branch on a string attr should coerce, not error: {e:#}")
        });
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "PickInfo")
        .expect("PickInfo missing");
    assert_eq!(
        field_str(p, "v"),
        "",
        "None branch → string attr default (\"\")"
    );
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

#[test]
fn select_picks_more_specific_config_setting_over_one_constraint() {
    // Two branches both match: a single-constraint `:linux` and a
    // two-constraint `linux_x86`. The two-constraint config_setting
    // wins because it's more specific.
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

config_setting(
    name = "linux_x86",
    constraint_values = ["//:linux", "//:x86_64"],
)

platform(name = "host", constraint_values = ["//:linux", "//:x86_64"])

mk(
    name = "world",
    who = select({
        "//:linux":     "less_specific",
        "//:linux_x86": "more_specific",
        "//conditions:default": "default",
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
    assert_eq!(field_str(p, "v"), "more_specific");
}

#[test]
fn config_setting_with_values_falls_through_when_setting_default_mismatches() {
    // `values={"compilation_mode": "opt"}` translates to a
    // `flag_values` entry against
    // `@bazel_tools//command_line_option:compilation_mode`, whose
    // default is `"fastbuild"` — the value mismatch fails the branch
    // and the select falls through to default. (The constraint
    // clause holds, so this also exercises the AND-semantics
    // between constraint_values and flag_values.)
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

# `values=` is unmodelled; this config_setting can't be honoured.
config_setting(
    name = "linux_with_define",
    constraint_values = ["//:linux"],
    values = {"compilation_mode": "opt"},
)

platform(name = "host", constraint_values = ["//:linux"])

mk(
    name = "world",
    who = select({
        "//:linux_with_define": "should_not_pick",
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
    assert_eq!(field_str(p, "v"), "default_branch");
}

#[test]
fn platform_parents_chain_inherits_constraint_values() {
    // Child platform declares `parents = [:base]` and adds one more
    // constraint. A select branch keyed on a constraint_value that's
    // ONLY in the parent must still match — i.e. the resolver
    // walks the parent chain when computing the platform's effective
    // constraint set.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "child");
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
constraint_value(name = "arm64", constraint_setting = ":arch_setting")

platform(name = "base",  constraint_values = ["//:linux"])
platform(name = "child", constraint_values = ["//:arm64"], parents = [":base"])

mk(
    name = "world",
    who = select({
        "//:linux": "linux_inherited",
        "//conditions:default": "default",
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
    assert_eq!(field_str(p, "v"), "linux_inherited");
}

#[test]
fn config_setting_values_match_against_engine_build_settings() {
    // `config_setting(values = {...})` matches against the
    // engine-exported `Configuration::build_settings` map. With the
    // default `compilation_mode = "fastbuild"`, the fastbuild branch
    // should win without touching constraint_values.
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
mk = rule(implementation = _impl, attrs = {"who": attr.string()})
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")
config_setting(name = "fastbuild_cs", values = {"compilation_mode": "fastbuild"})
config_setting(name = "opt_cs",       values = {"compilation_mode": "opt"})
mk(
    name = "world",
    who = select({
        "//:fastbuild_cs": "fastbuild",
        "//:opt_cs": "opt",
        "//conditions:default": "fallthrough",
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
    assert_eq!(field_str(p, "v"), "fastbuild");
}

#[test]
fn config_setting_values_match_picks_branch_when_setting_overridden() {
    // Same fixture, but the engine ships
    // `compilation_mode = "opt"` via `with_build_setting`. The
    // opt branch wins. Proves the override path works end-to-end.
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
mk = rule(implementation = _impl, attrs = {"who": attr.string()})
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")
config_setting(name = "fastbuild_cs", values = {"compilation_mode": "fastbuild"})
config_setting(name = "opt_cs",       values = {"compilation_mode": "opt"})
mk(
    name = "world",
    who = select({
        "//:fastbuild_cs": "fastbuild",
        "//:opt_cs": "opt",
        "//conditions:default": "fallthrough",
    }),
)
"#,
            ),
        ],
    );
    let config = Configuration::default().with_build_setting("compilation_mode", "opt");
    let analyzed = engine(tmp.path().to_path_buf(), config)
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "PickInfo")
        .expect("PickInfo missing");
    assert_eq!(field_str(p, "v"), "opt");
}

#[test]
fn config_setting_flag_values_match_against_user_defined_build_setting() {
    // User-defined `bool_flag(build_setting_default=True)` —
    // analyzed off its `build_setting_default` attr (Loupe doesn't
    // model CLI overrides). `config_setting(flag_values={...})`
    // matches when the requested string equals the default.
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
# skylib is unvendored; a flag rule defines its own BuildSettingInfo.
# select() resolves the flag from engine state (build_setting_value),
# not by reading this provider — it's here only to mirror a realistic
# flag impl.
BuildSettingInfo = provider(fields = ["value"])

PickInfo = provider(fields = ["v"])
def _impl(ctx):
    return [PickInfo(v = ctx.attr.who)]
mk = rule(implementation = _impl, attrs = {"who": attr.string()})

def _flag_impl(ctx):
    return [BuildSettingInfo(value = "True" if ctx.build_setting_value else "False")]
my_flag = rule(implementation = _flag_impl, build_setting = config.bool(flag = True))
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk", "my_flag")
my_flag(name = "fancy", build_setting_default = True)
config_setting(name = "fancy_on",  flag_values = {"//:fancy": "True"})
config_setting(name = "fancy_off", flag_values = {"//:fancy": "False"})
mk(
    name = "world",
    who = select({
        "//:fancy_on":  "on",
        "//:fancy_off": "off",
        "//conditions:default": "fallthrough",
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
    assert_eq!(field_str(p, "v"), "on");
}

#[test]
fn command_line_option_flags_load_from_embedded_bazel_tools() {
    // `@bazel_tools//command_line_option:*` resolves through the
    // embedded `RepoRoot::Embedded` tree — the vendored BUILD.bazel +
    // settings.bzl evaluate normally; `bool_flag`/`string_flag` impls
    // return real `BuildSettingInfo`. No engine synthesis.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"module(name = "demo", version = "0.0.0")"#,
            ),
            ("BUILD.bazel", r#""#),
        ],
    );
    let eng = engine(tmp.path().to_path_buf(), Configuration::default());
    // Probe a small set the vendored BUILD.bazel declares as
    // string/bool flags (excluding label-typed ones — those forward
    // providers rather than carrying `BuildSettingInfo`).
    for key in ["compilation_mode", "cpu", "stamp"] {
        let l = Label::new(
            CanonicalRepo::new("bazel_tools"),
            PackagePath::new("command_line_option").unwrap(),
            key,
        );
        let analyzed = eng
            .analyze_target(&l)
            .unwrap_or_else(|e| panic!("analyzing @bazel_tools//command_line_option:{key}: {e}"));
        let bsi = analyzed
            .provider_by_symbol("BuildSettingInfo")
            .unwrap_or_else(|| panic!("BuildSettingInfo missing on {key}"));
        assert!(
            bsi.fields.contains_key("value"),
            "BuildSettingInfo.value missing on {key}"
        );
    }
}

#[test]
fn select_picks_constraint_value_via_setting_default_when_platform_silent() {
    // `constraint_setting(default_constraint_value = ":linux")`
    // declares the implicit default. The platform doesn't list any
    // CV for the `os_setting`, so a `select({":linux": ...})` branch
    // matches via the setting's default.
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

constraint_setting(
    name = "os_setting",
    default_constraint_value = ":linux",
)
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_value(name = "mac",   constraint_setting = ":os_setting")

# Platform declares no CV for `os_setting` — the setting's default
# should fill in.
platform(name = "host", constraint_values = [])

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
fn setting_default_does_not_override_explicit_platform_constraint() {
    // `os_setting` defaults to `:linux`, but the platform explicitly
    // selects `:mac`. The default must not fire — the explicit value
    // wins. (This is the load-bearing guard: a naive "always treat
    // the default as present" would double-match.)
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

constraint_setting(
    name = "os_setting",
    default_constraint_value = ":linux",
)
constraint_value(name = "linux", constraint_setting = ":os_setting")
constraint_value(name = "mac",   constraint_setting = ":os_setting")

platform(name = "host", constraint_values = ["//:mac"])

mk(
    name = "world",
    who = select({
        "//:linux": "linux_branch",
        "//:mac":   "mac_branch",
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
    assert_eq!(field_str(p, "v"), "mac_branch");
}

#[test]
fn select_errors_on_ambiguous_incomparable_matches() {
    // Two config_settings match on a platform with three constraints,
    // but their constraint sets are incomparable:
    //   cs_a = {linux, gcc}
    //   cs_b = {linux, x86_64}
    // Neither is a strict superset of the other → Bazel rule says
    // ambiguous → resolver raises.
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
constraint_value(name = "linux",  constraint_setting = ":os_setting")
constraint_setting(name = "arch_setting")
constraint_value(name = "x86_64", constraint_setting = ":arch_setting")
constraint_setting(name = "cc_setting")
constraint_value(name = "gcc",    constraint_setting = ":cc_setting")

config_setting(name = "cs_a", constraint_values = ["//:linux", "//:gcc"])
config_setting(name = "cs_b", constraint_values = ["//:linux", "//:x86_64"])

platform(
    name = "host",
    constraint_values = ["//:linux", "//:x86_64", "//:gcc"],
)

mk(
    name = "world",
    who = select({
        "//:cs_a": "a",
        "//:cs_b": "b",
        "//conditions:default": "default",
    }),
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
        msg.contains("multiple incomparable branches"),
        "expected ambiguous-select error, got: {msg}"
    );
}

#[test]
fn select_picks_refinement_with_mixed_cvs_and_flag_values() {
    // cs_a matches with set {linux}. cs_b matches with set
    // {linux, fancy} (a constraint_value plus a flag_values entry).
    // cs_b strictly refines cs_a → cs_b wins.
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
# skylib is unvendored; a flag rule defines its own BuildSettingInfo.
# select() resolves the flag from engine state (build_setting_value),
# not by reading this provider — it's here only to mirror a realistic
# flag impl.
BuildSettingInfo = provider(fields = ["value"])

PickInfo = provider(fields = ["v"])

def _impl(ctx):
    return [PickInfo(v = ctx.attr.who)]

mk = rule(
    implementation = _impl,
    attrs = {"who": attr.string()},
)

def _flag_impl(ctx):
    return [BuildSettingInfo(value = "True" if ctx.build_setting_value else "False")]

my_flag = rule(
    implementation = _flag_impl,
    build_setting = config.bool(flag = True),
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk", "my_flag")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")

my_flag(name = "fancy", build_setting_default = True)

config_setting(name = "cs_linux",       constraint_values = ["//:linux"])
config_setting(
    name = "cs_linux_fancy",
    constraint_values = ["//:linux"],
    flag_values = {"//:fancy": "True"},
)

platform(name = "host", constraint_values = ["//:linux"])

mk(
    name = "world",
    who = select({
        "//:cs_linux":       "linux_only",
        "//:cs_linux_fancy": "linux_fancy",
        "//conditions:default": "default",
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
    assert_eq!(field_str(p, "v"), "linux_fancy");
}

#[test]
fn toolchain_target_compatible_with_satisfied_by_setting_default() {
    // A toolchain's `target_compatible_with = [":linux"]` matches a
    // platform that doesn't list any CV for `os_setting`, because
    // `os_setting.default_constraint_value` is `:linux`. The toolchain
    // resolver routes through the same `constraint_is_satisfied`
    // helper as `select()`.
    let tmp = tempfile::tempdir().unwrap();
    let target_platform_label = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "host");
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
MyToolchainInfo = provider(fields = ["greeting"])

def _tc_impl(ctx):
    return [platform_common.ToolchainInfo(greeting = "hi")]

my_toolchain = rule(implementation = _tc_impl)

WhoInfo = provider(fields = ["v"])

def _consumer_impl(ctx):
    tc = ctx.toolchains["//:my_toolchain_type"]
    return [WhoInfo(v = tc.greeting)]

consumer = rule(
    implementation = _consumer_impl,
    toolchains = ["//:my_toolchain_type"],
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "my_toolchain", "consumer")

constraint_setting(
    name = "os_setting",
    default_constraint_value = ":linux",
)
constraint_value(name = "linux", constraint_setting = ":os_setting")

# Platform lists no CVs — `os_setting` falls back to `:linux`.
platform(name = "host", constraint_values = [])

toolchain_type(name = "my_toolchain_type")
my_toolchain(name = "tc_impl")
toolchain(
    name = "my_tc",
    toolchain = ":tc_impl",
    toolchain_type = ":my_toolchain_type",
    target_compatible_with = ["//:linux"],
)

consumer(name = "world")
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
        .find(|p| p.id.symbol == "WhoInfo")
        .expect("WhoInfo missing — toolchain didn't resolve");
    assert_eq!(field_str(p, "v"), "hi");
}

#[test]
fn list_plus_select_concatenates_into_one_attr() {
    // `[...] + select({...})` must produce a single concatenated list
    // attr. rules_js's `npm_import` builds `args = [pkg, ...] + select({...})`;
    // without `add`/`radd` on the select value the `+` fails at load time.
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
    return [PickInfo(v = ",".join(ctx.attr.who))]

mk = rule(
    implementation = _impl,
    attrs = {"who": attr.string_list()},
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")

constraint_setting(name = "os_setting")
constraint_value(name = "linux", constraint_setting = ":os_setting")

platform(
    name = "linux_plat",
    constraint_values = ["//:linux"],
)

mk(
    name = "world",
    who = ["base"] + select({
        "//:linux": ["linux_branch"],
        "//conditions:default": ["default_branch"],
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
    assert_eq!(field_str(p, "v"), "base,linux_branch");
}
