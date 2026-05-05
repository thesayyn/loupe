//! Integration tests for the PR-A toolchain capture path.
//!
//! Two pieces under test:
//!
//! - The native `toolchain(...)` rule records its `toolchain`,
//!   `toolchain_type`, `exec_compatible_with`, and
//!   `target_compatible_with` attrs onto the resulting
//!   `UnconfiguredTarget`. Resolution (PR B) reads these back.
//! - `register_toolchains(...)` in `MODULE.bazel` lands its label
//!   strings on `ModuleGraph::registered_toolchains`. The resolver
//!   walks that list at lookup time.

use std::fs;
use std::path::PathBuf;

use loupe_core::package::AttrValue;
use loupe_core::{CanonicalRepo, Configuration, Engine, PackageId, PackagePath};

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

#[test]
fn toolchain_native_rule_captures_resolution_attrs() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("BUILD.bazel"),
        r#"
toolchain(
    name = "tc",
    toolchain = ":impl",
    toolchain_type = "//tools:my_toolchain_type",
    exec_compatible_with = ["//constraints:linux", "//constraints:x86_64"],
    target_compatible_with = ["//constraints:linux"],
)
"#,
    )
    .unwrap();
    let pkg = engine(tmp.path().to_path_buf())
        .query_package(&PackageId::new(CanonicalRepo::MAIN, PackagePath::root()))
        .unwrap();
    let tc = pkg.targets.get("tc").expect("toolchain target recorded");
    assert_eq!(
        tc.attrs.get("toolchain"),
        Some(&AttrValue::String(":impl".to_string())),
    );
    assert_eq!(
        tc.attrs.get("toolchain_type"),
        Some(&AttrValue::String("//tools:my_toolchain_type".to_string(),)),
    );
    match tc.attrs.get("exec_compatible_with") {
        Some(AttrValue::List(xs)) => {
            let strs: Vec<String> = xs
                .iter()
                .map(|v| match v {
                    AttrValue::String(s) => s.clone(),
                    other => panic!("expected String, got {other:?}"),
                })
                .collect();
            assert_eq!(
                strs,
                vec![
                    "//constraints:linux".to_string(),
                    "//constraints:x86_64".to_string(),
                ],
            );
        }
        other => panic!("expected AttrValue::List, got {other:?}"),
    }
    match tc.attrs.get("target_compatible_with") {
        Some(AttrValue::List(xs)) => {
            assert_eq!(xs.len(), 1);
        }
        other => panic!("expected AttrValue::List, got {other:?}"),
    }
}

#[test]
fn toolchain_native_rule_omits_unset_attrs() {
    // Capture is by absence, not by sentinel — keeps the resolver's
    // match logic from having to distinguish "unset" from "empty".
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(tmp.path().join("BUILD.bazel"), r#"toolchain(name = "tc")"#).unwrap();
    let pkg = engine(tmp.path().to_path_buf())
        .query_package(&PackageId::new(CanonicalRepo::MAIN, PackagePath::root()))
        .unwrap();
    let tc = pkg.targets.get("tc").unwrap();
    assert!(!tc.attrs.contains_key("toolchain"));
    assert!(!tc.attrs.contains_key("toolchain_type"));
    assert!(!tc.attrs.contains_key("exec_compatible_with"));
    assert!(!tc.attrs.contains_key("target_compatible_with"));
}
