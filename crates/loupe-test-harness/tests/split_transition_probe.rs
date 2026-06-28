//! Exercises a split (1:N) configuration transition.
//!
//! `split_cpu = transition(...)` returns a `dict[str, dict]` setting
//! `//:cpu_flag` to two different values. A `collector` rule attaches it
//! to a single `attr.label(cfg = split_cpu)` dep (`:l`). The dep fans
//! out into one configured target per split config, surfaced as a list:
//! `:l` resolves its `select()` to `arm.txt` under one config and
//! `x86.txt` under the other, so the collector aggregates both.

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/split_transition_probe")
}

#[test]
#[ignore = "manual probe: needs live BCR access to materialize bazel_skylib; \
            run with `--ignored`"]
fn split_transition_fans_dep_into_list() {
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
        .expect("Engine::build on split fixture");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on split fixture");

    assert_eq!(result.package_errors.len(), 0, "package-load errors");
    assert_eq!(
        result.failed.len(),
        0,
        "unexpected failures: {:?}",
        result.failed.iter().map(|f| &f.label).collect::<Vec<_>>()
    );

    let collector = result
        .analyzed
        .iter()
        .find(|t| t.label.to_string().ends_with("//:c"))
        .expect("//:c not analysed");
    let mut outputs: Vec<String> = collector.default_files().iter().map(|f| f.path()).collect();
    outputs.sort();
    // The single split dep produced two configured targets, each picking
    // a different select() branch.
    assert_eq!(outputs.len(), 2, "expected two split outputs, got {outputs:?}");
    assert!(outputs[0].ends_with("/arm.txt"), "got {outputs:?}");
    assert!(outputs[1].ends_with("/x86.txt"), "got {outputs:?}");
}
