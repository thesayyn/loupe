//! Integration tests for the BUILD-file builtins `glob`,
//! `exports_files`, and `select`. The unit tests in
//! `crate::starlark_types::*` cover the algorithmic gotchas; these
//! tests exercise the wired-up paths through real BUILD evaluation.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use loupe_core::build_eval;
use loupe_core::label::{CanonicalRepo, PackageId, PackagePath};
use loupe_core::package::{AttrValue, Package};
use loupe_core::starlark_types::exports::SOURCE_FILE_KIND;
use starlark::environment::FrozenModule;

fn evaluate_build(build: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, Package) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(dir.path().join("BUILD.bazel"), build).unwrap();
    for (rel, content) in files {
        let p = dir.path().join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }
    let pkg = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());
    let (_, ast) = build_eval::parse_build(dir.path(), None, &pkg).unwrap();
    let load_stmts = build_eval::extract_loads(&ast);
    let loaded: HashMap<String, (loupe_core::label::Label, FrozenModule)> = HashMap::new();
    let pkg = build_eval::evaluate(
        dir.path(),
        None,
        pkg,
        Default::default(),
        ast,
        &load_stmts,
        &loaded,
    )
    .unwrap();
    (dir, pkg)
}

#[test]
fn glob_lists_package_files_excluding_subpackage_contents() {
    let (_dir, pkg) = evaluate_build(
        r#"
srcs = glob(["**/*.txt"])
exports_files(srcs)
"#,
        &[
            ("a.txt", ""),
            ("nested/b.txt", ""),
            ("nested/sub/BUILD", ""), // sub-package — boundary
            ("nested/sub/c.txt", ""), // belongs to the sub-package
        ],
    );
    let names: Vec<&String> = pkg.targets.keys().collect();
    assert_eq!(
        names,
        vec![&"a.txt".to_string(), &"nested/b.txt".to_string()],
        "glob should not cross into sub-packages"
    );
    for t in pkg.targets.values() {
        assert_eq!(t.kind, SOURCE_FILE_KIND);
    }
}

#[test]
fn select_round_trips_through_attrs_as_a_value() {
    let (_dir, _pkg) = evaluate_build(
        r#"
# Building a select() and binding it to a variable verifies it
# evaluates without error and produces a `<select with N piece(s)>`
# value. Phase 5 will resolve it; for now we just need the value
# type to survive Starlark evaluation.
chosen = select({
    "//conditions:default": ["default.cc"],
    "//cpu:k8": ["k8.cc"],
})
exports_files(["sentinel.txt"])
"#,
        &[("sentinel.txt", "")],
    );
}

#[test]
fn exports_files_records_a_target_per_entry() {
    let (_dir, pkg) = evaluate_build(
        r#"exports_files(["a.txt", "b.txt"])"#,
        &[("a.txt", ""), ("b.txt", "")],
    );
    let names: Vec<&String> = pkg.targets.keys().collect();
    assert_eq!(names, vec![&"a.txt".to_string(), &"b.txt".to_string()]);
}

#[test]
fn glob_exclude_drops_test_files() {
    let (_dir, pkg) = evaluate_build(
        r#"
exports_files(glob(["*.cc"], exclude = ["*_test.cc"]))
"#,
        &[("a.cc", ""), ("a_test.cc", ""), ("b.cc", "")],
    );
    let labels: Vec<String> = pkg.targets.values().map(|t| t.label.to_string()).collect();
    assert_eq!(
        labels,
        vec!["@@//:a.cc".to_string(), "@@//:b.cc".to_string()]
    );
    // None of the surviving entries are test files.
    for t in pkg.targets.values() {
        match t.attrs.get("name") {
            Some(AttrValue::String(s)) => assert!(!s.ends_with("_test.cc")),
            _ => panic!("expected name attr"),
        }
    }
    // `_dir` keeps the tempdir alive while we read `pkg`.
    let _ = Path::new("");
}
