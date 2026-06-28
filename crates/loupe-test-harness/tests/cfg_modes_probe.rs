//! Pins per-attr `cfg=` config derivation to the right output segment.
//!
//! A `parent` rule depends on the same kind of leaf four ways:
//!   cfg="target"            -> parent config       -> k8-fastbuild
//!   cfg="exec"              -> exec transition      -> k8-opt-exec-<hash>
//!   cfg=config.none()       -> null configuration   -> none
//!   cfg=config.unconfigured -> build settings dropped -> k8-fastbuild
//!
//! No external deps, so this runs offline (no BCR).

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/cfg_modes_probe")
}

#[test]
fn cfg_modes_derive_expected_segments() {
    let workspace = fixture();
    // Point the bzlmod cache at `.loupe/bzlmod` so package discovery
    // skips it (a stray cache from a manual `loupe cquery` run would
    // otherwise be walked as workspace packages). No BCR / external deps.
    let engine = EngineBuilder::new(workspace.clone(), Configuration::default())
        .with_bzlmod_cache(workspace.join(".loupe").join("bzlmod"))
        .build()
        .expect("Engine::build on cfg_modes fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on cfg_modes fixture");

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
        result.failed.iter().map(|f| &f.label).collect::<Vec<_>>()
    );

    let parent = result
        .analyzed
        .iter()
        .find(|t| t.label.to_string().ends_with("//:p"))
        .expect("//:p not analysed");
    let paths: Vec<String> = parent.default_files().iter().map(|f| f.path()).collect();

    let has = |needle: &str| paths.iter().any(|p| p.contains(needle));
    assert!(has("/k8-fastbuild/bin/lt.txt"), "target cfg: {paths:?}");
    assert!(has("-opt-exec-"), "exec cfg should produce opt-exec segment: {paths:?}");
    assert!(has("/le.txt"), "exec leaf forwarded: {paths:?}");
    assert!(has("/none/bin/ln.txt"), "config.none() -> null cfg: {paths:?}");
    assert!(has("/k8-fastbuild/bin/lu.txt"), "unconfigured: {paths:?}");
}
