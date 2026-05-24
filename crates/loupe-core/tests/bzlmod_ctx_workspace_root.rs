//! `repository_ctx.workspace_root`: vendored repo_rules
//! (`aspect_tools_telemetry`'s `tel_repository`, `rules_python`'s
//! `whl_library`, anything that probes the user's source tree) read
//! this attribute. Loupe used to raise `no such attribute`; the
//! engine now threads the user's workspace_root through DiceData
//! into the `RepoCtxState` and surfaces it as a `RepoPath`.

use std::collections::BTreeMap;

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_workspace_root_returns_main_module_source_tree() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
"#,
    )
    .unwrap();
    // Sentinel under the workspace root that the rule reads via
    // workspace_root/sentinel.txt to prove the path resolves.
    std::fs::write(workspace.path().join("sentinel.txt"), "from-workspace").unwrap();
    std::fs::write(
        workspace.path().join("defs.bzl"),
        r#"
def _impl(ctx):
    root = ctx.workspace_root
    content = root.get_child("sentinel.txt").read()
    if content != "from-workspace":
        fail("expected from-workspace, got " + content)
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(implementation = _impl, attrs = {})
"#,
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
    let spec = tokio::task::spawn_blocking(move || {
        resolver_engine.build_starlark_repo_spec(
            "ext+".to_string(),
            &defining_bzl,
            "my_repo",
            BTreeMap::new(),
        )
    })
    .await
    .unwrap()
    .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let mut specs = BTreeMap::new();
    specs.insert("ext+".to_string(), spec);

    let prod_engine = tokio::task::spawn_blocking({
        let p = workspace_path.clone();
        let cache_path = cache.path().to_path_buf();
        move || {
            std::fs::write(
                p.join("MODULE.bazel"),
                r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "ext", version = "1.0")
"#,
            )
            .unwrap();
            EngineBuilder::new(p, Configuration::default())
                .with_bzlmod_cache(cache_path)
                .with_repo_specs(specs)
                .build()
        }
    })
    .await
    .unwrap()
    .unwrap();

    let pkg = PackageId::new(CanonicalRepo::new("ext+"), PackagePath::root());
    let _ = tokio::task::spawn_blocking(move || prod_engine.list_package(&pkg))
        .await
        .unwrap()
        .unwrap();

    // If we got here, ctx.workspace_root.get_child("sentinel.txt").read()
    // returned "from-workspace" and the rule didn't fail.
    assert!(cache.path().join("ext+/MODULE.bazel").is_file());
}
