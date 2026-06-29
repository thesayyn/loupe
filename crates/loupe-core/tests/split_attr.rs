//! `ctx.split_attr` — the keyed `{branch_key: Target}` view of a dep
//! configured through a split (1:N) `attr.label(cfg = transition)`.
//! `ctx.attr.<name>` surfaces the same deps as a flat `[Target]`.

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
    Label::new(CanonicalRepo::main(), PackagePath::root(), name)
}

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

#[test]
fn split_attr_exposes_keyed_targets() {
    // The split transition returns a `dict[str, dict]` setting cpu to two
    // values. The `top` rule attaches it to a single `attr.label` dep,
    // which fans out into one configured target per branch. The impl
    // self-checks `ctx.split_attr` (keyed) against `ctx.attr` (flat list);
    // any mismatch raises and fails analysis.
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
def _transition_impl(settings, attr):
    return {
        "x86": {"//command_line_option:cpu": "x86"},
        "arm": {"//command_line_option:cpu": "arm"},
    }

multi = transition(
    implementation = _transition_impl,
    inputs = [],
    outputs = ["//command_line_option:cpu"],
)

def _dep_impl(ctx):
    return [DefaultInfo()]

dep_rule = rule(implementation = _dep_impl)

def _top_impl(ctx):
    sa = ctx.split_attr.tools
    if sorted(sa.keys()) != ["arm", "x86"]:
        fail("split_attr keys wrong: %s" % sorted(sa.keys()))
    if sa["x86"].label.name != "t":
        fail("split_attr value is not the dep Target: %s" % sa["x86"])
    if len(ctx.attr.tools) != 2:
        fail("ctx.attr.tools should be a 2-element list, got %d" % len(ctx.attr.tools))
    return [DefaultInfo()]

top = rule(
    implementation = _top_impl,
    attrs = {
        "tools": attr.label(cfg = multi),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "top", "dep_rule")
dep_rule(name = "t")
top(name = "main", tools = ":t")
"#,
            ),
        ],
    );
    // Success = the impl's self-checks all passed.
    engine(tmp.path().to_path_buf())
        .analyze_target(&label("main"))
        .unwrap();
}

#[test]
fn split_attr_label_list_keys_to_lists() {
    // A split transition on an `attr.label_list` surfaces
    // `ctx.split_attr.<name>` as `{branch_key: [Target]}` — one target
    // per list element per branch — while `ctx.attr.<name>` flattens to
    // the full cross-product list.
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
def _ti(settings, attr):
    return {
        "x86": {"//command_line_option:cpu": "x86"},
        "arm": {"//command_line_option:cpu": "arm"},
    }

multi = transition(implementation = _ti, inputs = [], outputs = ["//command_line_option:cpu"])

def _dep_impl(ctx):
    return [DefaultInfo()]

dep_rule = rule(implementation = _dep_impl)

def _top_impl(ctx):
    sa = ctx.split_attr.tools
    if sorted(sa.keys()) != ["arm", "x86"]:
        fail("split_attr keys wrong: %s" % sorted(sa.keys()))
    for k in sa.keys():
        if type(sa[k]) != "list":
            fail("label_list split value must be a list, got %s" % type(sa[k]))
        if len(sa[k]) != 2:
            fail("expected 2 targets for branch %s, got %d" % (k, len(sa[k])))
    if len(ctx.attr.tools) != 4:
        fail("ctx.attr.tools should flatten to 4, got %d" % len(ctx.attr.tools))
    return [DefaultInfo()]

top = rule(
    implementation = _top_impl,
    attrs = {
        "tools": attr.label_list(cfg = multi),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "top", "dep_rule")
dep_rule(name = "a")
dep_rule(name = "b")
top(name = "main", tools = [":a", ":b"])
"#,
            ),
        ],
    );
    engine(tmp.path().to_path_buf())
        .analyze_target(&label("main"))
        .unwrap();
}
