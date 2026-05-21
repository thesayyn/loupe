//! Covers ctx.path / ctx.which / ctx.read / ctx.watch_tree / ctx.template.

use std::collections::BTreeMap;

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

fn write_workspace(dir: &std::path::Path, defs: &str) {
    std::fs::write(
        dir.join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
"#,
    )
    .unwrap();
    std::fs::write(dir.join("defs.bzl"), defs).unwrap();
    std::fs::write(dir.join("BUILD.bazel"), "").unwrap();
}

async fn materialise(workspace: &std::path::Path, cache: &std::path::Path) {
    let workspace_path = workspace.to_path_buf();
    let engine = tokio::task::spawn_blocking({
        let p = workspace_path.clone();
        move || EngineBuilder::new(p, Configuration::default()).build()
    })
    .await
    .unwrap()
    .unwrap();
    let defining_bzl = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "defs.bzl");
    let spec = tokio::task::spawn_blocking(move || {
        engine.build_starlark_repo_spec(
            "ext+".to_string(),
            &defining_bzl,
            "my_repo",
            BTreeMap::new(),
        )
    })
    .await
    .unwrap()
    .unwrap();

    let cache_path = cache.to_path_buf();
    let mut specs = BTreeMap::new();
    specs.insert("ext+".to_string(), spec);
    tokio::task::spawn_blocking(move || {
        std::fs::write(
            workspace_path.join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "ext", version = "1.0")
"#,
        )
        .unwrap();
        let engine = EngineBuilder::new(workspace_path, Configuration::default())
            .with_bzlmod_cache(cache_path)
            .with_repo_specs(specs)
            .build()
            .unwrap();
        let pkg = PackageId::new(CanonicalRepo::new("ext+"), PackagePath::root());
        engine.list_package(&pkg).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_path_read_records_observation() {
    // The rule reads a file via ctx.path(...).read() — both modes
    // should produce a File record.
    let observed_dir = tempfile::tempdir().unwrap();
    let observed = observed_dir.path().join("conf.txt");
    std::fs::write(&observed, b"OBSERVED_VALUE").unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let defs = format!(
        r#"
def _impl(ctx):
    p = ctx.path("{observed}")
    content = p.read()
    ctx.file("dump.txt", content)
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(implementation = _impl, attrs = {{}})
"#,
        observed = observed.display(),
    );
    write_workspace(workspace.path(), &defs);

    materialise(workspace.path(), cache.path()).await;
    assert_eq!(
        std::fs::read_to_string(cache.path().join("ext+/dump.txt")).unwrap(),
        "OBSERVED_VALUE"
    );
    let marker = std::fs::read_to_string(cache.path().join("ext+.marker")).unwrap();
    assert!(
        marker
            .lines()
            .any(|l| l.starts_with("FILE:") && l.contains("conf.txt")),
        "marker should record the FILE observation; got:\n{marker}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_which_finds_executable_on_declared_path() {
    // sh lives at /bin/sh on macOS/Linux. ctx.which("sh") should
    // return that (or another match in /bin or /usr/bin).
    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    write_workspace(
        workspace.path(),
        r#"
def _impl(ctx):
    p = ctx.which("sh")
    if p == None:
        fail("sh not found")
    ctx.file("sh_path.txt", p)
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(implementation = _impl, attrs = {})
"#,
    );

    materialise(workspace.path(), cache.path()).await;
    let p = std::fs::read_to_string(cache.path().join("ext+/sh_path.txt")).unwrap();
    assert!(
        p.ends_with("/sh"),
        "ctx.which should return a path ending in /sh, got `{p}`"
    );
    assert!(std::path::Path::new(&p).is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_watch_tree_records_dirtree_observation() {
    let observed_dir = tempfile::tempdir().unwrap();
    std::fs::write(observed_dir.path().join("a"), b"1").unwrap();
    std::fs::create_dir_all(observed_dir.path().join("sub")).unwrap();
    std::fs::write(observed_dir.path().join("sub/b"), b"2").unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let defs = format!(
        r#"
def _impl(ctx):
    ctx.watch_tree("{observed}")
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(implementation = _impl, attrs = {{}})
"#,
        observed = observed_dir.path().display(),
    );
    write_workspace(workspace.path(), &defs);

    materialise(workspace.path(), cache.path()).await;
    let marker = std::fs::read_to_string(cache.path().join("ext+.marker")).unwrap();
    assert!(
        marker.lines().any(|l| l.starts_with("DIRTREE:")),
        "marker should record a DIRTREE entry; got:\n{marker}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctx_template_substitutes_and_records_template_file() {
    let template_dir = tempfile::tempdir().unwrap();
    let template_path = template_dir.path().join("BUILD.tmpl");
    std::fs::write(&template_path, "filegroup(name=\"{NAME}\", srcs={SRCS})\n").unwrap();

    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let defs = format!(
        r#"
def _impl(ctx):
    ctx.template(
        "out.bzl",
        "{template}",
        substitutions = {{"{{NAME}}": "wired", "{{SRCS}}": "[\"a\", \"b\"]"}},
    )
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(implementation = _impl, attrs = {{}})
"#,
        template = template_path.display(),
    );
    write_workspace(workspace.path(), &defs);

    materialise(workspace.path(), cache.path()).await;
    let rendered = std::fs::read_to_string(cache.path().join("ext+/out.bzl")).unwrap();
    assert_eq!(rendered, "filegroup(name=\"wired\", srcs=[\"a\", \"b\"])\n");
    let marker = std::fs::read_to_string(cache.path().join("ext+.marker")).unwrap();
    assert!(
        marker
            .lines()
            .any(|l| l.starts_with("FILE:") && l.contains("BUILD.tmpl")),
        "marker should record the template FILE observation; got:\n{marker}"
    );
}
