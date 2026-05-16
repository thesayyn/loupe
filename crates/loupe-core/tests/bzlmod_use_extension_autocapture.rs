//! End-to-end test: MODULE.bazel declares a `use_extension(...)` plus
//! `ext.<class>(...)` tag invocations + `use_repo(ext, "name")`. The
//! engine builder auto-captures, evaluates the extension, registers
//! the resulting specs, materialises on `fetch_all`, and the
//! main-module's repo_mapping is wired so `@name` resolves to the
//! ext-generated canonical.

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn use_extension_autocaptures_and_materialises() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
ext = use_extension("//:defs.bzl", "ext")
ext.create(name = "foo", value = "alpha")
ext.create(name = "bar", value = "beta")
use_repo(ext, "foo", "bar")
"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("defs.bzl"),
        r#"
def _local_repo_impl(ctx):
    ctx.file("BUILD.bazel", "filegroup(name = \"%s\")\n" % ctx.attr.value)

local_repo = repository_rule(
    implementation = _local_repo_impl,
    attrs = {"value": attr.string()},
)

def _ext_impl(module_ctx):
    for m in module_ctx.modules:
        for tag in m.tags.create:
            local_repo(name = tag.name, value = tag.value)

ext = module_extension(
    implementation = _ext_impl,
    tag_classes = {"create": tag_class(attrs = {"name": attr.string(), "value": attr.string()})},
)
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache_path_for(&workspace);

    // Run the entire engine-driven flow inside a single spawn_blocking
    // so the Engine's tokio runtime can build and drop without
    // colliding with the test's outer tokio runtime.
    let result = tokio::task::spawn_blocking({
        let workspace_path = workspace_path.clone();
        let cache_path = cache_path.clone();
        move || -> Result<(_, _, _), anyhow::Error> {
            let engine = EngineBuilder::new(workspace_path.clone(), Configuration::default())
                .with_bzlmod_cache(cache_path.clone())
                .build()?;
            let graph = engine.module_graph()?;
            let report = engine.fetch_all()?;

            // Run a second engine to verify list_package works against
            // an ext-generated repo. Builds cleanly because the cache
            // is now populated from the first engine's fetch_all.
            let engine2 = EngineBuilder::new(workspace_path, Configuration::default())
                .with_bzlmod_cache(cache_path)
                .build()?;
            let labels = engine2.list_package(&PackageId::new(
                CanonicalRepo::new("+ext+foo"),
                PackagePath::root(),
            ))?;
            Ok((graph, report, labels))
        }
    })
    .await
    .unwrap()
    .unwrap();

    let (graph, report, labels) = result;

    assert_eq!(graph.use_extensions.len(), 1);
    let rec = &graph.use_extensions[0];
    assert_eq!(rec.name, "ext");
    assert_eq!(rec.tags.len(), 2);
    assert_eq!(rec.tags[0].class, "create");
    assert_eq!(rec.use_repo_names, vec!["foo".to_string(), "bar".to_string()]);

    let foo_canonical = graph.repo_mapping.get("foo").unwrap();
    let bar_canonical = graph.repo_mapping.get("bar").unwrap();
    assert_eq!(foo_canonical.as_str(), "+ext+foo");
    assert_eq!(bar_canonical.as_str(), "+ext+bar");

    assert_eq!(report.errors.len(), 0, "errors: {:?}", report.errors);
    let mut materialised_canonicals: Vec<&str> = report
        .materialised
        .iter()
        .map(|(c, _)| c.as_str())
        .collect();
    materialised_canonicals.sort();
    assert_eq!(materialised_canonicals, vec!["+ext+bar", "+ext+foo"]);

    let foo_build = std::fs::read_to_string(cache_path.join("+ext+foo/BUILD.bazel")).unwrap();
    assert!(foo_build.contains("alpha"));
    let bar_build = std::fs::read_to_string(cache_path.join("+ext+bar/BUILD.bazel")).unwrap();
    assert!(bar_build.contains("beta"));

    let names: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    assert_eq!(names, vec!["@@+ext+foo//:alpha"]);
}

fn cache_path_for(_w: &tempfile::TempDir) -> std::path::PathBuf {
    // Separate from workspace so workspace discovery doesn't pick it up.
    tempfile::tempdir().unwrap().keep()
}
