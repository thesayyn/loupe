//! Regression test: `glob()` in a main-repo package must read the
//! workspace, not a registered external root.
//!
//! When a bzlmod cache is configured, the engine registers a
//! `RepoRoot::Filesystem(cache)` in the external-root list. The main
//! repo's canonical is the empty string, and `Filesystem(cache).has("")`
//! used to be `true` (`cache.join("").is_dir()`), so a main-repo `glob()`
//! was mis-routed to walk the cache directory and returned nothing —
//! e.g. aspect bazel-lib's `write_source_file` then reported checked-in
//! source files as missing and emitted `fail_with_message_test`.

use std::fs;

use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, PackageId, PackagePath};

#[test]
fn main_repo_glob_reads_workspace_even_with_bzlmod_cache_registered() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let cache = tmp.path().join("cache");
    fs::create_dir_all(&cache).unwrap();
    fs::create_dir_all(ws.join("js/private/coverage/bundle")).unwrap();

    fs::write(
        ws.join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(ws.join("BUILD.bazel"), "").unwrap();

    // Package mirrors rules_js's `js/private/coverage`: a globbable file
    // at the package root plus a `bundle/` subpackage that the glob must
    // skip. Name a target after each globbed file so the load-phase glob
    // result is observable through `list_package`.
    fs::write(
        ws.join("js/private/coverage/BUILD.bazel"),
        r#"[filegroup(name = f) for f in glob(["*.js"])]"#,
    )
    .unwrap();
    fs::write(ws.join("js/private/coverage/coverage.js"), "// content\n").unwrap();
    fs::write(ws.join("js/private/coverage/bundle/BUILD.bazel"), "").unwrap();
    fs::write(ws.join("js/private/coverage/bundle/bundle.js"), "// nope\n").unwrap();

    // `with_bzlmod_cache` is what registers the competing
    // `RepoRoot::Filesystem(cache)` — without it the bug can't surface.
    let engine = Engine::builder(ws, Configuration::default())
        .with_bzlmod_cache(cache)
        .build()
        .expect("engine builds");

    let pkg = PackageId::new(
        CanonicalRepo::main(),
        PackagePath::new("js/private/coverage").unwrap(),
    );
    let labels = engine.list_package(&pkg).expect("package loads");
    let names: Vec<&str> = labels.iter().map(|l| l.name()).collect();

    assert!(
        names.contains(&"coverage.js"),
        "glob([\"*.js\"]) must find the package-root file; got targets {names:?}",
    );
    // The subpackage's file lives in a separate package and must not leak
    // into the parent's glob.
    assert!(
        !names.contains(&"bundle.js"),
        "glob must not cross the `bundle/` subpackage boundary; got {names:?}",
    );
}
