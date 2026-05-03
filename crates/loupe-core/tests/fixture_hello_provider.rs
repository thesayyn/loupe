//! End-to-end fixture #1 tests.
//!
//! - Phase 2: `Engine::list_package` returns `[@@//:world]`.
//! - Phase 3: the captured target's `kind` matches the BUILD-side
//!   import alias (`hello`), and `attr.string`'s default is applied
//!   (greeting = `"hello, world"` in BUILD; default would be `"hello"`).

use std::path::PathBuf;

use loupe_core::package::AttrValue;
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
    let graph = engine.module_graph().unwrap();
    assert_eq!(graph.main.name, "hello_provider");
    assert_eq!(graph.main.version, "0.0.0");
}

#[test]
fn captured_target_carries_real_kind_and_attrs() {
    // Phase 3: the recorded `UnconfiguredTarget` reflects the real
    // `rule()`/`attr.string()` definitions, not Phase 2's placeholder.
    let engine = Engine::new(fixture_path(), Configuration).unwrap();
    let pkg = engine
        .query_package(&PackageId::new(CanonicalRepo::MAIN, PackagePath::root()))
        .unwrap();
    let target = pkg
        .targets
        .get("world")
        .expect("PackageKey should produce target `world`");
    assert_eq!(target.label.to_string(), "@@//:world");
    assert_eq!(target.kind, "hello", "kind should match BUILD import alias");
    assert_eq!(target.bzl_source, ":defs.bzl");
    assert_eq!(
        target.attrs.get("greeting"),
        Some(&AttrValue::String("hello, world".into())),
        "greeting kwarg should round-trip from BUILD"
    );
    assert_eq!(
        target.attrs.get("name"),
        Some(&AttrValue::String("world".into())),
        "name should be recorded"
    );
}
