//! `ctx.actions.declare_shareable_artifact` on a root-package main-repo
//! target must yield a clean `bazel-out/<cfg>/bin/<name>` path.
//!
//! Regression: the wrapper built the prefix as `workspace_root + "/"`,
//! which is "/" for the main repo, so a valid call either `fail()`ed
//! (path doesn't start with "/") or produced `bin//name`.
//! Offline (no external deps).

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/declare_shareable_probe")
}

#[test]
fn declare_shareable_artifact_root_package() {
    let workspace = fixture();
    let engine = EngineBuilder::new(workspace.clone(), Configuration::default())
        .with_bzlmod_cache(workspace.join(".loupe").join("bzlmod"))
        .build()
        .expect("Engine::build on declare_shareable fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on declare_shareable fixture");

    assert_eq!(
        result.package_errors.len(),
        0,
        "package-load errors: {:?}",
        result
            .package_errors
            .iter()
            .map(|e| format!("{}: {}", e.package, e.error))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        result.failed.len(),
        0,
        "unexpected failures: {:?}",
        result.failed.iter().map(|f| (&f.label, &f.error)).collect::<Vec<_>>()
    );

    let t = result
        .analyzed
        .iter()
        .find(|t| t.label.to_string().ends_with("//:root_pkg"))
        .expect("//:root_pkg not analysed");
    let paths: Vec<String> = t.default_files().iter().map(|f| f.path()).collect();
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert!(
        paths[0].ends_with("/bin/gen.txt") && !paths[0].contains("//bin") && !paths[0].contains("bin//"),
        "clean shareable-artifact path expected, got {}",
        paths[0]
    );
}
