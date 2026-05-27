//! Errors surfaced by `loupe-exec-cas`.
//!
//! Cache miss is a *typed* error variant carrying the digest the
//! lookup was issued for. Per the design contract, cache miss is a
//! hard failure — no fallback to fake content — and the digest in the
//! error tells the operator what to populate or which input changed.

use loupe_core::ac_key::Digest;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CasError {
    /// `ActionCache.GetActionResult(digest)` returned 404 / NOT_FOUND.
    /// The action's REAPI key isn't in the cache and we don't execute,
    /// so analysis can't continue past this action's output digests.
    #[error("REAPI cache miss for action digest {digest}")]
    CacheMiss { digest: Digest },

    /// `ContentAddressableStorage.Read(digest)` returned 404. Only
    /// surfaces when a caller asks to materialise an output's bytes;
    /// digest propagation never hits this path.
    #[error("REAPI CAS miss for blob digest {digest}")]
    CasMiss { digest: Digest },

    /// HTTP / network layer failure. Wraps the underlying reqwest
    /// error stringified — keeps `CasError: Clone` (DICE keys need
    /// their value types to be `Clone + Allocative`, and
    /// `reqwest::Error` is neither).
    #[error("REAPI transport error talking to {url}: {reason}")]
    Transport { url: String, reason: String },

    /// REAPI server returned non-404, non-200 — usually 5xx or 401.
    #[error("REAPI HTTP {status} from {url}: {body}")]
    HttpStatus {
        status: u16,
        url: String,
        body: String,
    },

    /// CAS integrity check failed — `sha256(bytes) != digest.hash`.
    /// Per REAPI v2 §3.1 the client MUST verify; surface a typed
    /// error rather than silently propagating corrupt data.
    #[error("CAS integrity failure for {expected}: got sha256({size_bytes} bytes) = {actual_hash}")]
    IntegrityFailure {
        expected: Digest,
        actual_hash: String,
        size_bytes: usize,
    },

    /// Wire-format decode failed. Field tag, truncation, or unknown
    /// wire type. Indicates either a non-REAPI server, a v3+ server
    /// we haven't accommodated, or corruption.
    #[error("REAPI proto decode error: {0}")]
    Decode(String),

    /// AC-key computation failed for an action whose digest we tried
    /// to look up. Wraps `loupe_core::ac_key::AcKeyError` stringified
    /// for the same `Clone` reason as `Transport`.
    #[error("AC-key compute failed: {0}")]
    AcKey(String),

    /// `OutputDigestKey` queried for a path that no action in the
    /// recorded set produces, AND the upstream cache lookup didn't
    /// surface it either. Usually means the action graph is
    /// incomplete (a missing edge) — defence-in-depth, since the
    /// engine should have rejected the analysis earlier.
    #[error("no producer action found for generated output {path}")]
    UnknownProducer { path: String },

    /// `ActionResultKey` got a result but the requested output path
    /// wasn't in `result.output_files` / `result.output_directories`.
    /// Either Bazel cached a partial result (shouldn't happen) or the
    /// path we computed doesn't match what Bazel used as the
    /// `output_paths` entry.
    #[error("output {path} not present in cached ActionResult for {action_digest}")]
    OutputNotInResult { path: String, action_digest: Digest },
}
