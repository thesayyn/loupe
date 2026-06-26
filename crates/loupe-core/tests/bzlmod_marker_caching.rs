//! Marker file caching: a repo rule materialises, writes a marker
//! recording its observed inputs. Second call hits the marker fast-path
//! (rule impl does not re-run). Mutating a watched file triggers a
//! re-materialisation.

use std::collections::BTreeMap;

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

fn build_workspace(dir: &std::path::Path, defs_bzl: &str) {
    std::fs::write(
        dir.join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
"#,
    )
    .unwrap();
    std::fs::write(dir.join("defs.bzl"), defs_bzl).unwrap();
    std::fs::write(dir.join("BUILD.bazel"), "").unwrap();
}

async fn build_spec(workspace: &std::path::Path) -> loupe_bzlmod::RepoSpec {
    let workspace_path = workspace.to_path_buf();
    let engine = tokio::task::spawn_blocking(move || {
        EngineBuilder::new(workspace_path, Configuration::default()).build()
    })
    .await
    .unwrap()
    .unwrap();
    let defining_bzl = Label::new(CanonicalRepo::main(), PackagePath::root(), "defs.bzl");
    tokio::task::spawn_blocking(move || {
        engine.build_starlark_repo_spec(
            "ext+".to_string(),
            &defining_bzl,
            "my_repo",
            BTreeMap::new(),
        )
    })
    .await
    .unwrap()
    .unwrap()
}

async fn materialise(
    workspace: &std::path::Path,
    cache: &std::path::Path,
    spec: loupe_bzlmod::RepoSpec,
) {
    let workspace = workspace.to_path_buf();
    let cache = cache.to_path_buf();
    let mut specs = BTreeMap::new();
    specs.insert("ext+".to_string(), spec);
    tokio::task::spawn_blocking(move || {
        std::fs::write(
            workspace.join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "ext", version = "1.0")
"#,
        )
        .unwrap();
        let engine = EngineBuilder::new(workspace, Configuration::default())
            .with_bzlmod_cache(cache)
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
async fn marker_short_circuits_repeated_materialise() {
    // Rule writes a timestamp-ish counter file each time it runs. If
    // the marker fast-path doesn't kick in, the file would update on
    // every materialise call. We use a side-effect file in /tmp to
    // observe rerun behavior.
    let log_dir = tempfile::tempdir().unwrap();
    let log_path = log_dir.path().join("runs.log");
    let defs = format!(
        r#"
def _impl(ctx):
    res = ctx.execute(["/bin/sh", "-c", "echo run >> {log_path}"])
    if res.return_code != 0:
        fail("logging failed")
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(implementation = _impl, attrs = {{}})
"#,
        log_path = log_path.display(),
    );

    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    build_workspace(workspace.path(), &defs);

    let spec1 = build_spec(workspace.path()).await;
    materialise(workspace.path(), cache.path(), spec1).await;

    let after_first = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert_eq!(
        after_first.lines().count(),
        1,
        "first run should have logged once"
    );

    // Re-build the spec from the same workspace (same .bzl source ⇒
    // same source digest), reuse the cache. Marker fast-path should
    // skip re-running the rule impl.
    let spec2 = build_spec(workspace.path()).await;
    materialise(workspace.path(), cache.path(), spec2).await;

    let after_second = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert_eq!(
        after_second.lines().count(),
        1,
        "second run should NOT have re-executed the impl (got {after_second:?})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watched_file_change_triggers_rerun() {
    // The rule watches an external file via ctx.watch. Mutating the
    // file between runs must cause the second materialise to re-run.
    let observed_dir = tempfile::tempdir().unwrap();
    let observed_path = observed_dir.path().join("observed");
    std::fs::write(&observed_path, b"first").unwrap();

    let log_dir = tempfile::tempdir().unwrap();
    let log_path = log_dir.path().join("runs.log");
    let defs = format!(
        r#"
def _impl(ctx):
    ctx.watch("{observed}")
    res = ctx.execute(["/bin/sh", "-c", "echo run >> {log_path}"])
    if res.return_code != 0:
        fail("logging failed")
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(implementation = _impl, attrs = {{}})
"#,
        observed = observed_path.display(),
        log_path = log_path.display(),
    );

    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    build_workspace(workspace.path(), &defs);

    let spec1 = build_spec(workspace.path()).await;
    materialise(workspace.path(), cache.path(), spec1).await;
    assert_eq!(
        std::fs::read_to_string(&log_path).unwrap().lines().count(),
        1,
    );

    // Mutate the watched file. Re-materialise — should rerun.
    std::fs::write(&observed_path, b"second").unwrap();
    let spec2 = build_spec(workspace.path()).await;
    materialise(workspace.path(), cache.path(), spec2).await;
    assert_eq!(
        std::fs::read_to_string(&log_path).unwrap().lines().count(),
        2,
        "watched-file change must trigger re-materialise"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repo_env_override_visible_to_rule() {
    // `ctx.getenv("LOUPE_TEST_FOO")` returns the override value, not
    // the host value. And the record persists into the marker.
    let workspace = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    build_workspace(
        workspace.path(),
        r#"
def _impl(ctx):
    v = ctx.getenv("LOUPE_TEST_FOO")
    if v == None:
        fail("LOUPE_TEST_FOO not set")
    ctx.file("foo.txt", v)
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(implementation = _impl, attrs = {})
"#,
    );

    let spec = build_spec(workspace.path()).await;

    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache.path().to_path_buf();
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
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "LOUPE_TEST_FOO".to_string(),
            loupe_bzlmod::env_policy::EnvOverride::Set("bar-from-override".to_string()),
        );
        let engine = EngineBuilder::new(workspace_path, Configuration::default())
            .with_bzlmod_cache(cache_path)
            .with_repo_specs(specs)
            .with_repo_env(overrides)
            .build()
            .unwrap();
        let pkg = PackageId::new(CanonicalRepo::new("ext+"), PackagePath::root());
        engine.list_package(&pkg).unwrap();
    })
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(cache.path().join("ext+/foo.txt")).unwrap(),
        "bar-from-override"
    );
    let marker = std::fs::read_to_string(cache.path().join("ext+.marker")).unwrap();
    assert!(
        marker.contains("ENV:LOUPE_TEST_FOO bar-from-override"),
        "marker should record the env var observation; got:\n{marker}"
    );
}
