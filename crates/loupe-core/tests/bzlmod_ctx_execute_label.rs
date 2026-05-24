//! `ctx.execute([Label(...), …])`: vendored repo_rules in
//! `rules_kotlin` / `rules_rust` / others pass `Label(...)` directly
//! inside the `arguments` list to `ctx.execute`. Bazel resolves each
//! Label to its on-disk path via `StarlarkBaseExternalContext`'s
//! label-to-arg conversion; Loupe used to reject with `list entry must
//! be a string or path, got Label`.
//!
//! Pattern mirrors `bzlmod_ctx_path_label`: `rule_a` writes a script,
//! `rule_b` invokes it via `ctx.execute([..., Label("@dep//:tool.sh")])`.

use std::collections::BTreeMap;

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctx_execute_accepts_label_entries_in_argv() {
    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r##"module(name = "demo", version = "0.0.0")
bazel_dep(name = "dep", version = "1.0")
bazel_dep(name = "ext", version = "1.0")
"##,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("defs.bzl"),
        r##"
def _rule_a_impl(ctx):
    ctx.file("tool.sh", "#!/bin/sh\nprintf hello-from-label\n", executable = True)
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

def _rule_b_impl(ctx):
    res = ctx.execute(["/bin/sh", Label("@dep//:tool.sh")])
    if res.return_code != 0:
        fail("subprocess failed: %s" % res.stderr)
    ctx.file("payload.txt", res.stdout)
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

rule_a = repository_rule(implementation = _rule_a_impl, attrs = {})
rule_b = repository_rule(implementation = _rule_b_impl, attrs = {})
"##,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let workspace_path = workspace.path().to_path_buf();
    let resolver_engine = tokio::task::spawn_blocking({
        let p = workspace_path.clone();
        move || EngineBuilder::new(p, Configuration::default()).build()
    })
    .await
    .unwrap()
    .unwrap();

    let defining_bzl = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "defs.bzl");
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

    let payload = std::fs::read_to_string(cache.path().join("ext+/payload.txt")).unwrap();
    assert_eq!(payload, "hello-from-label");
}
