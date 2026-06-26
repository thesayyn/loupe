//! Tests that go beyond what `ctx_actions.rs` covers: inspecting
//! `expand_template` substitutions in the recorded action and
//! `args.add_all` over a depset.

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
fn expand_template_records_substitutions_map() {
    // `ctx.actions.expand_template` builds a TemplateExpand action.
    // The substitutions map should be preserved verbatim on the
    // recorded action so cquery / aquery can read it back.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"module(name = "demo", version = "0.0.0")"#,
            ),
            ("tmpl.in", "hello {NAME} v{VERSION}\n"),
            (
                "defs.bzl",
                r#"
def _impl(ctx):
    out = ctx.actions.declare_file("rendered.txt")
    ctx.actions.expand_template(
        template = ctx.file.template,
        output = out,
        substitutions = {
            "{NAME}": ctx.attr.name,
            "{VERSION}": "1.2.3",
        },
    )
    return [DefaultInfo(files = [out])]
mk = rule(
    implementation = _impl,
    attrs = {"template": attr.label(allow_single_file = True)},
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "mk")
mk(name = "world", template = "tmpl.in")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let exp = analyzed
        .actions
        .iter()
        .find(|a| matches!(&a.kind, loupe_core::ActionKind::ExpandTemplate { .. }))
        .expect("expand_template action missing");
    if let loupe_core::ActionKind::ExpandTemplate { substitutions, .. } = &exp.kind {
        let m: std::collections::BTreeMap<&str, &str> = substitutions
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(m.get("{NAME}").copied(), Some("world"));
        assert_eq!(m.get("{VERSION}").copied(), Some("1.2.3"));
    } else {
        unreachable!();
    }
}

#[test]
#[ignore = "TODO(args): Loupe's `args.add_all` currently rejects \
    `depset` inputs (`expected list, got depset`). Bazel's Args accepts \
    a depset and flattens lazily per the depset's order. Enable this \
    test once `args.add_all` accepts depsets."]
fn args_add_all_over_depset_records_argv_in_dep_order() {
    // Aquery-like introspection: mid_rule reads dep.actions and
    // surfaces the argv. The leaf rule packs a depset into the
    // recorded `arguments` via `args.add_all(depset(...))`.
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
ArgvInfo = provider(fields = ["argv"])

def _leaf_impl(ctx):
    tool = ctx.actions.declare_file("tool.bin")
    out = ctx.actions.declare_file("out.bin")
    ctx.actions.write(tool, "tool")
    a = ctx.actions.args()
    a.add_all(depset(direct = ["one", "two", "three"]))
    ctx.actions.run(
        executable = tool,
        arguments = [a],
        outputs = [out],
        mnemonic = "Run",
    )
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    for action in ctx.attr.dep.actions:
        if action.mnemonic == "Run":
            return [ArgvInfo(argv = ",".join(action.argv))]
    return [ArgvInfo(argv = "<missing>")]

leaf = rule(implementation = _leaf_impl)
mid = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")
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
        .find(|p| p.id.symbol == "ArgvInfo")
        .unwrap();
    let argv = p.fields.get("argv").unwrap().to_value().to_string();
    // argv elements should include all three depset items.
    assert!(
        argv.contains("one") && argv.contains("two") && argv.contains("three"),
        "depset items missing from argv: {argv}"
    );
}
