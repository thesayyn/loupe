//! REAPI v2 cache client surface.
//!
//! Two operations:
//!
//!   1. `get_action_result(digest) -> ActionResult` — the AC lookup
//!      driving the entire hydration flow. Returns digests of an
//!      action's outputs.
//!   2. `get_cas_blob(digest) -> Vec<u8>` — content fetch for cases
//!      where a caller wants the actual bytes (CLI output, runfiles
//!      staging). Digest propagation never calls this — DICE keys
//!      read `ActionResult.output_files[i].digest` directly.
//!
//! The trait stays minimal: `async fn` returning `Result<_, CasError>`
//! with `Send + Sync + 'static` bounds so DICE keys can hold an
//! `Arc<dyn CacheClient>` in `DiceData` and clone it across keys.

use std::sync::Arc;

use async_trait::async_trait;
use loupe_core::ac_key::Digest;
use sha2::{Digest as _, Sha256};

use crate::error::CasError;
use crate::proto;
use crate::proto::ActionResult;

#[async_trait]
pub trait CacheClient: Send + Sync + 'static {
    /// REAPI `ActionCache.GetActionResult(action_digest)`.
    /// 404 maps to [`CasError::CacheMiss`]; any other HTTP / network
    /// failure maps to [`CasError::Transport`] /
    /// [`CasError::HttpStatus`].
    async fn get_action_result(&self, digest: &Digest) -> Result<ActionResult, CasError>;

    /// REAPI `ContentAddressableStorage.Read(blob_digest)`. Verifies
    /// `sha256(bytes) == digest.hash` before returning — per REAPI
    /// §3.1 the client MUST verify, and a silent corruption pass-
    /// through would poison every downstream digest.
    async fn get_cas_blob(&self, digest: &Digest) -> Result<Vec<u8>, CasError>;
}

/// HTTP-flavoured REAPI client. Speaks the `bazel-remote` HTTP
/// surface — `<base>/ac/<hash>` and `<base>/cas/<hash>` endpoints
/// the bazelbuild/bazel-remote project (and most simple proxies)
/// exposes. The gRPC ByteStream / RemoteExecution.proto path is a
/// separate impl behind the same trait, lands when there's a target
/// that needs it.
///
/// v1 carries no retry, no auth, no compression. Each is a documented
/// STUB — a production deployment would layer at least a retry-with-
/// backoff over connection-level failures and an `Authorization`
/// header for hosted caches.
#[derive(Clone)]
pub struct BazelRemoteHttpClient {
    inner: Arc<BazelRemoteHttpClientInner>,
}

struct BazelRemoteHttpClientInner {
    base_url: String,
    http: reqwest::Client,
}

impl BazelRemoteHttpClient {
    /// `base_url` is the cache root, e.g. `http://localhost:8080`.
    /// Trailing slash optional — normalised on construction.
    pub fn new(base_url: impl Into<String>) -> Self {
        let mut base = base_url.into();
        while base.ends_with('/') {
            base.pop();
        }
        BazelRemoteHttpClient {
            inner: Arc::new(BazelRemoteHttpClientInner {
                base_url: base,
                http: reqwest::Client::new(),
            }),
        }
    }
}

#[async_trait]
impl CacheClient for BazelRemoteHttpClient {
    async fn get_action_result(&self, digest: &Digest) -> Result<ActionResult, CasError> {
        let url = format!("{}/ac/{}", self.inner.base_url, digest.hash);
        let resp = self
            .inner
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| CasError::Transport {
                url: url.clone(),
                reason: e.to_string(),
            })?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(CasError::CacheMiss {
                digest: digest.clone(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CasError::HttpStatus {
                status: status.as_u16(),
                url,
                body,
            });
        }
        let bytes = resp.bytes().await.map_err(|e| CasError::Transport {
            url,
            reason: e.to_string(),
        })?;
        proto::decode_action_result(&bytes)
    }

    async fn get_cas_blob(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let url = format!("{}/cas/{}", self.inner.base_url, digest.hash);
        let resp = self
            .inner
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| CasError::Transport {
                url: url.clone(),
                reason: e.to_string(),
            })?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(CasError::CasMiss {
                digest: digest.clone(),
            });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(CasError::HttpStatus {
                status: status.as_u16(),
                url,
                body,
            });
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| CasError::Transport {
                url,
                reason: e.to_string(),
            })?
            .to_vec();
        verify_cas_integrity(digest, &bytes)?;
        Ok(bytes)
    }
}

/// Verify `sha256(bytes) == digest.hash`. REAPI v2 §3.1 requires it;
/// a silent pass-through would let a corrupted blob poison every
/// downstream merkle-tree digest.
fn verify_cas_integrity(expected: &Digest, bytes: &[u8]) -> Result<(), CasError> {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut actual = String::with_capacity(out.len() * 2);
    for b in out {
        actual.push_str(&format!("{b:02x}"));
    }
    if actual == expected.hash {
        Ok(())
    } else {
        Err(CasError::IntegrityFailure {
            expected: expected.clone(),
            actual_hash: actual,
            size_bytes: bytes.len(),
        })
    }
}

/// In-memory `CacheClient` for tests. The hydrate logic and DICE keys
/// are exercised against this; the HTTP impl is a thin wrapper around
/// reqwest that we test against a real `bazel-remote` separately.
pub struct StubCacheClient {
    pub ac: std::sync::Mutex<std::collections::BTreeMap<String, ActionResult>>,
    pub cas: std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
}

impl Default for StubCacheClient {
    fn default() -> Self {
        StubCacheClient::new()
    }
}

impl StubCacheClient {
    pub fn new() -> Self {
        StubCacheClient {
            ac: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            cas: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    pub fn insert_ac(&self, digest: Digest, result: ActionResult) {
        self.ac.lock().unwrap().insert(digest.hash, result);
    }

    pub fn insert_cas(&self, bytes: Vec<u8>) -> Digest {
        let d = Digest::of_bytes(&bytes);
        self.cas.lock().unwrap().insert(d.hash.clone(), bytes);
        d
    }
}

#[async_trait]
impl CacheClient for StubCacheClient {
    async fn get_action_result(&self, digest: &Digest) -> Result<ActionResult, CasError> {
        match self.ac.lock().unwrap().get(&digest.hash) {
            Some(ar) => Ok(ar.clone()),
            None => Err(CasError::CacheMiss {
                digest: digest.clone(),
            }),
        }
    }

    async fn get_cas_blob(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        match self.cas.lock().unwrap().get(&digest.hash) {
            Some(b) => Ok(b.clone()),
            None => Err(CasError::CasMiss {
                digest: digest.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_round_trips_ac_and_cas() {
        let client = StubCacheClient::new();
        let blob = b"hello world".to_vec();
        let cas_digest = client.insert_cas(blob.clone());
        let back = client.get_cas_blob(&cas_digest).await.unwrap();
        assert_eq!(back, blob);

        let ar = ActionResult {
            output_files: vec![proto::OutputFile {
                path: "out".to_string(),
                digest: cas_digest.clone(),
                is_executable: false,
            }],
            output_directories: vec![],
            exit_code: 0,
            stdout_digest: None,
            stderr_digest: None,
        };
        let action_digest = Digest::of_bytes(b"action-key");
        client.insert_ac(action_digest.clone(), ar.clone());
        let back = client.get_action_result(&action_digest).await.unwrap();
        assert_eq!(back, ar);
    }

    #[tokio::test]
    async fn stub_returns_typed_cache_miss() {
        let client = StubCacheClient::new();
        let unknown = Digest::of_bytes(b"unknown");
        let err = client.get_action_result(&unknown).await.unwrap_err();
        assert!(matches!(err, CasError::CacheMiss { .. }));
        let err = client.get_cas_blob(&unknown).await.unwrap_err();
        assert!(matches!(err, CasError::CasMiss { .. }));
    }

    #[test]
    fn integrity_check_catches_corruption() {
        let expected = Digest::of_bytes(b"correct");
        let corrupted = b"different";
        let err = verify_cas_integrity(&expected, corrupted).unwrap_err();
        assert!(matches!(err, CasError::IntegrityFailure { .. }));
        verify_cas_integrity(&expected, b"correct").unwrap();
    }

    #[test]
    fn http_client_base_url_normalises_trailing_slash() {
        let a = BazelRemoteHttpClient::new("http://localhost:8080");
        let b = BazelRemoteHttpClient::new("http://localhost:8080/");
        let c = BazelRemoteHttpClient::new("http://localhost:8080///");
        assert_eq!(a.inner.base_url, "http://localhost:8080");
        assert_eq!(b.inner.base_url, "http://localhost:8080");
        assert_eq!(c.inner.base_url, "http://localhost:8080");
    }
}
