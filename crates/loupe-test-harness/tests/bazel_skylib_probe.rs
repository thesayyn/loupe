//! Analyses a small bazel_skylib workspace end-to-end.
//!
//! bazel_skylib's `write_file` / `copy_file` / `expand_template` are
//! pure-Starlark rules needing no downloaded toolchain, so they analyse
//! fully and pin the native-action mnemonics Loupe emits against the
//! ones `bazel aquery` reports:
//!   write_file      -> ctx.actions.write          -> FileWrite
//!   copy_file       -> ctx.actions.run_shell      -> CopyFile (skylib-set)
//!   expand_template -> ctx.actions.expand_template -> TemplateExpand
//!
//! `TemplateExpand` (not `ExpandTemplate`) is the regression this guards.

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/bazel_skylib_probe")
}

#[test]
#[ignore = "manual probe: needs live BCR access to materialize bazel_skylib; \
            run with `--ignored`"]
fn analyse_bazel_skylib_source_tree() {
    let workspace = fixture();
    assert!(
        workspace.join("MODULE.bazel").exists(),
        "fixture at {} missing MODULE.bazel",
        workspace.display()
    );

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
        .expect("Engine::build on bazel_skylib fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on bazel_skylib fixture");

    assert_eq!(result.package_errors.len(), 0, "package-load errors");
    assert_eq!(
        result.failed.len(),
        0,
        "unexpected analysis failures: {:?}",
        result.failed.iter().map(|f| &f.label).collect::<Vec<_>>()
    );

    let mnemonic_of = |suffix: &str| -> Vec<String> {
        result
            .analyzed
            .iter()
            .find(|t| t.label.to_string().ends_with(suffix))
            .unwrap_or_else(|| panic!("target {suffix} not analysed"))
            .actions
            .iter()
            .filter_map(|a| a.mnemonic.clone())
            .collect()
    };

    assert_eq!(mnemonic_of("//:gen_txt"), vec!["FileWrite"]);
    assert_eq!(mnemonic_of("//:copy_txt"), vec!["CopyFile"]);
    assert_eq!(mnemonic_of("//:tmpl"), vec!["TemplateExpand"]);
}
