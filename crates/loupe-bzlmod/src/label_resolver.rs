//! Label resolution from inside a repo-rule impl.
//!
//! When a rule calls `ctx.path(Label("@foo//pkg:f"))`, the Starlark
//! eval is on a `spawn_blocking` thread but the underlying work needs
//! DICE (`MaterializedRepoKey(canonical)`) — which is async and lives
//! on the parent task that holds `&mut DiceComputations`. We bridge
//! the two with an mpsc actor: the eval thread does
//! `blocking_send(req)` + `blocking_recv()`, the async driver runs
//! `tokio::select!` between the blocking eval's join handle and the
//! request channel, performing the compute and replying.
//!
//! The repo-mapping lookup is done on the *eval* side from a per-spec
//! `apparent → canonical` map snapshotted at spec-construction time
//! (so loupe-bzlmod stays unaware of loupe-core's `ModuleGraph` /
//! `Label` types). The DICE side only sees a canonical name.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;

/// One label-resolution request: by the time it reaches the actor,
/// the apparent name has been mapped to a canonical via the rule's
/// defining-bzl `repo_mapping`. The actor only needs to materialise
/// the target repo and hand back its directory.
#[derive(Debug, Clone)]
pub struct LabelReq {
    pub canonical: String,
    pub package: String,
    pub name: String,
}

/// Reply: the resolved on-disk path inside the materialised repo,
/// or an error string.
pub type LabelResp = Result<PathBuf, String>;

/// What `ctx.path(Label(...))` and friends call. The resolver does
/// not know about Starlark — it takes a parsed apparent label and
/// the source repo's mapping, and returns a path + the records
/// that should land in the marker.
pub trait LabelResolver: Send + Sync {
    /// Resolve `apparent_repo` (the bit between `@` and `//` in a
    /// label string) into a materialised on-disk path, joining the
    /// label's package and name. Returns the canonical name on
    /// success so the caller can record an
    /// `apparent → canonical` mapping observation.
    fn resolve(
        &self,
        apparent_repo: &str,
        package: &str,
        name: &str,
        repo_mapping: &BTreeMap<String, String>,
    ) -> Result<ResolvedLabel, String>;
}

#[derive(Debug, Clone)]
pub struct ResolvedLabel {
    pub path: PathBuf,
    pub apparent: String,
    pub canonical: String,
}

/// Default resolver — raises with a typed error. Installed when a
/// rule eval has no DICE-backed materialiser to talk to (e.g. unit
/// tests that build a `RepoCtxState` directly).
pub struct UnresolvedLabelResolver;

impl LabelResolver for UnresolvedLabelResolver {
    fn resolve(
        &self,
        apparent: &str,
        _package: &str,
        _name: &str,
        _repo_mapping: &BTreeMap<String, String>,
    ) -> Result<ResolvedLabel, String> {
        Err(format!(
            "label resolution unavailable in this context \
             (apparent repo `@{apparent}`) — pass a `RepoSpec` with a \
             repo_mapping and run via DICE"
        ))
    }
}

/// Resolver that does the per-spec mapping lookup locally, then
/// dispatches the canonical-name materialisation to a parent
/// async task over an mpsc channel.
pub struct ChannelLabelResolver {
    pub req_tx: Sender<LabelReq>,
    pub resp_rx: Arc<Mutex<Receiver<LabelResp>>>,
}

impl LabelResolver for ChannelLabelResolver {
    fn resolve(
        &self,
        apparent: &str,
        package: &str,
        name: &str,
        repo_mapping: &BTreeMap<String, String>,
    ) -> Result<ResolvedLabel, String> {
        let canonical = repo_mapping.get(apparent).cloned().ok_or_else(|| {
            format!(
                "no repo_mapping entry for apparent name `@{apparent}` in the rule's \
                 defining .bzl — declare a bazel_dep or use the canonical form"
            )
        })?;
        let req = LabelReq {
            canonical: canonical.clone(),
            package: package.to_string(),
            name: name.to_string(),
        };
        self.req_tx
            .blocking_send(req)
            .map_err(|e| format!("label resolver request channel closed: {e}"))?;
        let path = self
            .resp_rx
            .blocking_lock()
            .blocking_recv()
            .ok_or_else(|| "label resolver response channel closed without reply".to_string())??;
        Ok(ResolvedLabel {
            path,
            apparent: apparent.to_string(),
            canonical,
        })
    }
}
