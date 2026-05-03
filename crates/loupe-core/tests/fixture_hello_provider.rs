//! Phase 2 deliverable: `Engine::list_package` on fixture #1.

use std::path::PathBuf;

use loupe_core::{CanonicalRepo, Configuration, Engine, PackageId, PackagePath};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("loupe-test-harness")
        .join("fixtures")
        .join("hello_provider")
}

#[test]
fn list_package_returns_world_target() {
    let engine = Engine::new(fixture_path(), Configuration).unwrap();
    let labels = engine
        .list_package(&PackageId::new(CanonicalRepo::MAIN, PackagePath::root()))
        .unwrap();
    assert_eq!(
        labels.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
        vec!["@@//:world"],
    );
}

#[test]
fn discover_packages_finds_top_level() {
    let engine = Engine::new(fixture_path(), Configuration).unwrap();
    let pkgs = engine.discover_packages().unwrap();
    let paths: Vec<_> = pkgs.iter().map(|p| p.path.as_str().to_string()).collect();
    assert_eq!(paths, vec![""]);
}

#[test]
fn module_graph_records_main_module_name() {
    let engine = Engine::new(fixture_path(), Configuration).unwrap();
    assert_eq!(engine.module_graph().main.name, "hello_provider");
    assert_eq!(engine.module_graph().main.version, "0.0.0");
}
