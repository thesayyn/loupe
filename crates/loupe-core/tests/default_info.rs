//! Integration tests for the native `DefaultInfo` provider and the
//! `target.files` / `ctx.files.<attr>` / `ctx.file.<attr>` /
//! `ctx.executable.<attr>` accessors built on top of it.

use std::fs;
use std::path::PathBuf;

use loupe_core::analysis::CapturedProvider;
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
    Label::new(CanonicalRepo::main(), PackagePath::root(), name)
}

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

/// Helper: read a string field off a captured provider.
fn field_str(p: &CapturedProvider, name: &str) -> String {
    let v = p
        .fields
        .get(name)
        .unwrap_or_else(|| panic!("provider missing field `{name}`"));
    v.to_value()
        .unpack_str()
        .unwrap_or_else(|| {
            panic!(
                "field `{name}` is not a string: {}",
                v.to_value().get_type()
            )
        })
        .to_string()
}

#[test]
fn default_info_is_synthesized_when_impl_does_not_return_one() {
    // The impl returns `[]` — analysis must still surface a
    // `DefaultInfo` so consumers iterating providers don't see a
    // shape that violates Bazel's invariant.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    return []

empty = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "empty")
empty(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let di = analyzed.default_info();
    assert_eq!(di.id.symbol, "DefaultInfo");
    assert!(di.id.defining_bzl.is_none(), "DefaultInfo is native");
    assert!(analyzed.default_files().is_empty());
    assert!(analyzed.default_executable().is_none());
}

#[test]
fn impl_provided_default_info_overrides_synthetic() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    out = ctx.actions.declare_file("greeting.txt")
    ctx.actions.write(out, "hi\n")
    return [DefaultInfo(files = [out])]

writer = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "writer")
writer(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    // Exactly one DefaultInfo (impl-returned). No synthetic added.
    let count = analyzed
        .providers
        .iter()
        .filter(|p| p.id.symbol == "DefaultInfo")
        .count();
    assert_eq!(count, 1);
    let files = analyzed.default_files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].basename(), "greeting.txt");
    assert_eq!(files[0].path(), "bazel-out/k8-fastbuild/bin/greeting.txt");
}

#[test]
fn target_dot_files_reads_dep_default_info() {
    // The middle rule reads `ctx.attr.dep.files` (dep's
    // `target.files`) and embeds the basename in its provider so we
    // can assert from the test side.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
WhatInfo = provider(fields = ["names"])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file("leaf.txt")
    ctx.actions.write(out, "leaf")
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    names = ",".join([f.basename for f in ctx.attr.dep.files])
    return [WhatInfo(names = names)]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "WhatInfo")
        .unwrap();
    assert_eq!(field_str(p, "names"), "leaf.txt");
}

#[test]
fn ctx_files_attr_flattens_label_list_dep_default_files() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
NamesInfo = provider(fields = ["names"])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.attr.outname)
    ctx.actions.write(out, ctx.attr.outname)
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    # ctx.files.deps surfaces as a flat list across all deps.
    names = ",".join([f.basename for f in ctx.files.deps])
    return [NamesInfo(names = names)]

leaf = rule(
    implementation = _leaf_impl,
    attrs = {"outname": attr.string()},
)
mid  = rule(
    implementation = _mid_impl,
    attrs = {"deps": attr.label_list()},
)
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a", outname = "a.txt")
leaf(name = "b", outname = "b.txt")
mid(name = "world", deps = [":a", ":b"])"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "NamesInfo")
        .unwrap();
    assert_eq!(field_str(p, "names"), "a.txt,b.txt");
}

#[test]
fn ctx_file_attr_returns_single_file_for_label_dep() {
    // `ctx.file.<attr>` is the single-file convenience reader for
    // `attr.label` attrs. Returns `None` when the dep has no files.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
NameInfo = provider(fields = ["name"])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file("leaf.txt")
    ctx.actions.write(out, "leaf")
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    f = ctx.file.dep
    return [NameInfo(name = f.basename if f else "<none>")]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "NameInfo")
        .unwrap();
    assert_eq!(field_str(p, "name"), "leaf.txt");
}

#[test]
fn ctx_executable_attr_returns_dep_default_executable() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ToolNameInfo = provider(fields = ["name"])

def _tool_impl(ctx):
    bin = ctx.actions.declare_file("tool.sh")
    # Content is irrelevant — the test only inspects the File's
    # basename via `ctx.executable.tool`.
    ctx.actions.write(bin, "tool body", is_executable = True)
    return [DefaultInfo(files = [bin], executable = bin)]

def _user_impl(ctx):
    e = ctx.executable.tool
    return [ToolNameInfo(name = e.basename if e else "<none>")]

tool = rule(implementation = _tool_impl)
user = rule(
    implementation = _user_impl,
    attrs = {"tool": attr.label(executable = True)},
)
"#,
        r#"load(":defs.bzl", "tool", "user")
tool(name = "t")
user(name = "world", tool = ":t")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ToolNameInfo")
        .unwrap();
    assert_eq!(field_str(p, "name"), "tool.sh");
}

#[test]
fn default_info_files_accept_depset_and_route_to_ctx_file() {
    // rules_oci's `oci_image` returns
    // `DefaultInfo(files = depset([output]))`. Without depset
    // flattening in `unpack_default_files`, downstream consumers
    // got `ctx.file.<dep>` = None and `ctx.files.<dep>` = []. The
    // depset(direct=...)+depset(transitive=...) form must also work
    // — that's what `depset([a], transitive = [other_depset])`
    // produces.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
SeenInfo = provider(fields = ["one_basename", "all_basenames"])

def _leaf_impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    ctx.actions.write(a, "a")
    ctx.actions.write(b, "b")
    inner = depset([a])
    return [DefaultInfo(files = depset([b], transitive = [inner]))]

def _consumer_impl(ctx):
    one = ctx.file.dep.basename
    many = ",".join([f.basename for f in ctx.files.dep])
    return [SeenInfo(one_basename = one, all_basenames = many)]

leaf     = rule(implementation = _leaf_impl)
consumer = rule(implementation = _consumer_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "consumer")
leaf(name = "lf")
consumer(name = "world", dep = ":lf")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    // ctx.file.<dep> is the FIRST file the depset enumerates. The
    // postorder walk surfaces transitive children first, so `a.txt`
    // comes ahead of `b.txt`.
    assert_eq!(field_str(p, "one_basename"), "a.txt");
    assert_eq!(field_str(p, "all_basenames"), "a.txt,b.txt");
}

#[test]
fn depset_accepts_direct_named_kwarg() {
    // `depset(direct = [...])` — Bazel's named-kwarg form is what
    // most ruleset code uses (rules_oci, rules_python). Loupe used
    // to require `direct` as positional, breaking analysis of any
    // rule that constructed depsets the named way.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
SeenInfo = provider(fields = ["names"])

def _impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    ctx.actions.write(a, "a")
    ctx.actions.write(b, "b")
    inner = depset(direct = [a])
    outer = depset(direct = [b], transitive = [inner])
    names = ",".join([f.basename for f in outer.to_list()])
    return [SeenInfo(names = names)]

mk = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "names"), "a.txt,b.txt");
}

#[test]
fn args_add_two_positional_pushes_flag_and_value() {
    // `args.add("--flag", value)` is the form rules_oci, rules_go,
    // and rules_python use to emit `--flag value` argv pairs.
    // Previously the second positional was rejected — only
    // `args.add(value, format = "--flag=%s")` worked.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ArgvInfo = provider(fields = ["argv"])

def _leaf_impl(ctx):
    tool = ctx.actions.declare_file("tool.bin")
    out  = ctx.actions.declare_file("out.bin")
    ctx.actions.write(tool, "")
    args = ctx.actions.args()
    args.add("--name", "world")
    args.add("--count", 42)
    ctx.actions.run(executable = tool, arguments = [args], outputs = [out], mnemonic = "Run")
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    for a in ctx.attr.dep.actions:
        if a.mnemonic == "Run":
            return [ArgvInfo(argv = ",".join(a.argv))]
    return [ArgvInfo(argv = "<missing>")]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "ArgvInfo")
        .expect("ArgvInfo missing");
    assert_eq!(field_str(p, "argv"), "--name,world,--count,42");
}

#[test]
fn ctx_files_honors_preorder_depset() {
    // Bazel preorder: parent direct first, then transitive (left-to-right).
    // Tree:
    //   root direct = [r.txt], transitive = [inner]
    //   inner direct = [a.txt, b.txt]
    // Expected preorder: r.txt, a.txt, b.txt
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
SeenInfo = provider(fields = ["names"])

def _leaf_impl(ctx):
    r = ctx.actions.declare_file("r.txt")
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    ctx.actions.write(r, "r")
    ctx.actions.write(a, "a")
    ctx.actions.write(b, "b")
    inner = depset(direct = [a, b], order = "preorder")
    return [DefaultInfo(files = depset(direct = [r], transitive = [inner], order = "preorder"))]

def _consumer_impl(ctx):
    names = ",".join([f.basename for f in ctx.files.dep])
    return [SeenInfo(names = names)]

leaf     = rule(implementation = _leaf_impl)
consumer = rule(implementation = _consumer_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "consumer")
leaf(name = "lf")
consumer(name = "world", dep = ":lf")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "names"), "r.txt,a.txt,b.txt");
}

#[test]
fn ctx_files_honors_postorder_depset_with_last_wins_dedupe() {
    // Postorder = children-first, last-wins on duplicates. Same shape
    // as the depset module's `postorder_dedupe_keeps_last_occurrence`
    // test, surfaced through `ctx.files`.
    //
    // Tree:
    //   root direct = [a.txt, x.txt], transitive = [c1, c2]
    //   c1   direct = [x.txt, b.txt]
    //   c2   direct = [c.txt]
    // Unfiltered postorder: x, b, c, a, x  → last-wins: b, c, a, x
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
SeenInfo = provider(fields = ["names"])

def _leaf_impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    c = ctx.actions.declare_file("c.txt")
    x = ctx.actions.declare_file("x.txt")
    for f in [a, b, c, x]:
        ctx.actions.write(f, "")
    c1 = depset(direct = [x, b])
    c2 = depset(direct = [c])
    return [DefaultInfo(files = depset(direct = [a, x], transitive = [c1, c2]))]

def _consumer_impl(ctx):
    names = ",".join([f.basename for f in ctx.files.dep])
    return [SeenInfo(names = names)]

leaf     = rule(implementation = _leaf_impl)
consumer = rule(implementation = _consumer_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "consumer")
leaf(name = "lf")
consumer(name = "world", dep = ":lf")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "names"), "b.txt,c.txt,a.txt,x.txt");
}

#[test]
fn ctx_files_honors_topological_depset_first_wins_dedupe() {
    // Same children-first traversal as postorder, but first-wins
    // dedupe — shared items float up to their first occurrence so
    // they appear *before* every node that depends on them.
    //
    // Same tree as the postorder test:
    //   Unfiltered postorder: x, b, c, a, x  → first-wins: x, b, c, a
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
SeenInfo = provider(fields = ["names"])

def _leaf_impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    c = ctx.actions.declare_file("c.txt")
    x = ctx.actions.declare_file("x.txt")
    for f in [a, b, c, x]:
        ctx.actions.write(f, "")
    c1 = depset(direct = [x, b], order = "topological")
    c2 = depset(direct = [c], order = "topological")
    return [DefaultInfo(files = depset(
        direct = [a, x],
        transitive = [c1, c2],
        order = "topological",
    ))]

def _consumer_impl(ctx):
    names = ",".join([f.basename for f in ctx.files.dep])
    return [SeenInfo(names = names)]

leaf     = rule(implementation = _leaf_impl)
consumer = rule(implementation = _consumer_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "consumer")
leaf(name = "lf")
consumer(name = "world", dep = ":lf")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    assert_eq!(field_str(p, "names"), "x.txt,b.txt,c.txt,a.txt");
}

#[test]
fn ctx_files_dedupes_diamond_shared_file_only_once() {
    // Diamond: a shared File is reachable through two transitive
    // children. The dedupe must collapse it to one entry — File
    // equality is keyed on (kind, package, short_path, config) so
    // two refs to the same declare_file collapse cleanly.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
SeenInfo = provider(fields = ["count", "names"])

def _leaf_impl(ctx):
    shared = ctx.actions.declare_file("shared.txt")
    l_only = ctx.actions.declare_file("l.txt")
    r_only = ctx.actions.declare_file("r.txt")
    for f in [shared, l_only, r_only]:
        ctx.actions.write(f, "")
    base   = depset(direct = [shared])
    left   = depset(direct = [l_only], transitive = [base])
    right  = depset(direct = [r_only], transitive = [base])
    return [DefaultInfo(files = depset(direct = [], transitive = [left, right]))]

def _consumer_impl(ctx):
    fs = ctx.files.dep
    return [SeenInfo(
        count = str(len(fs)),
        names = ",".join(sorted([f.basename for f in fs])),
    )]

leaf     = rule(implementation = _leaf_impl)
consumer = rule(implementation = _consumer_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "consumer")
leaf(name = "lf")
consumer(name = "world", dep = ":lf")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "SeenInfo")
        .expect("SeenInfo missing");
    // Without dedupe, shared.txt would surface twice → count == 4.
    assert_eq!(field_str(p, "count"), "3");
    assert_eq!(field_str(p, "names"), "l.txt,r.txt,shared.txt");
}
