//! Analyses a small rules_pkg workspace end-to-end.
//!
//! Guards three fixes the pkg_tar path surfaced: the `PyRuntimeInfo`
//! builtin global (rules_python's reexports.bzl captures it), the
//! `ctx.fragments.py.disallow_native_rules` fragment attr, and
//! `py_builtins.get_label_repo_runfiles_path` returning the
//! package-inclusive runfiles path (so rules_python's `get_imports`
//! doesn't `fail()` on an above-root path).
//!
//! `pkg_files` analyses fully. `pkg_tar` pulls a py_binary build tool, so
//! it can't fully analyse without the `@@bazel_tools//tools/python`
//! toolchain (downloaded-interpreter frontier — Loupe runs no module
//! extensions, so it faithfully raises rather than fabricating one).

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/rules_pkg_probe")
}

#[test]
#[ignore = "manual probe: needs live BCR access to materialize rules_pkg; run with `--ignored`"]
fn analyse_rules_pkg_source_tree() {
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
        .expect("Engine::build on rules_pkg fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on rules_pkg fixture");

    assert_eq!(result.package_errors.len(), 0, "package-load errors");

    // pkg_files analyses (no toolchain needed) — the PyRuntimeInfo /
    // disallow_native_rules / runfiles-path fixes are all on the path
    // that reaches it via the rules_python deps.
    assert!(
        result.analyzed.iter().any(|t| t.label.to_string().ends_with("//:files")),
        "pkg_files //:files did not analyse"
    );

    // Any remaining failure must be the python-toolchain frontier, not a
    // regression of the three fixes above.
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
