//! Integrated end-to-end: workspace has only `MODULE.bazel`. It
//! declares `bazel_dep(name="extprovider")` and then calls
//! `use_extension("@extprovider//:defs.bzl", "ext")` whose definition
//! lives *inside* the registry-fetched archive. The full pipeline
//! exercises: BCR resolution → archive fetch → load extension `.bzl`
//! → module_extension eval → repository_rule materialisation. No
//! lockfile, no vendor, no bazel binary.

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
async fn registry_resolved_bazel_dep_provides_extension() {
    // extprovider's archive carries a `.bzl` defining a
    // `repository_rule` + `module_extension`. The workspace's
    // MODULE.bazel imports the extension and feeds tags into it; each
    // captured tag materialises through `local_repo`.
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
    let extprovider_archive = build_tar_gz(&[
        (
            "extprovider-1.0/MODULE.bazel",
            b"module(name = \"extprovider\", version = \"1.0\")\n",
        ),
        ("extprovider-1.0/BUILD.bazel", b""),
        ("extprovider-1.0/defs.bzl", extprovider_defs),
    ]);
    let archive_sri =
        Integrity::compute(HashAlgorithm::Sha256, &extprovider_archive).to_sri_string();

    let server = MockServer::start().await;
    let server_uri = server.uri();

    Mock::given(method("GET"))
        .and(path("/modules/extprovider/1.0/MODULE.bazel"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(
            b"module(name = \"extprovider\", version = \"1.0\")\n".to_vec(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/modules/extprovider/1.0/source.json"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(
            serde_json::json!({
                "url": format!("{server_uri}/archives/extprovider-1.0.tar.gz"),
                "integrity": archive_sri,
                "strip_prefix": "extprovider-1.0"
            })
            .to_string()
            .into_bytes(),
        ))
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
ext.create(name = "alpha", value = "a")
ext.create(name = "beta", value = "b")
use_repo(ext, "alpha", "beta")
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let cache = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache.path().to_path_buf();
    let registry = Registry::custom(server_uri);

    let report = tokio::task::spawn_blocking(move || -> Result<_, anyhow::Error> {
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

    let (report, cache_path) = report;
    assert_eq!(report.errors.len(), 0, "errors: {:?}", report.errors);
    let mut canonicals: Vec<&str> = report
        .materialised
        .iter()
        .map(|(c, _)| c.as_str())
        .collect();
    canonicals.sort();
    // Must include: the registry-resolved dep (extprovider+) AND the
    // two extension-declared repos (+ext+alpha, +ext+beta).
    assert_eq!(
        canonicals,
        vec!["+ext+alpha", "+ext+beta", "extprovider+"]
    );

    // The extension's repo_rule wrote the right `value` to each BUILD.
    let alpha_build =
        std::fs::read_to_string(cache_path.join("+ext+alpha/BUILD.bazel")).unwrap();
    assert!(alpha_build.contains("\"a\""), "got: {alpha_build}");
    let beta_build =
        std::fs::read_to_string(cache_path.join("+ext+beta/BUILD.bazel")).unwrap();
    assert!(beta_build.contains("\"b\""), "got: {beta_build}");
}
