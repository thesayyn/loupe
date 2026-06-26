//! `filegroup` and `alias` native rule semantics — both follow their
//! `srcs` / `actual` references during analysis instead of returning
//! a synthetic single-File DefaultInfo.

use std::path::Path;

use loupe_core::engine::EngineBuilder;
use loupe_core::{CanonicalRepo, Configuration, Label, PackagePath};

fn build_engine(workspace: &Path) -> loupe_core::Engine {
    EngineBuilder::new(workspace.to_path_buf(), Configuration::default())
        .build()
        .expect("engine")
}

#[test]
fn filegroup_unions_srcs_default_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("MODULE.bazel"), "module(name = \"main\")\n").unwrap();
    std::fs::write(
        root.join("BUILD.bazel"),
        "filegroup(name = \"all\", srcs = [\"a.txt\", \"b.txt\"])\n",
    )
    .unwrap();
    let engine = build_engine(root);
    let label = Label::new(
        CanonicalRepo::main(),
        PackagePath::root(),
        "all".to_string(),
    );
    let analyzed = engine.analyze_target(&label).expect("filegroup analyses");
    let files: Vec<String> = analyzed.default_files().iter().map(|f| f.path()).collect();
    assert_eq!(files, vec!["a.txt".to_string(), "b.txt".to_string()]);
}

#[test]
fn alias_forwards_actual_providers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("MODULE.bazel"), "module(name = \"main\")\n").unwrap();
    std::fs::write(
        root.join("BUILD.bazel"),
        "filegroup(name = \"src\", srcs = [\"a.txt\"])\nalias(name = \"link\", actual = \":src\")\n",
    )
    .unwrap();
    let engine = build_engine(root);
    let alias_label = Label::new(
        CanonicalRepo::main(),
        PackagePath::root(),
        "link".to_string(),
    );
    let analyzed = engine.analyze_target(&alias_label).expect("alias analyses");
    let files: Vec<String> = analyzed.default_files().iter().map(|f| f.path()).collect();
    assert_eq!(files, vec!["a.txt".to_string()]);
    // Alias's own label is preserved on the AnalyzedTarget — only
    // the providers come from `actual`.
    assert_eq!(analyzed.label, alias_label);
}
