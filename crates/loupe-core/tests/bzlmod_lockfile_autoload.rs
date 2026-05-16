//! `MODULE.bazel.lock`-driven auto-load: when the engine builder is
//! given a `with_bzlmod_cache` but no manual `with_repo_specs`, it
//! should pull `generatedRepoSpecs` from the workspace lockfile so the
//! lazy-fetch path "just works" for the common case of a workspace
//! that's already been `bazel mod deps`'d once.

use loupe_bzlmod::integrity::HashAlgorithm;
use loupe_bzlmod::integrity::Integrity;
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
async fn fetch_all_materialises_lockfile_generated_repos() {
    let payload = build_tar_gz(&[("inner/file", b"x")]);
    let sri = Integrity::compute(HashAlgorithm::Sha256, &payload).to_sri_string();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
        .mount(&server)
        .await;
    let url = format!("{}/x.tar.gz", server.uri());

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
"#,
    )
    .unwrap();
    // Lockfile with one extension-generated `http_archive` spec.
    // Canonical-naming follows `<owner><sep><ext><sep><repo>`; the
    // owner ID format `@@bazel_features+//foo:bar%ext` yields owner =
    // "bazel_features+" and ext = "ext", sep = "+" → canonical name
    // `bazel_features++ext+thing`.
    let lockfile = serde_json::json!({
        "lockFileVersion": 26,
        "moduleExtensions": {
            "@@bazel_features+//foo:bar.bzl%ext": {
                "general": {
                    "generatedRepoSpecs": {
                        "thing": {
                            "ruleClassName": "http_archive",
                            "attributes": {
                                "urls": [url],
                                "integrity": sri,
                                "strip_prefix": "inner"
                            }
                        }
                    }
                }
            }
        }
    });
    std::fs::write(
        workspace.path().join("MODULE.bazel.lock"),
        serde_json::to_string_pretty(&lockfile).unwrap(),
    )
    .unwrap();

    let cache = tempfile::tempdir().unwrap();
    let engine = tokio::task::spawn_blocking({
        let p = workspace.path().to_path_buf();
        let c = cache.path().to_path_buf();
        move || {
            EngineBuilder::new(p, Configuration::default())
                .with_bzlmod_cache(c)
                .build()
        }
    })
    .await
    .unwrap()
    .unwrap();

    let report = tokio::task::spawn_blocking(move || engine.fetch_all())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(report.errors.len(), 0, "errors: {:?}", report.errors);
    assert_eq!(report.materialised.len(), 1);
    let (canonical, path) = &report.materialised[0];
    assert_eq!(canonical, "bazel_features++ext+thing");
    assert_eq!(std::fs::read(path.join("file")).unwrap(), b"x");
}
