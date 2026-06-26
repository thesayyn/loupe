//! Exercises user-defined configuration transitions end-to-end.
//!
//! The fixture declares `to_prod = transition(...)` setting `//:flavor`
//! to `"prod"`, a `printer` rule that attaches it as a rule-level
//! incoming transition (`rule(cfg = to_prod)`), and a `no_transition`
//! rule that doesn't. Both pick their output file via
//! `select({":is_prod": "prod.txt", ...})`.
//!
//! With the transition firing, `:p` analyses under flavor=prod so its
//! select resolves to `prod.txt`; `:n` stays flavor=dev -> `dev.txt`.
//! This covers transition() returning a real value, rule(cfg=) capture
//! and application, and the build-setting-key normalisation that lets
//! the transitioned `//:flavor` reach the config_setting flag lookup.

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/transition_probe")
}

#[test]
#[ignore = "manual probe: needs live BCR access to materialize bazel_skylib; \
            run with `--ignored`"]
fn rule_incoming_transition_flips_select() {
    let workspace = fixture();
    let host_platform = loupe_core::Label::resolve(
        "@@platforms//host:host",
        &loupe_labels::PackageId::new(
            loupe_labels::CanonicalRepo::main(),
            loupe_labels::PackagePath::root(),
        ),
        &Default::default(),
    )
    .expect("resolving @@platforms//host:host");
    let config = Configuration::default().with_target_platform(host_platform);
    let engine = EngineBuilder::new(workspace.clone(), config)
        .with_bzlmod_cache(workspace.join(".loupe").join("bzlmod"))
        .with_bcr()
        .build()
        .expect("Engine::build on transition fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on transition fixture");

    assert_eq!(result.package_errors.len(), 0, "package-load errors");
    assert_eq!(
        result.failed.len(),
        0,
        "unexpected failures: {:?}",
        result.failed.iter().map(|f| &f.label).collect::<Vec<_>>()
    );

    let output_of = |suffix: &str| -> String {
        let t = result
            .analyzed
            .iter()
            .find(|t| t.label.to_string().ends_with(suffix))
            .unwrap_or_else(|| panic!("target {suffix} not analysed"));
        let files: Vec<String> = t
            .actions
            .iter()
            .flat_map(|a| a.outputs.iter().map(|f| f.path()))
            .collect();
        assert_eq!(files.len(), 1, "{suffix} should have one output: {files:?}");
        files.into_iter().next().unwrap()
    };

    // The incoming transition flipped //:flavor=prod, so :p's select
    // resolves to prod.txt; :n has no transition and stays dev.txt.
    assert!(
        output_of("//:p").ends_with("/prod.txt"),
        "got {}",
        output_of("//:p")
    );
    assert!(
        output_of("//:n").ends_with("/dev.txt"),
        "got {}",
        output_of("//:n")
    );
}
