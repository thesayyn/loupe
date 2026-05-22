//! Bazel-Central-Registry-format client.
//!
//! BCR layout (https://bcr.bazel.build/):
//!
//! ```text
//! modules/<name>/<version>/MODULE.bazel      # the module's own MODULE.bazel
//! modules/<name>/<version>/source.json       # url + integrity + strip_prefix
//! modules/<name>/<version>/patches/<f>.patch # optional
//! ```
//!
//! v1 fetches `source.json` and `MODULE.bazel` as raw bytes; the
//! caller (resolution.rs) parses them. Responses are content-addressed
//! by URL (the registry never reuses a version slot for different
//! bytes), so cache-forever is safe.
//!
//! Well-known modules (`bazel_tools`, `platforms`,
//! `local_config_platform`) aren't in BCR — they're hardcoded into
//! Bazel itself. Loupe serves them through embedded roots / vendor.
//! The resolver skips them before reaching this client.

use std::path::Path;
use std::path::PathBuf;

use crate::fetch::FetchError;
use crate::fetch::download;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("fetch: {0}")]
    Fetch(#[from] FetchError),

    #[error("registry returned non-JSON for {url}: {source}")]
    Json {
        url: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("missing required field `{0}` in source.json")]
    MissingField(&'static str),
}

#[derive(Debug, Clone)]
pub struct Registry {
    /// Base URL — `https://bcr.bazel.build` for the canonical registry.
    /// No trailing slash.
    pub url: String,
}

impl Registry {
    pub fn bcr() -> Self {
        Self {
            url: "https://bcr.bazel.build".to_string(),
        }
    }

    pub fn custom(url: impl Into<String>) -> Self {
        Self {
            url: url.into().trim_end_matches('/').to_string(),
        }
    }
}

/// Decoded `source.json` for one module version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSource {
    pub url: String,
    pub integrity: Option<String>,
    pub strip_prefix: Option<String>,
}

/// Fetch `<registry>/modules/<name>/<version>/MODULE.bazel` and cache
/// under `<cache_root>/registry/<safe-host>/modules/<name>/<version>/MODULE.bazel`.
/// Returns the raw bytes.
pub async fn fetch_module_bazel(
    client: &reqwest::Client,
    cache_root: &Path,
    registry: &Registry,
    name: &str,
    version: &str,
) -> Result<Vec<u8>, RegistryError> {
    let rel = format!("modules/{name}/{version}/MODULE.bazel");
    fetch_with_cache(client, cache_root, registry, &rel).await
}

/// Fetch + parse `<registry>/modules/<name>/<version>/source.json`.
pub async fn fetch_source(
    client: &reqwest::Client,
    cache_root: &Path,
    registry: &Registry,
    name: &str,
    version: &str,
) -> Result<ModuleSource, RegistryError> {
    let rel = format!("modules/{name}/{version}/source.json");
    let bytes = fetch_with_cache(client, cache_root, registry, &rel).await?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| RegistryError::Json {
        url: format!("{}/{rel}", registry.url),
        source: e,
    })?;
    let url = v
        .get("url")
        .and_then(|x| x.as_str())
        .ok_or(RegistryError::MissingField("url"))?
        .to_string();
    let integrity = v
        .get("integrity")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let strip_prefix = v
        .get("strip_prefix")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Ok(ModuleSource {
        url,
        integrity,
        strip_prefix,
    })
}

async fn fetch_with_cache(
    client: &reqwest::Client,
    cache_root: &Path,
    registry: &Registry,
    rel: &str,
) -> Result<Vec<u8>, RegistryError> {
    let cache_path = cache_path_for(cache_root, &registry.url, rel);
    if let Ok(bytes) = tokio::fs::read(&cache_path).await {
        return Ok(bytes);
    }
    let url = format!("{}/{rel}", registry.url);
    let bytes = download(client, &[url], None).await?;
    if let Some(parent) = cache_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&cache_path, &bytes).await?;
    Ok(bytes.to_vec())
}

fn cache_path_for(cache_root: &Path, registry_url: &str, rel: &str) -> PathBuf {
    let host = host_segment(registry_url);
    cache_root.join("registry").join(host).join(rel)
}

fn host_segment(url: &str) -> String {
    // Strip scheme and turn `host:port/path` → `host_port`. Good
    // enough for cache layout — we don't need bijective URL recovery,
    // just a filesystem-safe disambiguator per registry.
    let no_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_with_port = no_scheme.split('/').next().unwrap_or(no_scheme);
    host_with_port.replace(':', "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn fetches_source_json_and_caches() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "url": "https://example.com/skylib-1.7.1.tar.gz",
            "integrity": "sha256-abc",
            "strip_prefix": "bazel-skylib-1.7.1"
        });
        Mock::given(method("GET"))
            .and(path("/modules/bazel_skylib/1.7.1/source.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(serde_json::to_vec(&body).unwrap()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let registry = Registry::custom(server.uri());
        let cache = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();

        let s = fetch_source(&client, cache.path(), &registry, "bazel_skylib", "1.7.1")
            .await
            .unwrap();
        assert_eq!(s.url, "https://example.com/skylib-1.7.1.tar.gz");
        assert_eq!(s.integrity.as_deref(), Some("sha256-abc"));
        assert_eq!(s.strip_prefix.as_deref(), Some("bazel-skylib-1.7.1"));

        // Second call should hit cache (server's mock only allowed 1 hit).
        let s2 = fetch_source(&client, cache.path(), &registry, "bazel_skylib", "1.7.1")
            .await
            .unwrap();
        assert_eq!(s, s2);
    }

    #[tokio::test]
    async fn fetches_module_bazel_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/modules/foo/1.0/MODULE.bazel"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"module(name=\"foo\", version=\"1.0\")".to_vec()),
            )
            .mount(&server)
            .await;

        let registry = Registry::custom(server.uri());
        let cache = tempfile::tempdir().unwrap();
        let bytes = fetch_module_bazel(
            &reqwest::Client::new(),
            cache.path(),
            &registry,
            "foo",
            "1.0",
        )
        .await
        .unwrap();
        assert!(std::str::from_utf8(&bytes).unwrap().contains("foo"));
    }

    #[test]
    fn cache_path_is_filesystem_safe() {
        let p = cache_path_for(
            Path::new("/tmp/c"),
            "https://bcr.bazel.build:443",
            "modules/foo/1.0/source.json",
        );
        assert!(p.to_string_lossy().contains("bcr.bazel.build_443"));
    }
}
