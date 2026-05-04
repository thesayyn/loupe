//! Integration tests for the Target magic attrs added in this round —
//! `target.actions`, `target.files_to_run`, `target.output_groups` —
//! and the Starlark surface on `runfiles` (`merge`, `merge_all`,
//! `symlinks`, `root_symlinks`).

use std::fs;
use std::path::PathBuf;

use loupe_core::analysis::CapturedProvider;
use loupe_core::label::{CanonicalRepo, Label, PackagePath};
use loupe_core::{Configuration, Engine};

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

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

fn field_str(p: &CapturedProvider, name: &str) -> String {
    let v = p
        .fields
        .get(name)
        .unwrap_or_else(|| panic!("provider missing field `{name}`"));
    v.to_value()
        .unpack_str()
        .unwrap_or_else(|| panic!("field `{name}` is not a string"))
        .to_string()
}

#[test]
fn target_actions_exposes_recorded_actions_with_bazel_attrs() {
    // The middle rule reads `ctx.attr.dep.actions` and returns a
    // CSV of `mnemonic|primary-output basename|first-arg-or-empty`,
    // exercising every Bazel-shaped attr v1 surfaces (mnemonic,
    // outputs, argv).
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
SeenInfo = provider(fields = ["seen"])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file("leaf.txt")
    ctx.actions.write(out, "leaf body")
    tool = ctx.actions.declare_file("tool.bin")
    ctx.actions.write(tool, "tool body", is_executable = True)
    log = ctx.actions.declare_file("log.txt")
    ctx.actions.run_shell(
        command = "echo hi > $1",
        arguments = ["arg0", "arg1"],
        outputs = [log],
        mnemonic = "Echo",
    )
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    rows = []
    for a in ctx.attr.dep.actions:
        out0 = a.outputs[0].basename if a.outputs else ""
        first_arg = a.argv[0] if a.argv else ""
        rows.append(a.mnemonic + "|" + out0 + "|" + first_arg)
    return [SeenInfo(seen = ";".join(rows))]

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
        .find(|p| p.id.symbol == "SeenInfo")
        .unwrap();
    // Order matches recording order: write leaf, write tool, run_shell.
    assert_eq!(
        field_str(p, "seen"),
        "FileWrite|leaf.txt|;FileWrite|tool.bin|;Echo|log.txt|arg0",
    );
}

#[test]
fn target_files_to_run_reads_default_info_executable() {
    // `target.files_to_run.executable` is the most-used field — it's
    // what wrapper rules grab when they need the tool path.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ToolPathInfo = provider(fields = ["path"])

def _tool_impl(ctx):
    bin = ctx.actions.declare_file("tool.sh")
    ctx.actions.write(bin, "tool body", is_executable = True)
    return [DefaultInfo(files = [bin], executable = bin)]

def _user_impl(ctx):
    ftr = ctx.attr.tool.files_to_run
    return [ToolPathInfo(path = ftr.executable.basename if ftr.executable else "<none>")]

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
        .find(|p| p.id.symbol == "ToolPathInfo")
        .unwrap();
    assert_eq!(field_str(p, "path"), "tool.sh");
}

#[test]
fn target_output_groups_is_an_empty_struct_stub() {
    // STUB at v1: empty struct so `target.output_groups` doesn't
    // crash, but no real OutputGroupInfo plumbing.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ShapeInfo = provider(fields = ["shape"])

def _leaf_impl(ctx):
    return [DefaultInfo()]

def _mid_impl(ctx):
    og = ctx.attr.dep.output_groups
    return [ShapeInfo(shape = type(og))]

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
        .find(|p| p.id.symbol == "ShapeInfo")
        .unwrap();
    assert_eq!(field_str(p, "shape"), "struct");
}

#[test]
fn runfiles_merge_combines_files_and_first_wins_on_symlink_collision() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
ResultInfo = provider(fields = ["files", "syms"])

def _impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    sa = ctx.actions.declare_file("sa.txt")
    sb = ctx.actions.declare_file("sb.txt")
    ctx.actions.write(a, "a")
    ctx.actions.write(b, "b")
    ctx.actions.write(sa, "sa")
    ctx.actions.write(sb, "sb")
    ra = ctx.runfiles(files = [a], symlinks = {"foo": sa})
    rb = ctx.runfiles(files = [b], symlinks = {"foo": sb})
    merged = ra.merge(rb)
    return [ResultInfo(
        files = ",".join([f.basename for f in merged.files]),
        syms = merged.symlinks["foo"].basename,
    )]

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
        .find(|p| p.id.symbol == "ResultInfo")
        .unwrap();
    assert_eq!(field_str(p, "files"), "a.txt,b.txt");
    assert_eq!(
        field_str(p, "syms"),
        "sa.txt",
        "merge is first-wins on symlink collisions"
    );
}

#[test]
fn runfiles_merge_all_folds_in_declaration_order() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
NamesInfo = provider(fields = ["names"])

def _impl(ctx):
    fs = []
    for n in ["a", "b", "c"]:
        f = ctx.actions.declare_file(n + ".txt")
        ctx.actions.write(f, n)
        fs.append(f)
    r0 = ctx.runfiles(files = [fs[0]])
    r1 = ctx.runfiles(files = [fs[1]])
    r2 = ctx.runfiles(files = [fs[2]])
    merged = r0.merge_all([r1, r2])
    return [NamesInfo(names = ",".join([f.basename for f in merged.files]))]

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
        .find(|p| p.id.symbol == "NamesInfo")
        .unwrap();
    assert_eq!(field_str(p, "names"), "a.txt,b.txt,c.txt");
}

#[test]
fn runfiles_root_symlinks_attr_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
PathInfo = provider(fields = ["path"])

def _impl(ctx):
    f = ctx.actions.declare_file("rooted.txt")
    ctx.actions.write(f, "x")
    r = ctx.runfiles(root_symlinks = {"_main/foo": f})
    return [PathInfo(path = r.root_symlinks["_main/foo"].basename)]

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
        .find(|p| p.id.symbol == "PathInfo")
        .unwrap();
    assert_eq!(field_str(p, "path"), "rooted.txt");
}
