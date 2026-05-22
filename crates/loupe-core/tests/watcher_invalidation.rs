//! End-to-end check that `WorkspaceWatcher` + `Engine::poll_invalidations`
//! actually drives DICE invalidation when a workspace file changes.
//!
//! Setup: a tiny workspace with one rule, one target. First analyze.
//! Edit the BUILD file. Poll. Re-analyze and assert the new target
//! shows up — that requires `PackageKey` to have re-run, which only
//! happens if the underlying `FileContentKey(<BUILD>)` was
//! invalidated by the watcher.
//!
//! `notify`'s recommended backend is platform-dependent (inotify on
//! Linux, kqueue on macOS, ReadDirectoryChangesW on Windows). Events
//! may take a few ms to propagate after a write; the test retries the
//! poll a handful of times with short sleeps to handle that. Marked
//! `#[ignore]` because the underlying notify backend is flaky in
//! constrained CI containers (inotify watch descriptor limits, kernel
//! version quirks); the logic is exercised by the per-classify unit
//! tests in `src/watch.rs`.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn write(dir: &Path, path: &str, contents: &str) {
    let full = dir.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(full, contents).unwrap();
}

#[test]
#[ignore = "notify backend is flaky in CI containers; logic is covered by unit tests in src/watch.rs"]
fn editing_a_build_file_invalidates_via_watcher() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write(
        dir,
        "MODULE.bazel",
        r#"module(name = "demo", version = "0.0.0")"#,
    );
    write(
        dir,
        "defs.bzl",
        r#"
Info = provider(fields = ["name"])
def _impl(ctx):
    return [Info(name = ctx.attr.name)]
my_rule = rule(implementation = _impl)
"#,
    );
    write(
        dir,
        "BUILD.bazel",
        r#"
load(":defs.bzl", "my_rule")
my_rule(name = "first")
"#,
    );

    let engine = EngineBuilder::new(dir.to_path_buf(), Configuration::default())
        .with_file_watcher(true)
        .build()
        .unwrap();

    // Cycle 1: only `first` exists.
    let initial = engine.analyze_workspace(&[]).unwrap();
    let labels: Vec<String> = initial
        .analyzed
        .iter()
        .map(|t| t.label.to_string())
        .collect();
    assert!(
        labels.iter().any(|l| l.ends_with(":first")),
        "first cycle should see :first, got {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l.ends_with(":second")),
        "first cycle should not see :second, got {labels:?}"
    );

    // Edit BUILD: add a second target.
    write(
        dir,
        "BUILD.bazel",
        r#"
load(":defs.bzl", "my_rule")
my_rule(name = "first")
my_rule(name = "second")
"#,
    );

    // Drain watcher events (retry — notify events take a few ms to
    // surface after the write).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let n = engine.poll_invalidations().unwrap();
        if n > 0 {
            break;
        }
        if Instant::now() > deadline {
            panic!("watcher never saw the edit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Cycle 2: now `:second` should be visible. PackageKey re-ran
    // because its FileContentKey input was invalidated by the watcher.
    let after = engine.analyze_workspace(&[]).unwrap();
    let labels: Vec<String> = after.analyzed.iter().map(|t| t.label.to_string()).collect();
    assert!(
        labels.iter().any(|l| l.ends_with(":second")),
        "post-edit cycle should see :second, got {labels:?}"
    );
}

#[test]
fn poll_invalidations_is_noop_without_watcher() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write(
        dir,
        "MODULE.bazel",
        r#"module(name = "demo", version = "0.0.0")"#,
    );
    write(dir, "BUILD.bazel", "");
    let engine = EngineBuilder::new(dir.to_path_buf(), Configuration::default())
        .build()
        .unwrap();
    // No `with_file_watcher` — poll should return 0 and not error.
    assert_eq!(engine.poll_invalidations().unwrap(), 0);
}
