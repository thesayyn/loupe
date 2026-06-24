//! End-to-end test: a `bazel_dep`-declared external repo is materialised
//! lazily from a wiremock-served `http_archive` during `list_package`,
//! with no `bazel mod vendor` step.

use std::collections::BTreeMap;

use loupe_bzlmod::RepoSpec;
use loupe_bzlmod::RepoSpecKind;
use loupe_bzlmod::integrity::HashAlgorithm;
use loupe_bzlmod::integrity::Integrity;
use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
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
async fn lazy_fetch_resolves_bazel_dep_external_repo() {
    let tarball = build_tar_gz(&[
        ("ext-1.0/MODULE.bazel", b"module(name=\"ext\", version=\"1.0\")"),
        (
            "ext-1.0/defs.bzl",
            b"def _hello_impl(ctx):\n    return []\n\nhello = rule(implementation=_hello_impl, attrs={})\n",
        ),
        (
            "ext-1.0/BUILD.bazel",
            b"load(\":defs.bzl\", \"hello\")\nhello(name=\"x\")\n",
        ),
    ]);
    let sri = Integrity::compute(HashAlgorithm::Sha256, &tarball).to_sri_string();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ext.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball.clone()))
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "ext", version = "1.0")
"#,
    )
    .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let mut specs: BTreeMap<String, RepoSpec> = BTreeMap::new();
    specs.insert(
        "ext+".to_string(),
        RepoSpec {
            canonical: "ext+".to_string(),
            kind: RepoSpecKind::Http {
                urls: vec![format!("{}/ext.tar.gz", server.uri())],
                integrity: Some(sri),
                strip_prefix: Some("ext-1.0".to_string()),
                archive_kind: None,
                overlay: BTreeMap::new(),
                patches: Vec::new(),
                patch_strip: 0,
            },
        },
    );

    let engine = tokio::task::spawn_blocking({
        let workspace_path = workspace.path().to_path_buf();
        let cache_path = cache.path().to_path_buf();
        move || {
            EngineBuilder::new(workspace_path, Configuration::default())
                .with_bzlmod_cache(cache_path)
                .with_repo_specs(specs)
                .build()
        }
    })
    .await
    .unwrap()
    .unwrap();

    let pkg = PackageId::new(CanonicalRepo::new("ext+"), PackagePath::root());
    let labels = tokio::task::spawn_blocking(move || engine.list_package(&pkg))
        .await
        .unwrap()
        .unwrap();

    let names: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    assert_eq!(names, vec!["@@ext+//:x"]);

    // Materialised on disk under the cache.
    assert!(cache.path().join("ext+/MODULE.bazel").is_file());
}
