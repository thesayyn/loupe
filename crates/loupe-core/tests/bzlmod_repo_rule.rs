//! End-to-end test: a user `.bzl` defines a `repository_rule` whose
//! impl calls `ctx.download_and_extract` and `ctx.file`. The test
//! resolves it through `Engine::build_starlark_repo_spec`, registers
//! the spec, and triggers materialisation via `list_package`. The
//! workdir ends up populated by the rule, not by the http_archive
//! fast-path.

use std::collections::BTreeMap;

use loupe_bzlmod::RepoAttrValue;
use loupe_bzlmod::integrity::HashAlgorithm;
use loupe_bzlmod::integrity::Integrity;
use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::Label;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;
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
async fn repository_rule_materialises_an_external_repo() {
    // Tarball whose payload becomes the materialised repo's source tree.
    let payload = build_tar_gz(&[("inner/data.txt", b"payload"), ("inner/hello", b"world")]);
    let payload_sri = Integrity::compute(HashAlgorithm::Sha256, &payload).to_sri_string();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/p.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
        .mount(&server)
        .await;
    let payload_url = format!("{}/p.tar.gz", server.uri());

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
    ctx.report_progress("starting %s" % ctx.name)
    ctx.download_and_extract(
        url = ctx.attr.url,
        integrity = ctx.attr.integrity,
        strip_prefix = "inner",
    )
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(
    implementation = _impl,
    attrs = {
        "url": attr.string(),
        "integrity": attr.string(),
    },
)
"#,
    )
    .unwrap();
    // Empty top-level BUILD so the workspace discovers cleanly.
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    // Stage 1: build an engine to resolve the rule symbol through
    // BzlLoadKey. The resulting FrozenModule + FrozenValue get stuffed
    // into the spec.
    let workspace_path = workspace.path().to_path_buf();
    let resolver_engine = tokio::task::spawn_blocking({
        let p = workspace_path.clone();
        move || EngineBuilder::new(p, Configuration::default()).build()
    })
    .await
    .unwrap()
    .unwrap();

    let defining_bzl = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "defs.bzl");
    let mut attrs: BTreeMap<String, RepoAttrValue> = BTreeMap::new();
    attrs.insert("url".to_string(), RepoAttrValue::String(payload_url));
    attrs.insert(
        "integrity".to_string(),
        RepoAttrValue::String(payload_sri.clone()),
    );

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

    // Stage 2: build the production engine with the spec registered,
    // trigger materialisation via list_package on the external repo's
    // root package.
    let cache = tempfile::tempdir().unwrap();
    let mut specs = BTreeMap::new();
    specs.insert("ext+".to_string(), spec);

    let prod_engine = tokio::task::spawn_blocking({
        let p = workspace_path.clone();
        let cache_path = cache.path().to_path_buf();
        move || {
            // The main workspace gets a synthetic bazel_dep on `ext` so
            // ModuleGraphKey installs the apparent→canonical mapping.
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

    // Both rule outputs landed in the cache: the synthesised BUILD/MODULE
    // files and the extracted-archive contents (strip_prefix=inner).
    assert!(cache.path().join("ext+/MODULE.bazel").is_file());
    assert!(cache.path().join("ext+/BUILD.bazel").is_file());
    assert_eq!(
        std::fs::read(cache.path().join("ext+/data.txt")).unwrap(),
        b"payload"
    );
    assert_eq!(
        std::fs::read(cache.path().join("ext+/hello")).unwrap(),
        b"world"
    );
}
