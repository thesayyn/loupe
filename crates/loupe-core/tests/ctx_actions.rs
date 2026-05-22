//! Integration tests for `ctx.actions.*`, `ctx.runfiles(...)`, and
//! the `// STUB:` shims on the `ctx` surface.
//!
//! Each test stands up a tiny synthetic workspace with one rule that
//! exercises the API and inspects the resulting `AnalyzedTarget`.

use std::fs;
use std::path::PathBuf;

use loupe_core::action::ActionKind;
use loupe_core::analysis::CapturedProvider;
use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

/// Extract a captured provider field as a Rust `String`. Provider
/// fields are `FrozenValue`s into the target's analysis heap; this
/// is the test-side bridge to a comparable type.
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

fn write_workspace(dir: &std::path::Path, defs_bzl: &str, build: &str) {
    fs::write(
        dir.join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(dir.join("defs.bzl"), defs_bzl).unwrap();
    fs::write(dir.join("BUILD.bazel"), build).unwrap();
}

fn world_label() -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), "world")
}

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

fn run_and_get_provider(defs: &str, build: &str, sym: &str) -> CapturedProvider {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(tmp.path(), defs, build);
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();
    analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == sym)
        .cloned()
        .unwrap_or_else(|| panic!("provider {sym} missing"))
}

#[test]
fn ctx_actions_write_records_a_filewrite_action() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    out = ctx.actions.declare_file("greeting.txt")
    ctx.actions.write(out, "hello, world\n")
    return []

writer = rule(implementation = _impl)
"#,
        r#"
load(":defs.bzl", "writer")
writer(name = "world")
"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();

    assert_eq!(analyzed.actions.len(), 1, "one ctx.actions.write call");
    let a = &analyzed.actions[0];
    match &a.kind {
        ActionKind::Write {
            content,
            is_executable,
        } => {
            assert_eq!(content, "hello, world\n");
            assert!(!is_executable);
        }
        other => panic!("expected Write, got {other:?}"),
    }
    assert_eq!(a.outputs.len(), 1);
    assert_eq!(
        a.outputs[0].path(),
        "bazel-out/k8-fastbuild/bin/greeting.txt",
        "declare_file produces a generated-bin File"
    );
    assert_eq!(a.mnemonic.as_deref(), Some("FileWrite"));
}

#[test]
fn ctx_actions_run_records_executable_inputs_outputs_env() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    tool = ctx.actions.declare_file("tool.sh")
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.run(
        executable = tool,
        arguments = ["--flag", "v"],
        inputs = [tool],
        outputs = [out],
        env = {"K": "V"},
        mnemonic = "Frobnicate",
        progress_message = "frobbing",
    )
    return []

runner = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "runner")
runner(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();

    assert_eq!(analyzed.actions.len(), 1);
    let a = &analyzed.actions[0];
    match &a.kind {
        ActionKind::Run {
            executable,
            arguments,
        } => {
            assert_eq!(executable.basename(), "tool.sh");
            assert_eq!(arguments, &vec!["--flag".to_string(), "v".to_string()]);
        }
        other => panic!("expected Run, got {other:?}"),
    }
    assert_eq!(a.inputs.len(), 1);
    assert_eq!(a.outputs.len(), 1);
    assert_eq!(a.env.get("K").map(String::as_str), Some("V"));
    assert_eq!(a.mnemonic.as_deref(), Some("Frobnicate"));
    assert_eq!(a.progress_message.as_deref(), Some("frobbing"));
}

#[test]
fn ctx_actions_run_shell_records_command_and_args() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.run_shell(
        command = "echo $1 > $@",
        arguments = ["greeting"],
        outputs = [out],
    )
    return []

shellrun = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "shellrun")
shellrun(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();

    // Vendored `wrappers.bzl::ctx_runfiles`'s `actions_run_shell`
    // rewrites the call to `actions.run(executable="/bin/bash",
    // arguments=["-c", command, ""] + arguments, ...)`. The recorded
    // action is `Run` with `/bin/bash` as the executable and the
    // shell command threaded through `arguments`.
    let a = &analyzed.actions[0];
    match &a.kind {
        ActionKind::Run {
            executable,
            arguments,
        } => {
            assert_eq!(executable.path(), "/bin/bash");
            assert_eq!(
                arguments,
                &vec![
                    "-c".to_string(),
                    "echo $1 > $@".to_string(),
                    "".to_string(),
                    "greeting".to_string(),
                ]
            );
        }
        other => panic!("expected Run (from vendored run_shell rewrite), got {other:?}"),
    }
}

#[test]
fn ctx_actions_symlink_records_target_and_output() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    src = ctx.actions.declare_file("src.txt")
    sl  = ctx.actions.declare_file("link.txt")
    ctx.actions.symlink(output = sl, target_file = src)
    return []

linker = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "linker")
linker(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();

    let a = &analyzed.actions[0];
    match &a.kind {
        ActionKind::Symlink { target } => {
            assert_eq!(target.basename(), "src.txt");
        }
        other => panic!("expected Symlink, got {other:?}"),
    }
    // The target also appears in `inputs` so action-graph consumers
    // see the dependency without inspecting the kind.
    assert_eq!(a.inputs.len(), 1);
    assert_eq!(a.outputs.len(), 1);
    assert_eq!(a.outputs[0].basename(), "link.txt");
}

#[test]
fn action_for_output_finds_by_short_path() {
    // Aquery-style consumers look up actions by primary-output path.
    // Confirm the convenience accessor matches what they'd see.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    ctx.actions.write(a, "A")
    ctx.actions.write(b, "B")
    return []

twoer = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "twoer")
twoer(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();
    assert_eq!(analyzed.actions.len(), 2);
    let a = analyzed.action_for_output("a.txt").unwrap();
    match &a.kind {
        ActionKind::Write { content, .. } => assert_eq!(content, "A"),
        _ => panic!(),
    }
    let b = analyzed.action_for_output("b.txt").unwrap();
    match &b.kind {
        ActionKind::Write { content, .. } => assert_eq!(content, "B"),
        _ => panic!(),
    }
    assert!(analyzed.action_for_output("missing.txt").is_none());
}

#[test]
fn ctx_runfiles_factory_carries_files_and_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
HelloInfo = provider(fields = ["count"])

def _impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    rf = ctx.runfiles(files = [a, b], symlinks = {"alt/a": a})
    return [HelloInfo(count = str(len(rf.files.to_list())))]

rf_rule = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "rf_rule")
rf_rule(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "HelloInfo")
        .expect("HelloInfo present");
    assert_eq!(
        field_str(p, "count"),
        "2",
        "rf.files surfaces both direct entries"
    );
}

#[test]
fn ctx_bin_dir_and_genfiles_dir_use_active_config_segment() {
    // bin_dir / genfiles_dir surface as Bazel-shaped `root` structs
    // with a `.path` attribute (`bazel-out/<config>/bin`). genfiles
    // collapses into bin/ — matches modern Bazel.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
HelloInfo = provider(fields = ["bin", "gen"])

def _impl(ctx):
    return [HelloInfo(bin = ctx.bin_dir.path, gen = ctx.genfiles_dir.path)]

dirs_rule = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "dirs_rule")
dirs_rule(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();
    let p = &analyzed.providers[0];
    assert_eq!(field_str(p, "bin"), "bazel-out/k8-fastbuild/bin");
    assert_eq!(field_str(p, "gen"), "bazel-out/k8-fastbuild/bin");
}

#[test]
fn ctx_stubs_surface_universal_common_attrs() {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
HelloInfo = provider(fields = ["files", "file", "executable", "toolchains", "fragments_bound", "var", "configuration"])

def _impl(ctx):
    return [HelloInfo(
        files = str(dir(ctx.files)),
        file = str(dir(ctx.file)),
        executable = str(dir(ctx.executable)),
        toolchains = str(len(ctx.toolchains)),
        fragments_bound = str(hasattr(ctx, "fragments")),
        var = str(sorted(ctx.var.keys())),
        configuration = str(ctx.configuration),
    )]

stubs_rule = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "stubs_rule")
stubs_rule(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();
    let p = &analyzed.providers[0];
    assert_eq!(
        field_str(p, "files"),
        "[\"compatible_with\", \"exec_compatible_with\", \"restricted_to\", \"target_compatible_with\", \"toolchains\"]"
    );
    assert_eq!(field_str(p, "file"), "[]");
    assert_eq!(field_str(p, "executable"), "[]");
    assert_eq!(field_str(p, "toolchains"), "0");
    // The vendored `_wrap_rule_ctx` only binds `ctx.fragments` when
    // the rule declared `fragments=` (signalled via the `__fragments`
    // hidden attr). This rule declares none, so it stays unbound.
    assert_eq!(field_str(p, "fragments_bound"), "False");
    // `ctx.var` is always populated with Bazel's built-in make
    // variables, even when no toolchain contributes its own.
    assert_eq!(
        field_str(p, "var"),
        "[\"BINDIR\", \"COMPILATION_MODE\", \"GENDIR\", \"TARGET_CPU\"]"
    );
    assert_eq!(
        field_str(p, "configuration"),
        "struct(default_shell_env={}, host_path_separator=\":\", coverage_enabled=False)"
    );
}

#[test]
fn ctx_expand_location_resolves_directive_to_dep_path() {
    // The vendored `wrappers.bzl::ctx_expand_location` resolves
    // `$(location …)` / `$(execpath …)` against the targets the
    // caller passes — looking each label up in
    // `target.original_label` and substituting the dep's single
    // default file path.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
HelloInfo = provider(fields = ["s"])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file("leaf.out")
    ctx.actions.write(out, "")
    return [DefaultInfo(files = depset([out]))]

def _consume_impl(ctx):
    s = ctx.expand_location("run $(location :leaf)", targets = ctx.attr.deps)
    return [HelloInfo(s = s)]

leaf = rule(implementation = _leaf_impl)
consume = rule(
    implementation = _consume_impl,
    attrs = {"deps": attr.label_list()},
)
"#,
        r#"load(":defs.bzl", "leaf", "consume")
leaf(name = "leaf")
consume(name = "world", deps = [":leaf"])"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();
    assert_eq!(
        field_str(&analyzed.providers[0], "s"),
        "run bazel-out/k8-fastbuild/bin/leaf.out"
    );
}

#[test]
fn declare_directory_marks_file_as_is_directory() {
    let p = run_and_get_provider(
        r#"
ShapeInfo = provider(fields = ["name", "is_dir", "is_file"])

def _impl(ctx):
    d = ctx.actions.declare_directory("treeart")
    f = ctx.actions.declare_file("plain.txt")
    return [ShapeInfo(name = d.basename, is_dir = str(d.is_directory), is_file = str(f.is_directory))]

mk = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
        "ShapeInfo",
    );
    assert_eq!(field_str(&p, "name"), "treeart");
    assert_eq!(field_str(&p, "is_dir"), "True");
    assert_eq!(field_str(&p, "is_file"), "False");
}

#[test]
fn expand_template_visible_via_target_actions() {
    let p = run_and_get_provider(
        r#"
SeenInfo = provider(fields = ["seen"])

def _leaf_impl(ctx):
    tmpl = ctx.actions.declare_file("tmpl.in")
    ctx.actions.write(tmpl, "hi {N}")
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.expand_template(template = tmpl, output = out, substitutions = {"{N}": "x"})
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    rows = []
    for a in ctx.attr.dep.actions:
        rows.append(a.mnemonic + "|" + (a.outputs[0].basename if a.outputs else ""))
    return [SeenInfo(seen = ";".join(rows))]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
        "SeenInfo",
    );
    assert_eq!(
        field_str(&p, "seen"),
        "FileWrite|tmpl.in;ExpandTemplate|out.txt"
    );
}

#[test]
fn args_add_and_add_all_flatten_into_run_argv() {
    let p = run_and_get_provider(
        r#"
ArgvInfo = provider(fields = ["argv"])

def _leaf_impl(ctx):
    tool = ctx.actions.declare_file("tool.bin")
    out  = ctx.actions.declare_file("out.bin")
    ctx.actions.write(tool, "tool")
    args = ctx.actions.args()
    args.add("--flag")
    args.add("--name", format = "name=%s")
    args.add_all(["a", "b"], format_each = "--in=%s")
    args.add_joined(["x", "y", "z"], join_with = ",")
    ctx.actions.run(
        executable = tool,
        arguments = [args, "trailing"],
        outputs = [out],
        mnemonic = "Run",
    )
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
        "ArgvInfo",
    );
    assert_eq!(
        field_str(&p, "argv"),
        "--flag,name=--name,--in=a,--in=b,x,y,z,trailing"
    );
}

#[test]
fn add_all_accepts_map_each_and_directory_kwargs() {
    // aspect_bazel_lib's mtree_spec passes
    // `add_all(values, map_each=fn, expand_directories=True,
    // uniquify=True)`. v1 doesn't apply map_each (no execution), but
    // the call must not error at the parse boundary.
    let p = run_and_get_provider(
        r#"
ArgvInfo = provider(fields = ["argv"])

def _expand(f):
    return [f]

def _leaf_impl(ctx):
    tool = ctx.actions.declare_file("tool.bin")
    out  = ctx.actions.declare_file("out.bin")
    ctx.actions.write(tool, "tool")
    args = ctx.actions.args()
    args.add_all(
        ["a", "b"],
        map_each = _expand,
        expand_directories = True,
        uniquify = True,
        omit_if_empty = False,
        terminate_with = "END",
    )
    ctx.actions.run(
        executable = tool,
        arguments = [args],
        outputs = [out],
        mnemonic = "Run",
    )
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
        "ArgvInfo",
    );
    // map_each is ignored — values pass through stringified — but
    // terminate_with does append.
    assert_eq!(field_str(&p, "argv"), "a,b,END");
}

#[test]
fn run_tools_kwarg_folds_into_inputs() {
    let p = run_and_get_provider(
        r#"
InfoInfo = provider(fields = ["count"])

def _leaf_impl(ctx):
    exe  = ctx.actions.declare_file("exe.bin")
    tool = ctx.actions.declare_file("tool.bin")
    out  = ctx.actions.declare_file("o.bin")
    ctx.actions.write(exe, "")
    ctx.actions.write(tool, "")
    ctx.actions.run(
        executable = exe,
        outputs = [out],
        tools = [tool],
        execution_requirements = {"no-sandbox": "1"},
        mnemonic = "Run",
    )
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    for a in ctx.attr.dep.actions:
        if a.mnemonic == "Run":
            return [InfoInfo(count = str(len(a.inputs)))]
    return [InfoInfo(count = "<missing>")]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
        "InfoInfo",
    );
    // tools=[tool.bin] folds into inputs — recorded action has 1 input.
    assert_eq!(field_str(&p, "count"), "1");
}

#[test]
fn runfiles_accepts_depset_for_transitive_files() {
    let p = run_and_get_provider(
        r#"
NamesInfo = provider(fields = ["names"])

def _impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    ctx.actions.write(a, "a")
    ctx.actions.write(b, "b")
    rf = ctx.runfiles(transitive_files = depset([a, b]))
    return [NamesInfo(names = ",".join([f.basename for f in rf.files]))]

mk = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
        "NamesInfo",
    );
    assert_eq!(field_str(&p, "names"), "a.txt,b.txt");
}
