//! Analyses native genrule / filegroup / alias end-to-end.
//!
//! These need no external ruleset. They pin native-rule fidelity:
//!   genrule   -> ctx.actions.run -> "Genrule" mnemonic (regression guard)
//!   filegroup -> no actions, forwards srcs
//!   alias     -> forwards the actual target's actions/files

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/native_rules_probe")
}

#[test]
#[ignore = "manual probe: analyze_workspace needs bzlmod/BCR setup; run with `--ignored`"]
fn analyse_native_rules() {
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
        .expect("Engine::build on native_rules fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on native_rules fixture");

    assert_eq!(result.package_errors.len(), 0, "package-load errors");
    assert_eq!(
        result.failed.len(),
        0,
        "unexpected failures: {:?}",
        result.failed.iter().map(|f| &f.label).collect::<Vec<_>>()
    );

    let find = |suffix: &str| {
        result
            .analyzed
            .iter()
            .find(|t| t.label.to_string().ends_with(suffix))
            .unwrap_or_else(|| panic!("target {suffix} not analysed"))
    };
    let mnemonics =
        |suffix: &str| -> Vec<String> { find(suffix).actions.iter().filter_map(|a| a.mnemonic.clone()).collect() };

    assert_eq!(mnemonics("//:gen"), vec!["Genrule"]);
    // alias forwards the genrule's action.
    assert_eq!(mnemonics("//:al"), vec!["Genrule"]);
    // filegroup records no actions.
    assert!(find("//:fg").actions.is_empty());
}
