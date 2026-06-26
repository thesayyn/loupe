//! Analyses a small rules_python workspace end-to-end.
//!
//! The fixture (`fixtures/rules_python_probe`) pulls real rules_python
//! through a BCR `bazel_dep` and declares `py_library` / `py_binary` /
//! `py_test`. It exists to guard the CcInfo-merge path: `py_library`'s
//! `get_cc_info_for_library` calls `cc_common.merge_cc_infos`, whose
//! result is read back through `PyCcLinkParamsInfo` for its
//! `linking_context` field — so a regression there breaks `py_library`
//! analysis outright.
//!
//! `py_binary` / `py_test` are NOT expected to analyse: they require the
//! `@@rules_python+//python:toolchain_type` toolchain, registered via
//! `@pythons_hub` — a repo built by rules_python's `python` module
//! extension that downloads platform interpreters. Loupe runs no module
//! extensions, so it faithfully raises "no resolved toolchain" rather
//! than fabricating one.

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/rules_python_probe")
}

#[test]
#[ignore = "manual probe: needs live BCR access to materialize rules_python; \
            run with `--ignored`"]
fn analyse_rules_python_source_tree() {
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
        .expect("Engine::build on rules_python fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on rules_python fixture");

    eprintln!(
        "rules_python probe: {} analyzed, {} failed, {} package errors",
        result.analyzed.len(),
        result.failed.len(),
        result.package_errors.len()
    );
    for fe in &result.failed {
        eprintln!("  failed {}: {}", fe.label, fe.error.lines().next().unwrap_or(""));
    }

    assert_eq!(
        result.package_errors.len(),
        0,
        "package-load errors mean a BUILD/bzl couldn't even be parsed"
    );

    // py_library must analyse: this is the cc_common.merge_cc_infos /
    // PyCcLinkParamsInfo.linking_context regression guard.
    assert!(
        result
            .analyzed
            .iter()
            .any(|t| t.label.to_string().ends_with("//:lib")),
        "py_library //:lib did not analyse — the CcInfo merge path regressed"
    );

    // Every remaining failure must be the python-toolchain frontier, not
    // a new gap. If py_binary/py_test ever start failing for another
    // reason, this catches it.
    for fe in &result.failed {
        assert!(
            fe.error.contains("no resolved toolchain")
                && fe.error.contains("python:toolchain_type"),
            "unexpected failure on {}: {}",
            fe.label,
            fe.error
        );
    }
}
