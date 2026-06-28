//! Pins native-action mnemonic/shape fidelity for `ctx.actions.*`:
//!   symlink            -> FileWrite (the src) + Symlink
//!   run_shell (no mnemonic) -> "Action" (Bazel's default, never empty)
//!   declare_directory + run_shell(mnemonic="MakeDir") -> tree artifact
//!
//! Offline (no external deps).

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/action_fidelity_probe")
}

#[test]
fn native_action_mnemonics() {
    let workspace = fixture();
    let engine = EngineBuilder::new(workspace.clone(), Configuration::default())
        .with_bzlmod_cache(workspace.join(".loupe").join("bzlmod"))
        .build()
        .expect("Engine::build on action_fidelity fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on action_fidelity fixture");

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

    let mnemonics = |suffix: &str| -> Vec<String> {
        result
            .analyzed
            .iter()
            .find(|t| t.label.to_string().ends_with(suffix))
            .unwrap_or_else(|| panic!("{suffix} not analysed"))
            .actions
            .iter()
            .filter_map(|a| a.mnemonic.clone())
            .collect()
    };

    assert_eq!(mnemonics("//:sl"), vec!["FileWrite", "Symlink"]);
    // run_shell with no mnemonic -> Bazel's default "Action", not "".
    assert_eq!(mnemonics("//:rs"), vec!["Action"]);
    assert_eq!(mnemonics("//:dm"), vec!["MakeDir"]);
}
