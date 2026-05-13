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
use loupe_core::{CanonicalRepo, Configuration, Engine, Label, PackageId, PackagePath};

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
        Some(&AttrValue::Label(Label::new(
            CanonicalRepo::MAIN,
            PackagePath::root(),
            "impl",
        ))),
    );
    assert_eq!(
        tc.attrs.get("toolchain_type"),
        Some(&AttrValue::Label(Label::new(
            CanonicalRepo::MAIN,
            PackagePath::new("tools").unwrap(),
            "my_toolchain_type",
        ))),
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
fn toolchain_native_rule_defaults_unset_constraint_attrs_to_empty() {
    // Unset `exec_compatible_with` / `target_compatible_with` are
    // captured as empty label lists by the rule's universal-kwarg
    // path. The resolver treats an empty constraint list as "no
    // constraints" — semantically equivalent to absence.
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
)
"#,
    )
    .unwrap();
    let pkg = engine(tmp.path().to_path_buf())
        .query_package(&PackageId::new(CanonicalRepo::MAIN, PackagePath::root()))
        .unwrap();
    let tc = pkg.targets.get("tc").unwrap();
    assert_eq!(
        tc.attrs.get("exec_compatible_with"),
        Some(&AttrValue::LabelList(Vec::new())),
    );
    assert_eq!(
        tc.attrs.get("target_compatible_with"),
        Some(&AttrValue::LabelList(Vec::new())),
    );
}
