//! End-to-end: drive the analysis engine, harvest the recorded
//! [`Action`]s off an `AnalyzedTarget`, and feed each through
//! [`loupe_core::ac_key::compute_action_cache_key`].
//!
//! These tests close the loop on the protobuf encoding: real rule
//! impl → real recorded action → Bazel-identical digest, sourced
//! exclusively from Loupe's analysis pipeline. They bypass DICE and
//! build the `inputs_by_path` map directly from disk reads + recorded
//! Write contents — the DICE-routed production path lives in
//! `loupe-exec-cas` (`SourceFileDigestKey` composes `FileContentKey` +
//! `FileMetadataKey`, `OutputDigestKey` reflects upstream
//! Write/Symlink/ExpandTemplate content) and is exercised by that
//! crate's tests.

use std::collections::BTreeMap;
use std::fs;

use loupe_core::ac_key::{AcKeyError, Digest, compute_action_cache_key};
use loupe_core::action::{Action, ActionKind};
use loupe_core::starlark_types::file::File;
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

fn world_label() -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), "world")
}

/// Build the `inputs_by_path` map by reading each source input's bytes
/// from `workspace_root`. Mirrors what
/// `loupe-exec-cas::SourceFileDigestKey` does in the DICE-backed path
/// (minus the DICE caching) — for non-source inputs the caller layers
/// digests in on top (see [`pipelined_write_then_run_resolves_via_inputs_map`]).
fn source_inputs_from_disk(
    inputs: &[File],
    workspace_root: &std::path::Path,
) -> BTreeMap<String, (Digest, bool)> {
    inputs
        .iter()
        .filter(|f| f.is_source())
        .map(|f| {
            let bytes = fs::read(workspace_root.join(f.path()))
                .unwrap_or_else(|e| panic!("read {}: {e}", f.path()));
            (f.path(), (Digest::of_bytes(&bytes), false))
        })
        .collect()
}

#[test]
fn run_action_recorded_via_ctx_actions_produces_bazel_identical_key() {
    let tmp = tempfile::tempdir().unwrap();
    // A source file the rule references via attr — its bytes flow
    // into the input root via the on-disk map.
    fs::write(tmp.path().join("payload.txt"), b"hello, loupe\n").unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    out = ctx.actions.declare_file("out.bin")
    ctx.actions.run(
        executable = "/bin/echo",
        arguments = ["hello"],
        # `payload.txt` lives at the workspace root; loupe's source-
        # file auto-discovery synthesises the File when we name it
        # here via the typed attr default.
        inputs = ctx.files.srcs,
        outputs = [out],
        env = {"K": "V"},
        mnemonic = "Echo",
        progress_message = "echoing",
    )
    return []

echoer = rule(
    implementation = _impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
    },
)
"#,
        r#"
load(":defs.bzl", "echoer")
echoer(name = "world", srcs = ["payload.txt"])
"#,
    );
    let engine = Engine::new(tmp.path().to_path_buf(), Configuration::default()).unwrap();
    let analyzed = engine.analyze_target(&world_label()).unwrap();
    assert_eq!(analyzed.actions.len(), 1, "rule recorded one action");
    let action = &analyzed.actions[0];
    assert!(matches!(action.kind, ActionKind::Run { .. }));
    assert_eq!(action.inputs.len(), 1, "payload.txt flowed into inputs");
    assert!(action.inputs[0].is_source());

    let inputs_by_path = source_inputs_from_disk(&action.inputs, tmp.path());
    let cfg = analyzed.configuration.clone();
    let digest = compute_action_cache_key(action, &cfg, &inputs_by_path).expect("digest computes");
    // sha256-hex is 64 lowercase hex chars; size_bytes > 0.
    assert_eq!(digest.hash.len(), 64);
    assert!(digest.hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(digest.size_bytes > 0);
    // Determinism: same engine state → same digest.
    let again = compute_action_cache_key(action, &cfg, &inputs_by_path).expect("digest computes");
    assert_eq!(
        digest, again,
        "AC key is a pure function of action + input bytes"
    );

    // Mutating the source file flips the AC key. Rebuild the input
    // map from the updated disk state (mirrors what a DICE invalidation
    // event would trigger on a real `FileContentKey` watch).
    fs::write(tmp.path().join("payload.txt"), b"different bytes\n").unwrap();
    let inputs_after = source_inputs_from_disk(&action.inputs, tmp.path());
    let bumped =
        compute_action_cache_key(action, &cfg, &inputs_after).expect("digest still computes");
    assert_ne!(digest, bumped, "input content change must flip the AC key");
}

#[test]
fn pipelined_write_then_run_resolves_via_inputs_map() {
    // Two recorded actions:
    //   1. ctx.actions.write(gen.txt, "generated body")
    //   2. ctx.actions.run(executable=tool, inputs=[gen.txt], outputs=[final])
    //
    // Action #2's input is the output of action #1. Bazel hands the
    // executor a merkle tree containing the upstream output bytes;
    // loupe can do the same *without executing* because action #1 is
    // a `Write` — its content is recorded directly in
    // `ActionKind::Write { content }`. The caller pre-walks the local
    // action list and layers each Write/Symlink/ExpandTemplate output
    // digest into the `inputs_by_path` map; the downstream Run's AC
    // key then computes cleanly.
    //
    // This is the "no chicken-and-egg" pattern: spawn-less upstream
    // actions (Write/Symlink/ExpandTemplate) are content-derivable
    // from analysis alone; the AC key plumbing handles them in-engine.
    // Only upstream Run/RunShell outputs need execution (the
    // loupe-exec-cas boundary). The DICE-routed production path
    // replaces this hand-walk with `OutputDigestKey` so the
    // memoisation and parallelism fall out of DICE.

    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    gen = ctx.actions.declare_file("gen.txt")
    out = ctx.actions.declare_file("final.bin")
    ctx.actions.write(gen, "generated body")
    ctx.actions.run(
        executable = "/bin/cat",
        arguments = [],
        inputs = [gen],
        outputs = [out],
        env = {},
        mnemonic = "Cat",
    )
    return []

pipeliner = rule(implementation = _impl)
"#,
        r#"
load(":defs.bzl", "pipeliner")
pipeliner(name = "world")
"#,
    );
    let engine = Engine::new(tmp.path().to_path_buf(), Configuration::default()).unwrap();
    let analyzed = engine.analyze_target(&world_label()).unwrap();
    assert_eq!(analyzed.actions.len(), 2);

    // Topological pre-walk: harvest every Write/Symlink/ExpandTemplate
    // output digest into the input map.
    let inputs_by_path = harvest_generated(&analyzed.actions);
    assert!(inputs_by_path.contains_key(&analyzed.actions[0].outputs[0].path()));

    // Find the Run action — the cat that consumes the upstream Write.
    let cat_action = analyzed
        .actions
        .iter()
        .find(|a| matches!(a.kind, ActionKind::Run { .. }))
        .expect("one Run action");
    let cfg = analyzed.configuration.clone();
    let digest = compute_action_cache_key(cat_action, &cfg, &inputs_by_path).unwrap();
    assert_eq!(digest.hash.len(), 64);

    // Sanity: flipping the upstream Write's content (rebuilds the
    // input map with a different digest) flips the downstream
    // action's AC key — the cross-action plumbing works.
    let mut tweaked = inputs_by_path.clone();
    tweaked.insert(
        analyzed.actions[0].outputs[0].path(),
        (Digest::of_bytes(b"different generated body"), false),
    );
    let digest2 = compute_action_cache_key(cat_action, &cfg, &tweaked).unwrap();
    assert_ne!(digest, digest2);
}

#[test]
fn write_actions_explicitly_have_no_remote_cache_key() {
    // FileWriteAction is spawn-less; Bazel never sends it to a remote
    // executor. The AC-key API surfaces that boundary as a typed
    // error rather than silently returning a made-up digest. No input
    // resolution happens — `project_action` returns `NotSpawnAction`
    // before the input map is consulted — so an empty map is fine.
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(
        tmp.path(),
        r#"
def _impl(ctx):
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.write(out, "hi")
    return []

writer = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "writer")
writer(name = "world")"#,
    );
    let engine = Engine::new(tmp.path().to_path_buf(), Configuration::default()).unwrap();
    let analyzed = engine.analyze_target(&world_label()).unwrap();
    assert_eq!(analyzed.actions.len(), 1);
    let empty: BTreeMap<String, (Digest, bool)> = BTreeMap::new();
    let cfg = analyzed.configuration.clone();
    let err = compute_action_cache_key(&analyzed.actions[0], &cfg, &empty).unwrap_err();
    assert!(matches!(err, AcKeyError::NotSpawnAction(_)));
}

/// Harvest every `Write` output (path → content digest) from the
/// recorded action list. Mirrors the analysis-time pre-walk a non-DICE
/// consumer needs; the DICE-backed path replaces it with
/// `OutputDigestKey`.
fn harvest_generated(actions: &[Action]) -> BTreeMap<String, (Digest, bool)> {
    let mut out: BTreeMap<String, (Digest, bool)> = BTreeMap::new();
    for a in actions {
        if let ActionKind::Write {
            content,
            is_executable,
        } = &a.kind
        {
            let path = a.outputs.first().expect("Write has one output").path();
            out.insert(
                path,
                (Digest::of_bytes(content.as_bytes()), *is_executable),
            );
        }
    }
    out
}
