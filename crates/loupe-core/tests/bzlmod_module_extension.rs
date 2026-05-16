//! End-to-end test: a user `.bzl` defines a `module_extension` whose
//! impl iterates `module_ctx.modules[*].tags.create` and calls a
//! locally-defined `repository_rule` for each tag. `build_module_extension_specs`
//! drives the eval; the returned spec map is registered via
//! `EngineBuilder::with_repo_specs` and a `list_package` triggers
//! materialisation of each declared repo.

use std::collections::BTreeMap;

use loupe_bzlmod::ModuleTagInvocation;
use loupe_bzlmod::ModuleView;
use loupe_bzlmod::RepoAttrValue;
use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_extension_eval_declares_repos() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
"#,
    )
    .unwrap();
    // The extension defines its own local `repository_rule` that just
    // writes a BUILD file mentioning the tag's `value` attr; the
    // extension impl iterates module.tags.create and calls it.
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

    // Stage 1: build a resolver engine to load defs.bzl through
    // BzlLoadKey + run extension eval.
    let workspace_path = workspace.path().to_path_buf();
    let resolver_engine = tokio::task::spawn_blocking({
        let p = workspace_path.clone();
        move || EngineBuilder::new(p, Configuration::default()).build()
    })
    .await
    .unwrap()
    .unwrap();

    let defining_bzl = Label::new(
        CanonicalRepo::MAIN,
        PackagePath::root(),
        "defs.bzl",
    );

    // Build the modules-view: one root module with two `create` tags.
    let modules = vec![ModuleView {
        name: "demo".to_string(),
        version: "0.0.0".to_string(),
        is_root: true,
        tags: vec![
            ModuleTagInvocation {
                class: "create".to_string(),
                attrs: {
                    let mut a: BTreeMap<String, RepoAttrValue> = BTreeMap::new();
                    a.insert("name".to_string(), RepoAttrValue::String("foo".to_string()));
                    a.insert(
                        "value".to_string(),
                        RepoAttrValue::String("alpha".to_string()),
                    );
                    a
                },
            },
            ModuleTagInvocation {
                class: "create".to_string(),
                attrs: {
                    let mut a: BTreeMap<String, RepoAttrValue> = BTreeMap::new();
                    a.insert("name".to_string(), RepoAttrValue::String("bar".to_string()));
                    a.insert(
                        "value".to_string(),
                        RepoAttrValue::String("beta".to_string()),
                    );
                    a
                },
            },
        ],
    }];

    let specs = tokio::task::spawn_blocking(move || {
        resolver_engine.build_module_extension_specs(&defining_bzl, "ext", "", '+', "ext", modules)
    })
    .await
    .unwrap()
    .unwrap();

    assert_eq!(specs.len(), 2);
    assert!(specs.contains_key("+ext+foo"));
    assert!(specs.contains_key("+ext+bar"));

    // Stage 2: build a fresh engine with the spec map registered,
    // trigger materialisation, verify the repos got created with the
    // values the impl forwarded.
    let cache = tempfile::tempdir().unwrap();
    let prod_engine = tokio::task::spawn_blocking({
        let p = workspace_path.clone();
        let cache_path = cache.path().to_path_buf();
        move || {
            std::fs::write(
                p.join("MODULE.bazel"),
                r#"module(name = "demo", version = "0.0.0")
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

    let report = tokio::task::spawn_blocking(move || prod_engine.fetch_all())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.errors.len(), 0, "errors: {:?}", report.errors);
    assert_eq!(report.materialised.len(), 2);

    let foo_build = std::fs::read_to_string(cache.path().join("+ext+foo/BUILD.bazel")).unwrap();
    assert!(foo_build.contains("alpha"), "got: {foo_build}");
    let bar_build = std::fs::read_to_string(cache.path().join("+ext+bar/BUILD.bazel")).unwrap();
    assert!(bar_build.contains("beta"), "got: {bar_build}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_extension_with_zero_tags_emits_no_specs() {
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
def _impl(module_ctx):
    pass
ext = module_extension(implementation = _impl)
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let engine = tokio::task::spawn_blocking({
        let p = workspace.path().to_path_buf();
        move || EngineBuilder::new(p, Configuration::default()).build()
    })
    .await
    .unwrap()
    .unwrap();
    let defining_bzl = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "defs.bzl");
    let specs = tokio::task::spawn_blocking(move || {
        engine.build_module_extension_specs(&defining_bzl, "ext", "", '+', "ext", vec![])
    })
    .await
    .unwrap()
    .unwrap();
    assert!(specs.is_empty());
    let _ = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());
}
