//! End-to-end: a repo rule whose impl shells out via `ctx.execute`,
//! reads stdout, and writes a file based on the result. Exercises the
//! tokio process bridge + the `exec_result` struct shape.

use std::collections::BTreeMap;

use loupe_bzlmod::RepoAttrValue;
use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_execute_shells_out_and_captures_stdout() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
"#,
    )
    .unwrap();
    // The rule runs `printf hello-from-execute` and writes the captured
    // stdout into a file. Verifies: subprocess actually runs, return
    // code is 0, stdout is captured into the struct, and the rule can
    // act on it.
    std::fs::write(
        workspace.path().join("defs.bzl"),
        r#"
def _impl(ctx):
    res = ctx.execute(["/bin/sh", "-c", "printf hello-from-execute"])
    if res.return_code != 0:
        fail("subprocess failed: %s" % res.stderr)
    ctx.file("payload.txt", res.stdout)
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

    let defining_bzl = Label::new(CanonicalRepo::main(), PackagePath::root(), "defs.bzl");
    let attrs: BTreeMap<String, RepoAttrValue> = BTreeMap::new();
    let spec = tokio::task::spawn_blocking(move || {
        resolver_engine.build_starlark_repo_spec(
            "ext+".to_string(),
            &defining_bzl,
            "my_repo",
            attrs,
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
    let labels = tokio::task::spawn_blocking(move || prod_engine.list_package(&pkg))
        .await
        .unwrap()
        .unwrap();
    let names: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    assert_eq!(names, vec!["@@ext+//:all"]);

    let payload = std::fs::read_to_string(cache.path().join("ext+/payload.txt")).unwrap();
    assert_eq!(payload, "hello-from-execute");
    assert!(cache.path().join("ext+.marker").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_execute_failing_subprocess_surfaces_to_rule() {
    // `false` exits 1; the rule asserts that and writes a sentinel.
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
    res = ctx.execute(["/bin/sh", "-c", "echo to-stderr >&2; exit 7"])
    if res.return_code == 0:
        fail("expected non-zero exit")
    ctx.file("rc.txt", "%d" % res.return_code)
    ctx.file("err.txt", res.stderr)
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

    let defining_bzl = Label::new(CanonicalRepo::main(), PackagePath::root(), "defs.bzl");
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

    assert_eq!(
        std::fs::read_to_string(cache.path().join("ext+/rc.txt")).unwrap(),
        "7"
    );
    assert!(
        std::fs::read_to_string(cache.path().join("ext+/err.txt"))
            .unwrap()
            .contains("to-stderr")
    );
}
