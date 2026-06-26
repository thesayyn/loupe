//! Exercises the Pattern-B channel actor for `ctx.path(Label(...))`.
//!
//! Two Starlark `repository_rule`s: `rule_a` writes a file, `rule_b`
//! reads that file via `ctx.path(Label("@dep//:foo.txt")).read()`.
//! When `rule_b` materialises, its eval thread blocks on the channel
//! resolver, which causes `MaterializedRepoKey::compute`'s select loop
//! to recursively materialise `rule_a` via DICE, hand the path back,
//! and continue `rule_b`'s eval.
//!
//! The marker for `rule_b` should record:
//! 1. A `RepoMapping` entry (`@@ext+ : dep → dep+`).
//! 2. A `File` entry for the resolved path inside `dep+/foo.txt`.

use std::collections::BTreeMap;

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctx_path_label_resolves_via_channel_actor() {
    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "dep", version = "1.0")
bazel_dep(name = "ext", version = "1.0")
"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("defs.bzl"),
        r#"
def _rule_a_impl(ctx):
    ctx.file("foo.txt", "from-dep")
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

def _rule_b_impl(ctx):
    p = ctx.path(Label("@dep//:foo.txt"))
    content = p.read()
    ctx.file("from_dep.txt", content)
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

rule_a = repository_rule(implementation = _rule_a_impl, attrs = {})
rule_b = repository_rule(implementation = _rule_b_impl, attrs = {})
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let workspace_path = workspace.path().to_path_buf();

    // Stage 1: resolve both rule symbols + build their specs through
    // a temporary engine. The engine sees `bazel_dep(name="dep", ...)`,
    // so its main-repo effective mapping puts "dep" → "dep+".
    let resolver_engine = tokio::task::spawn_blocking({
        let p = workspace_path.clone();
        move || EngineBuilder::new(p, Configuration::default()).build()
    })
    .await
    .unwrap()
    .unwrap();

    let defining_bzl = Label::new(CanonicalRepo::main(), PackagePath::root(), "defs.bzl");
    let bzl_a = defining_bzl.clone();
    let bzl_b = defining_bzl.clone();
    let (spec_a, spec_b) = tokio::task::spawn_blocking(move || {
        let a = resolver_engine
            .build_starlark_repo_spec("dep+".to_string(), &bzl_a, "rule_a", BTreeMap::new())
            .unwrap();
        let b = resolver_engine
            .build_starlark_repo_spec("ext+".to_string(), &bzl_b, "rule_b", BTreeMap::new())
            .unwrap();
        (a, b)
    })
    .await
    .unwrap();

    // Stage 2: production engine with both specs registered. Triggering
    // list_package on "ext+" forces rule_b's materialisation, which
    // recursively forces rule_a via the channel actor.
    let mut specs = BTreeMap::new();
    specs.insert("dep+".to_string(), spec_a);
    specs.insert("ext+".to_string(), spec_b);
    let cache_path = cache.path().to_path_buf();
    let prod_engine = tokio::task::spawn_blocking(move || {
        EngineBuilder::new(workspace_path, Configuration::default())
            .with_bzlmod_cache(cache_path)
            .with_repo_specs(specs)
            .build()
    })
    .await
    .unwrap()
    .unwrap();

    let pkg = PackageId::new(CanonicalRepo::new("ext+"), PackagePath::root());
    let labels = tokio::task::spawn_blocking(move || prod_engine.list_package(&pkg))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        labels.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
        vec!["@@ext+//:all"],
    );

    assert_eq!(
        std::fs::read_to_string(cache.path().join("ext+/from_dep.txt")).unwrap(),
        "from-dep",
        "rule_b should have read rule_a's foo.txt content via Label resolution"
    );

    let marker = std::fs::read_to_string(cache.path().join("ext+.marker")).unwrap();
    assert!(
        marker
            .lines()
            .any(|l| l.starts_with("REPO_MAPPING:") && l.contains("dep")),
        "marker should record the dep→dep+ mapping observation; got:\n{marker}"
    );
    assert!(
        marker
            .lines()
            .any(|l| l.starts_with("FILE:") && l.contains("foo.txt")),
        "marker should record the FILE observation for the resolved path; got:\n{marker}"
    );
}
