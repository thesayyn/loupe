//! Integration tests for the post-stop-gate Phase 5 ctx surface:
//! `ctx.actions.*`, `ctx.runfiles(...)`, and the `// STUB:` shims.
//!
//! Each test stands up a tiny synthetic workspace with one rule that
//! exercises the API and inspects the resulting `AnalyzedTarget`.

use std::fs;
use std::path::PathBuf;

use loupe_core::action::ActionKind;
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

fn world_label() -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), "world")
}

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
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
        "bazel-out/fake-cfg/bin/greeting.txt",
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

    let a = &analyzed.actions[0];
    match &a.kind {
        ActionKind::RunShell { command, arguments } => {
            assert_eq!(command, "echo $1 > $@");
            assert_eq!(arguments, &vec!["greeting".to_string()]);
        }
        other => panic!("expected RunShell, got {other:?}"),
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
    return [HelloInfo(count = str(len(rf.files)))]

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
    use loupe_core::AttrValue;
    assert_eq!(
        p.fields.get("count"),
        Some(&AttrValue::String("2".into())),
        "rf.files surfaces both direct entries"
    );
}

#[test]
fn ctx_bin_dir_and_genfiles_dir_use_active_config_segment() {
    // STUB: bin_dir / genfiles_dir are strings in v1, not the
    // struct-with-`.path` Bazel surfaces. Lock the format so a
    // ruleset relying on it sees a stable shape until the upgrade.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
HelloInfo = provider(fields = ["bin", "gen"])

def _impl(ctx):
    return [HelloInfo(bin = ctx.bin_dir, gen = ctx.genfiles_dir)]

dirs_rule = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "dirs_rule")
dirs_rule(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();
    let p = &analyzed.providers[0];
    use loupe_core::AttrValue;
    assert_eq!(
        p.fields.get("bin"),
        Some(&AttrValue::String("bazel-out/fake-cfg/bin".into()))
    );
    assert_eq!(
        p.fields.get("gen"),
        Some(&AttrValue::String("bazel-out/fake-cfg/bin".into())),
        "v1 collapses genfiles into bin/, matching modern Bazel"
    );
}

#[test]
fn ctx_stubs_return_empty_collections() {
    // STUB inventory: every empty-collection shim. If a fix changes
    // any of these to non-empty, this test surfaces it.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
HelloInfo = provider(fields = ["files", "file", "executable", "toolchains", "fragments", "var", "configuration"])

def _impl(ctx):
    return [HelloInfo(
        files = str(dir(ctx.files)),
        file = str(dir(ctx.file)),
        executable = str(dir(ctx.executable)),
        toolchains = str(len(ctx.toolchains)),
        fragments = str(dir(ctx.fragments)),
        var = str(len(ctx.var)),
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
    use loupe_core::AttrValue;
    let s = |name: &str| match p.fields.get(name) {
        Some(AttrValue::String(s)) => s.clone(),
        other => panic!("{name}: {other:?}"),
    };
    assert_eq!(s("files"), "[]");
    assert_eq!(s("file"), "[]");
    assert_eq!(s("executable"), "[]");
    assert_eq!(s("toolchains"), "0");
    assert_eq!(s("fragments"), "[]");
    assert_eq!(s("var"), "0");
    assert_eq!(s("configuration"), "None");
}

#[test]
fn ctx_expand_location_passes_through_in_v1() {
    // STUB: returns input unchanged. Real $(location ...)
    // substitution requires the configured-target graph.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
HelloInfo = provider(fields = ["s"])

def _impl(ctx):
    s = ctx.expand_location("run $(location //:nope)", targets = [])
    return [HelloInfo(s = s)]

expand_rule = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "expand_rule")
expand_rule(name = "world")"#,
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&world_label())
        .unwrap();
    use loupe_core::AttrValue;
    assert_eq!(
        analyzed.providers[0].fields.get("s"),
        Some(&AttrValue::String("run $(location //:nope)".into()))
    );
}
