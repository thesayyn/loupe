//! A transition writing `//command_line_option:cpu` must change the
//! `bazel-out/<cpu>-<mode>/bin` output segment.
//!
//! This guards the build-setting-key normalisation: transitions name
//! settings by full label (`//command_line_option:cpu`) while Loupe keys
//! `build_settings` (and `derive_output_segment`) by the bare name
//! (`cpu`). `:a` carries a rule(cfg=) setting cpu=arm64 -> arm64-fastbuild;
//! `:b` is plain -> k8-fastbuild. Offline (no external deps).

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/clo_transition_probe")
}

#[test]
fn command_line_option_transition_changes_output_segment() {
    let workspace = fixture();
    let engine = EngineBuilder::new(workspace.clone(), Configuration::default())
        .with_bzlmod_cache(workspace.join(".loupe").join("bzlmod"))
        .build()
        .expect("Engine::build on clo fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on clo fixture");

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
    assert_eq!(result.failed.len(), 0, "unexpected analysis failures");

    let seg_of = |suffix: &str| -> String {
        result
            .analyzed
            .iter()
            .find(|t| t.label.to_string().ends_with(suffix))
            .unwrap_or_else(|| panic!("{suffix} not analysed"))
            .default_files()
            .first()
            .map(|f| f.path())
            .unwrap_or_default()
    };

    assert!(
        seg_of("//:a").contains("/arm64-fastbuild/"),
        "transitioned cpu should change segment: {}",
        seg_of("//:a")
    );
    assert!(
        seg_of("//:b").contains("/k8-fastbuild/"),
        "untransitioned stays k8: {}",
        seg_of("//:b")
    );
}
