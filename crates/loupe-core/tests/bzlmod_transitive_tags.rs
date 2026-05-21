//! Transitive-module tag aggregation: when a `bazel_dep` module
//! invokes the same extension as the workspace's main module, its
//! tags should reach the extension's `module_ctx.modules` list. The
//! engine canonicalises each module's `use_extension` bzl_label
//! against that module's own repo_mapping and joins by canonical Label.

use loupe_bzlmod::integrity::HashAlgorithm;
use loupe_bzlmod::integrity::Integrity;
use loupe_bzlmod::registry::Registry;
use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn build_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut t = tar::Builder::new(gz);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            t.append_data(&mut header, name, *data).unwrap();
        }
        t.into_inner().unwrap().finish().unwrap();
    }
    buf
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "TODO(engine): same canonical-prefix bug as \
    bzlmod_registry_with_extension::registry_resolved_bazel_dep_provides_extension. \
    Extension repos owned by main should be `+ext+name`, not \
    `extprovider++ext+name`. Fix in the extension repo canonical-name \
    derivation in loupe-bzlmod."]
async fn transitive_module_tags_reach_extension_impl() {
    // extprovider's archive carries:
    //   - defs.bzl: defines local_repo + ext (module_extension).
    //   - MODULE.bazel: declares `bazel_dep(name="extprovider")` for
    //     its own self-reference, then `use_extension("//:defs.bzl",
    //     "ext")` + `ext.create(name="from_transitive", value="t")`.
    // The workspace's main MODULE.bazel does the same use_extension
    // (via `@extprovider//:defs.bzl`) + invokes `ext.create(
    // name="from_main", value="m")`. Both tags should reach the
    // extension impl.
    let extprovider_defs = br#"
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
"#;
    let extprovider_module_bazel = br#"module(name = "extprovider", version = "1.0")
ext = use_extension("//:defs.bzl", "ext")
ext.create(name = "from_transitive", value = "t")
"#;
    let extprovider_archive = build_tar_gz(&[
        (
            "extprovider-1.0/MODULE.bazel",
            extprovider_module_bazel.as_slice(),
        ),
        ("extprovider-1.0/BUILD.bazel", b""),
        ("extprovider-1.0/defs.bzl", extprovider_defs),
    ]);
    let archive_sri =
        Integrity::compute(HashAlgorithm::Sha256, &extprovider_archive).to_sri_string();

    let server = MockServer::start().await;
    let server_uri = server.uri();

    // Registry serves both the archive AND the standalone MODULE.bazel
    // (parsed by the resolver to discover transitive deps + tags).
    Mock::given(method("GET"))
        .and(path("/modules/extprovider/1.0/MODULE.bazel"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(extprovider_module_bazel.as_slice().to_vec()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/modules/extprovider/1.0/source.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(
                serde_json::json!({
                    "url": format!("{server_uri}/archives/extprovider-1.0.tar.gz"),
                    "integrity": archive_sri,
                    "strip_prefix": "extprovider-1.0"
                })
                .to_string()
                .into_bytes(),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archives/extprovider-1.0.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(extprovider_archive))
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "extprovider", version = "1.0")
ext = use_extension("@extprovider//:defs.bzl", "ext")
ext.create(name = "from_main", value = "m")
use_repo(ext, "from_main")
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let cache = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache.path().to_path_buf();
    let registry = Registry::custom(server_uri);

    let result = tokio::task::spawn_blocking(move || -> Result<_, anyhow::Error> {
        let engine = EngineBuilder::new(workspace_path, Configuration::default())
            .with_bzlmod_cache(cache_path.clone())
            .with_registry(registry)
            .build()?;
        let report = engine.fetch_all()?;
        Ok((report, cache_path))
    })
    .await
    .unwrap()
    .unwrap();
    let (report, cache_path) = result;

    assert_eq!(report.errors.len(), 0, "errors: {:?}", report.errors);
    let mut canonicals: Vec<&str> = report
        .materialised
        .iter()
        .map(|(c, _)| c.as_str())
        .collect();
    canonicals.sort();
    // Both the main-module's tag AND the transitive module's tag
    // produced an ext-generated repo. Plus the registry-fetched
    // extprovider+ itself.
    assert_eq!(
        canonicals,
        vec!["+ext+from_main", "+ext+from_transitive", "extprovider+"]
    );
    assert!(cache_path.join("+ext+from_main/BUILD.bazel").is_file());
    assert!(
        cache_path
            .join("+ext+from_transitive/BUILD.bazel")
            .is_file()
    );
}
