//! End-to-end test: a workspace declares `bazel_dep(name=X, version=V)`
//! with no lockfile and no vendor dir. The engine builder, given a
//! `with_registry(...)` pointing at a wiremock-served BCR, resolves
//! the dep transitively, materialises each module's `http_archive`,
//! and `@X` becomes loadable. This is the "complete cut from bazel"
//! gate per the bzlmod plan.

use loupe_bzlmod::integrity::HashAlgorithm;
use loupe_bzlmod::integrity::Integrity;
use loupe_bzlmod::registry::Registry;
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
async fn registry_resolves_and_materialises_a_bazel_dep() {
    // Tarball that becomes `foo+`'s materialised tree: contains a
    // BUILD.bazel that declares one filegroup.
    let foo_tarball = build_tar_gz(&[
        (
            "foo-1.0/MODULE.bazel",
            b"module(name=\"foo\", version=\"1.0\")",
        ),
        ("foo-1.0/BUILD.bazel", b"filegroup(name=\"all\")\n"),
    ]);
    let foo_sri = Integrity::compute(HashAlgorithm::Sha256, &foo_tarball).to_sri_string();

    let server = MockServer::start().await;
    // BCR endpoints for module foo@1.0.
    Mock::given(method("GET"))
        .and(path("/modules/foo/1.0/MODULE.bazel"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"module(name=\"foo\", version=\"1.0\")".to_vec()),
        )
        .mount(&server)
        .await;
    // Archive endpoint.
    Mock::given(method("GET"))
        .and(path("/archives/foo-1.0.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(foo_tarball.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/modules/foo/1.0/source.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(
                serde_json::json!({
                    "url": format!("{}/archives/foo-1.0.tar.gz", server.uri()),
                    "integrity": foo_sri,
                    "strip_prefix": "foo-1.0"
                })
                .to_string()
                .into_bytes(),
            ),
        )
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "foo", version = "1.0")
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let cache = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache.path().to_path_buf();
    let registry = Registry::custom(server.uri());

    // Build + query — all on the blocking thread to avoid runtime nesting.
    let result = tokio::task::spawn_blocking(move || -> Result<_, anyhow::Error> {
        let engine = EngineBuilder::new(workspace_path, Configuration::default())
            .with_bzlmod_cache(cache_path.clone())
            .with_registry(registry)
            .build()?;

        // After registry resolution, foo+ is in the engine's spec map.
        // Materialise on demand by listing its root package — which
        // forces BzlLoadKey for the BUILD eval.
        let labels = engine.list_package(&PackageId::new(
            CanonicalRepo::new("foo+"),
            PackagePath::root(),
        ))?;
        Ok((labels, cache_path))
    })
    .await
    .unwrap()
    .unwrap();

    let (labels, cache_path) = result;
    let names: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    assert_eq!(names, vec!["@@foo+//:all"]);

    // The materialised tree landed under <cache>/foo+/.
    assert!(cache_path.join("foo+/MODULE.bazel").is_file());
    assert!(cache_path.join("foo+/BUILD.bazel").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_overlay_supplies_missing_build_file() {
    // zlib-shaped: the module's source archive ships NO Bazel BUILD;
    // BCR provides `BUILD.bazel` via the `overlay` field of source.json.
    // The engine must fetch the overlay file, write it into the
    // materialised repo root, and `@zlib//` must become loadable.
    let zlib_tarball = build_tar_gz(&[("zlib-1.3.1/zlib.h", b"/* no BUILD here */")]);
    let zlib_sri = Integrity::compute(HashAlgorithm::Sha256, &zlib_tarball).to_sri_string();
    let overlay_build = b"filegroup(name=\"z\")\n";
    let overlay_sri = Integrity::compute(HashAlgorithm::Sha256, overlay_build).to_sri_string();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/modules/zlib/1.3.1/MODULE.bazel"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"module(name=\"zlib\", version=\"1.3.1\")".to_vec()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archives/zlib-1.3.1.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zlib_tarball.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/modules/zlib/1.3.1/source.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(
                serde_json::json!({
                    "url": format!("{}/archives/zlib-1.3.1.tar.gz", server.uri()),
                    "integrity": zlib_sri,
                    "strip_prefix": "zlib-1.3.1",
                    "overlay": { "BUILD.bazel": overlay_sri }
                })
                .to_string()
                .into_bytes(),
            ),
        )
        .mount(&server)
        .await;
    // The overlay file endpoint.
    Mock::given(method("GET"))
        .and(path("/modules/zlib/1.3.1/overlay/BUILD.bazel"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(overlay_build.to_vec()))
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "zlib", version = "1.3.1")
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let cache = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache.path().to_path_buf();
    let registry = Registry::custom(server.uri());

    let result = tokio::task::spawn_blocking(move || -> Result<_, anyhow::Error> {
        let engine = EngineBuilder::new(workspace_path, Configuration::default())
            .with_bzlmod_cache(cache_path.clone())
            .with_registry(registry)
            .build()?;
        let labels = engine.list_package(&PackageId::new(
            CanonicalRepo::new("zlib+"),
            PackagePath::root(),
        ))?;
        Ok((labels, cache_path))
    })
    .await
    .unwrap()
    .unwrap();

    let (labels, cache_path) = result;
    let names: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    assert_eq!(names, vec!["@@zlib+//:z"]);

    // The overlay BUILD.bazel was written into the materialised root,
    // alongside the archive's own (BUILD-less) content.
    assert!(cache_path.join("zlib+/BUILD.bazel").is_file());
    assert!(cache_path.join("zlib+/zlib.h").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_patch_supplies_missing_build_file() {
    // The real zlib case: `source.json` carries `patches` (not
    // `overlay`). `add_build_file.patch` is a `--- /dev/null` new-file
    // diff that creates `BUILD.bazel`; `patch_strip` = 0 → bare path.
    let zlib_tarball = build_tar_gz(&[("zlib-1.3.1/zlib.h", b"/* no BUILD here */")]);
    let zlib_sri = Integrity::compute(HashAlgorithm::Sha256, &zlib_tarball).to_sri_string();
    let patch_text = "\
--- /dev/null
+++ BUILD.bazel
@@ -0,0 +1,1 @@
+filegroup(name = \"z\")
";
    let patch_sri =
        Integrity::compute(HashAlgorithm::Sha256, patch_text.as_bytes()).to_sri_string();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/modules/zlib/1.3.1/MODULE.bazel"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"module(name=\"zlib\", version=\"1.3.1\")".to_vec()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archives/zlib-1.3.1.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zlib_tarball.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/modules/zlib/1.3.1/source.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(
                serde_json::json!({
                    "url": format!("{}/archives/zlib-1.3.1.tar.gz", server.uri()),
                    "integrity": zlib_sri,
                    "strip_prefix": "zlib-1.3.1",
                    "patches": { "add_build_file.patch": patch_sri },
                    "patch_strip": 0
                })
                .to_string()
                .into_bytes(),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/modules/zlib/1.3.1/patches/add_build_file.patch"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(patch_text.as_bytes().to_vec()))
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "zlib", version = "1.3.1")
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let cache = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache.path().to_path_buf();
    let registry = Registry::custom(server.uri());

    let result = tokio::task::spawn_blocking(move || -> Result<_, anyhow::Error> {
        let engine = EngineBuilder::new(workspace_path, Configuration::default())
            .with_bzlmod_cache(cache_path.clone())
            .with_registry(registry)
            .build()?;
        let labels = engine.list_package(&PackageId::new(
            CanonicalRepo::new("zlib+"),
            PackagePath::root(),
        ))?;
        Ok((labels, cache_path))
    })
    .await
    .unwrap()
    .unwrap();

    let (labels, cache_path) = result;
    let names: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    assert_eq!(names, vec!["@@zlib+//:z"]);
    assert!(cache_path.join("zlib+/BUILD.bazel").is_file());
    assert!(cache_path.join("zlib+/zlib.h").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_resolves_transitive_dep() {
    // foo@1.0 depends on bar@2.0. The workspace only declares foo;
    // bar should be discovered transitively.
    let foo_tarball = build_tar_gz(&[(
        "foo-1.0/MODULE.bazel",
        b"module(name=\"foo\", version=\"1.0\")
bazel_dep(name=\"bar\", version=\"2.0\")",
    )]);
    let bar_tarball = build_tar_gz(&[(
        "bar-2.0/MODULE.bazel",
        b"module(name=\"bar\", version=\"2.0\")",
    )]);
    let foo_sri = Integrity::compute(HashAlgorithm::Sha256, &foo_tarball).to_sri_string();
    let bar_sri = Integrity::compute(HashAlgorithm::Sha256, &bar_tarball).to_sri_string();

    let server = MockServer::start().await;
    let server_uri = server.uri();

    Mock::given(method("GET"))
        .and(path("/modules/foo/1.0/MODULE.bazel"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(
            b"module(name=\"foo\", version=\"1.0\")\nbazel_dep(name=\"bar\", version=\"2.0\")\n"
                .to_vec(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/modules/bar/2.0/MODULE.bazel"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"module(name=\"bar\", version=\"2.0\")".to_vec()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/modules/foo/1.0/source.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(
                serde_json::json!({
                    "url": format!("{server_uri}/archives/foo-1.0.tar.gz"),
                    "integrity": foo_sri,
                    "strip_prefix": "foo-1.0"
                })
                .to_string()
                .into_bytes(),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/modules/bar/2.0/source.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(
                serde_json::json!({
                    "url": format!("{server_uri}/archives/bar-2.0.tar.gz"),
                    "integrity": bar_sri,
                    "strip_prefix": "bar-2.0"
                })
                .to_string()
                .into_bytes(),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archives/foo-1.0.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(foo_tarball))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/archives/bar-2.0.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bar_tarball))
        .mount(&server)
        .await;

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
bazel_dep(name = "foo", version = "1.0")
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let cache = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache.path().to_path_buf();
    let registry = Registry::custom(server.uri());

    let report = tokio::task::spawn_blocking(move || -> Result<_, anyhow::Error> {
        let engine = EngineBuilder::new(workspace_path, Configuration::default())
            .with_bzlmod_cache(cache_path)
            .with_registry(registry)
            .build()?;
        engine.fetch_all()
    })
    .await
    .unwrap()
    .unwrap();

    assert_eq!(report.errors.len(), 0, "errors: {:?}", report.errors);
    let mut canonicals: Vec<&str> = report
        .materialised
        .iter()
        .map(|(c, _)| c.as_str())
        .collect();
    canonicals.sort();
    assert_eq!(canonicals, vec!["bar+", "foo+"]);
}
