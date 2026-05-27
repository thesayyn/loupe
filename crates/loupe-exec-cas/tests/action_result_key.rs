//! Integration test for `ActionResultKey` — proves the DICE Key
//! plumbing into the `CacheClient` works end-to-end:
//!
//! 1. Build a DICE session with a `StubCacheClient` injected.
//! 2. Pre-populate the stub with an `ActionResult` keyed by a
//!    synthetic digest.
//! 3. `ctx.compute(ActionResultKey(digest))` returns the same
//!    `ActionResult`. Re-issuing the same key hits the DICE cache —
//!    no second HTTP / stub call.
//! 4. A missing digest returns a `CacheMiss` error.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dice::Dice;
use loupe_core::ac_key::Digest;
use loupe_exec_cas::client::{CacheClient, StubCacheClient};
use loupe_exec_cas::error::CasError;
use loupe_exec_cas::proto::{ActionResult, OutputFile};
use loupe_exec_cas::{ActionResultKey, SetCacheClient};

/// Stub wrapper that counts `get_action_result` calls so we can
/// assert DICE memoisation eliminates duplicate fetches.
struct CountingStub {
    inner: StubCacheClient,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl CacheClient for CountingStub {
    async fn get_action_result(&self, digest: &Digest) -> Result<ActionResult, CasError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.get_action_result(digest).await
    }
    async fn get_cas_blob(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.inner.get_cas_blob(digest).await
    }
}

fn build_dice(client: Arc<dyn CacheClient>) -> Arc<Dice> {
    let mut builder = Dice::builder();
    builder.set_cache_client(client);
    builder.build(dice::DetectCycles::Enabled)
}

#[tokio::test]
async fn action_result_key_hits_cache_and_memoises_per_session() {
    // Pre-populate the stub with one ActionResult.
    let stub = StubCacheClient::new();
    let action_digest = Digest::of_bytes(b"action-key-bytes");
    let cas_digest = stub.insert_cas(b"hello".to_vec());
    stub.insert_ac(
        action_digest.clone(),
        ActionResult {
            output_files: vec![OutputFile {
                path: "bazel-out/k8-fastbuild/bin/pkg/out.bin".to_string(),
                digest: cas_digest,
                is_executable: false,
            }],
            output_directories: vec![],
            exit_code: 0,
            stdout_digest: None,
            stderr_digest: None,
        },
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let counting = Arc::new(CountingStub {
        inner: stub,
        calls: calls.clone(),
    });
    let dice = build_dice(counting);
    let mut ctx = dice.updater().commit().await;

    // First lookup: hits the stub.
    let r1 = ctx
        .compute(&ActionResultKey(action_digest.clone()))
        .await
        .unwrap();
    assert!(matches!(r1, loupe_core::dice_keys::SharedResult::Ok(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Second lookup of the *same* key — DICE returns cached value,
    // no second stub call.
    let r2 = ctx
        .compute(&ActionResultKey(action_digest.clone()))
        .await
        .unwrap();
    assert!(matches!(r2, loupe_core::dice_keys::SharedResult::Ok(_)));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "DICE must memoise ActionResultKey within a session — second \
         compute should not re-call the cache client"
    );
}

#[tokio::test]
async fn action_result_key_surfaces_cache_miss_as_typed_error() {
    let stub = Arc::new(StubCacheClient::new());
    let dice = build_dice(stub);
    let mut ctx = dice.updater().commit().await;

    let unknown = Digest::of_bytes(b"never-populated");
    let result = ctx.compute(&ActionResultKey(unknown)).await.unwrap();
    match result {
        loupe_core::dice_keys::SharedResult::Err(msg) => {
            // SharedResult stringifies the underlying CasError; the
            // "cache miss" wording is the contract surfaces should
            // grep for / display.
            assert!(
                msg.contains("cache miss") || msg.contains("CacheMiss"),
                "expected cache-miss surfaced through SharedResult::Err, got {msg}",
            );
        }
        other => panic!("expected SharedResult::Err, got {other:?}"),
    }
}
