//! Bazel-7-era `lockFileVersion: 11` workspaces use `~` as the
//! canonical separator. ModuleGraph detects this from the lockfile
//! and threads it through extension eval + use_repo canonicalisation.

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tilde_separator_threads_to_repo_mapping() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
ext = use_extension("//:defs.bzl", "ext")
ext.create(name = "foo", value = "v")
use_repo(ext, "foo")
"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("defs.bzl"),
        r#"
def _impl(ctx):
    ctx.file("BUILD.bazel", "")

local_repo = repository_rule(implementation = _impl, attrs = {"value": attr.string()})

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

    // A Bazel-7-era lockfile: contains an extension owner ending in
    // `~` (`bazel_features~`), which makes module_graph::parse detect
    // sep = '~'.
    let lockfile = serde_json::json!({
        "lockFileVersion": 11,
        "moduleExtensions": {
            "@@bazel_features~//foo:bar.bzl%marker": {
                "general": {}
            }
        }
    });
    std::fs::write(
        workspace.path().join("MODULE.bazel.lock"),
        serde_json::to_string_pretty(&lockfile).unwrap(),
    )
    .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache.path().to_path_buf();

    let (sep, foo_canonical, report) =
        tokio::task::spawn_blocking(move || -> Result<_, anyhow::Error> {
            let engine = EngineBuilder::new(workspace_path, Configuration::default())
                .with_bzlmod_cache(cache_path)
                .build()?;
            let graph = engine.module_graph()?;
            let sep = graph.canonical_separator();
            let foo = graph.repo_mapping.lookup("foo").unwrap().as_str().to_string();
            let report = engine.fetch_all()?;
            Ok((sep, foo, report))
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(sep, '~');
    assert_eq!(foo_canonical, "~ext~foo");
    assert_eq!(report.errors.len(), 0, "errors: {:?}", report.errors);
    let canonicals: Vec<&str> = report
        .materialised
        .iter()
        .map(|(c, _)| c.as_str())
        .collect();
    assert_eq!(canonicals, vec!["~ext~foo"]);
}
