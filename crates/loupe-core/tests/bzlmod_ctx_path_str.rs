//! `ctx.path(".")` collapses to the workdir, and `str(path)` returns
//! the bare filesystem path. The vendored
//! `@bazel_tools//tools/build_defs/repo:http.bzl::_http_file_impl`
//! does `str(download_path).startswith(str(repo_root))` to sanity-
//! check `downloaded_file_path`; without these normalisations the
//! check raises `'X' cannot be used as downloaded_file_path` on
//! valid inputs (buildozer, buildifier, etc.).

use std::collections::BTreeMap;

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_path_dot_and_str_match_http_file_sanity_check() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("defs.bzl"),
        r#"
def _impl(ctx):
    repo_root = ctx.path(".")
    download_path = ctx.path("file/buildozer")
    if not str(download_path).startswith(str(repo_root)):
        fail("startswith failed: download_path=%s repo_root=%s" %
             (str(download_path), str(repo_root)))
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

    assert!(cache.path().join("ext+/MODULE.bazel").is_file());
}
