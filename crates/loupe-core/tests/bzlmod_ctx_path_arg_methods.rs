//! `ctx.delete` / `ctx.template` / `ctx.symlink` accept either a str
//! or a `RepoPath` for their path-typed arguments. Vendored
//! `bazel_tools//tools/build_defs/repo:utils.bzl::patch` does
//! `ctx.delete(ctx.path(_REMOTE_PATCH_DIR))` and Loupe used to raise
//! `Type of parameter 'path' doesn't match, expected 'str', actual 'path'`.

use std::collections::BTreeMap;

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_path_methods_accept_repo_path_args() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("template.in"), "hello {NAME}\n").unwrap();
    std::fs::write(
        workspace.path().join("defs.bzl"),
        r#"
def _impl(ctx):
    # Each path-typed arg passed as ctx.path(...) — Loupe must accept
    # both str and path. Bazel's vendored utils.bzl::patch does
    # ctx.delete(ctx.path("...")) and similar.

    # Stage a template file then template-render into out.txt — both
    # paths are passed as RepoPath, not str.
    ctx.file("template.in", "hello {NAME}\n")
    tmpl = ctx.path("template.in")
    out = ctx.path("out.txt")
    ctx.template(out, tmpl, substitutions = {"{NAME}": "world"})
    content = ctx.read("out.txt")
    if content != "hello world\n":
        fail("template miss: %s" % content)

    # Stage a file then delete via ctx.delete(path).
    ctx.file("scratch.txt", "x")
    deleted = ctx.delete(ctx.path("scratch.txt"))
    if not deleted:
        fail("expected delete to return True")

    # ctx.symlink with both args as paths.
    src = ctx.path("out.txt")
    dst = ctx.path("link.txt")
    ctx.symlink(src, dst)

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

    // Verify all three operations ran in order.
    assert!(!cache.path().join("ext+/scratch.txt").exists());
    assert_eq!(
        std::fs::read_to_string(cache.path().join("ext+/out.txt")).unwrap(),
        "hello world\n"
    );
    assert!(cache.path().join("ext+/link.txt").is_symlink());
}
