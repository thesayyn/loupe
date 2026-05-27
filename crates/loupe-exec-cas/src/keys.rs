//! DICE keys that hydrate REAPI Action digests and ActionResults.
//!
//! Four keys, mutually composed:
//!
//! ```text
//!                      ActionDigestKey(action_ref)
//!                                │
//!                  ┌─────────────┼──────────────┐
//!                  │             │              │
//!                  ▼             ▼              ▼
//!         SourceFileDigest  OutputDigest   OutputDigest
//!                                │              │
//!                                │  (if producer is Run/RunShell)
//!                                ▼
//!                       ActionResultKey(digest)
//!                                │
//!                                ▼  (recurses through ActionDigestKey
//!                                    for the producer)
//! ```
//!
//! DICE supplies the three non-functional requirements every key
//! inherits for free:
//!
//!   - **Parallelism.** Within one key's compute fn, independent
//!     inputs fan out via `try_join_all` over `ctx.compute` futures.
//!     DICE schedules the underlying keys concurrently, bounded only
//!     by data dependencies.
//!   - **Caching.** Every digest, every ActionResult is memoised by
//!     DICE for the session. Two consumers of the same upstream
//!     output trigger one digest computation; two queries about the
//!     same action trigger one HTTP call.
//!   - **Incrementality.** Source-file edits invalidate exactly the
//!     keys downstream of them via the DICE dependency edge graph.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use dice::{
    CancellationContext, DiceComputations, DiceData, DiceDataBuilder, Key, NoValueSerialize,
    ValueSerialize,
};
use pagable::{Pagable, pagable_typetag};

use loupe_core::ac_key::{Digest, compute_action_cache_key};
use loupe_core::action::{
    Action, ActionKind, TerminalContent, index_actions_by_output, resolve_symlink_chain,
};
use loupe_core::analysis::{AnalyzedTarget, Configuration};
use loupe_core::dice_keys::{
    AnalysisKey, FileContentKey, FileMetadataKey, HasWorkspaceRoot, SharedResult,
};
use loupe_core::starlark_types::file::File;
use loupe_labels::Label;

use crate::client::CacheClient;
use crate::error::CasError;
use crate::global_index::{GlobalActionIndexKey, ProducerEntry, ProducerLookup};
use crate::proto::{self, ActionResult, Tree};

// ---- DiceData wiring for the CacheClient ----

#[derive(Clone, Allocative)]
struct CacheClientHandle(#[allocative(skip)] Arc<dyn CacheClient>);

pub trait HasCacheClient {
    fn cache_client(&self) -> Arc<dyn CacheClient>;
}

impl HasCacheClient for DiceData {
    fn cache_client(&self) -> Arc<dyn CacheClient> {
        self.get::<CacheClientHandle>()
            .expect(
                "CacheClient was not set on DiceData — call \
                 SetCacheClient::set_cache_client before issuing \
                 any ActionResultKey",
            )
            .0
            .clone()
    }
}

pub trait SetCacheClient {
    fn set_cache_client(&mut self, client: Arc<dyn CacheClient>);
}

impl SetCacheClient for DiceDataBuilder {
    fn set_cache_client(&mut self, client: Arc<dyn CacheClient>) {
        self.set(CacheClientHandle(client));
    }
}

// ---- value types ----

/// Locates one recorded action by `(target, configuration, index)`.
/// Compact, hashable — the right shape to embed in a DICE key.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ActionRef({target}, {config}, {action_index})")]
pub struct ActionRef {
    pub target: Label,
    pub config: Configuration,
    pub action_index: usize,
}

/// Resolved input: digest + REAPI `is_executable` bit. Returned by
/// `SourceFileDigestKey` and `OutputDigestKey`.
#[derive(Clone, Debug, PartialEq, Eq, Allocative)]
pub struct ResolvedInput {
    pub digest: Digest,
    pub is_executable: bool,
}

// ---- SourceFileDigestKey ----

/// Sha256 of a source file's bytes + its POSIX `+x` bit. Composes the
/// existing `FileContentKey` and `FileMetadataKey` so the file is
/// read once per session and DICE invalidates downstream keys when
/// it changes.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("SourceFileDigestKey({})", _0.display())]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct SourceFileDigestKey(pub PathBuf);

#[async_trait]
impl Key for SourceFileDigestKey {
    type Value = SharedResult<ResolvedInput>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        // Fetch content and metadata concurrently via DICE's
        // `try_compute2` — independent keys so DICE schedules them
        // in parallel on the underlying executor.
        let path1 = self.0.clone();
        let path2 = self.0.clone();
        let result: Result<(Arc<Vec<u8>>, Arc<loupe_core::dice_keys::FileMetadata>), String> = ctx
            .try_compute2(
                move |c| {
                    use futures::FutureExt;
                    async move {
                        match c.compute(&FileContentKey(path1)).await {
                            Ok(SharedResult::Ok(b)) => Ok(b),
                            Ok(SharedResult::Err(e)) => Err((*e).clone()),
                            Err(e) => Err(format!("DICE compute error: {e}")),
                        }
                    }
                    .boxed()
                },
                move |c| {
                    use futures::FutureExt;
                    async move {
                        match c.compute(&FileMetadataKey(path2)).await {
                            Ok(SharedResult::Ok(m)) => Ok(m),
                            Ok(SharedResult::Err(e)) => Err((*e).clone()),
                            Err(e) => Err(format!("DICE compute error: {e}")),
                        }
                    }
                    .boxed()
                },
            )
            .await;
        match result {
            Ok((content, meta)) => SharedResult::ok(ResolvedInput {
                digest: Digest::of_bytes(&content),
                is_executable: meta.is_executable,
            }),
            Err(e) => SharedResult::err(e),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (SharedResult::Ok(a), SharedResult::Ok(b)) => a == b,
            (SharedResult::Err(a), SharedResult::Err(b)) => a == b,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

// ---- ActionResultKey ----

/// REAPI cache fetch for one action digest. Single network round trip
/// per unique digest per session; DICE caches the decoded `ActionResult`.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ActionResultKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ActionResultKey(pub Digest);

#[async_trait]
impl Key for ActionResultKey {
    type Value = SharedResult<ActionResult>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let client = ctx.global_data().cache_client();
        match client.get_action_result(&self.0).await {
            Ok(ar) => SharedResult::ok(ar),
            Err(e) => SharedResult::err(format!("{e}")),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (SharedResult::Ok(a), SharedResult::Ok(b)) => a == b,
            (SharedResult::Err(a), SharedResult::Err(b)) => a == b,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

// ---- OutputDigestKey ----

/// Digest of a *generated* file's content. The output is identified
/// by `(producer, output_index)`: which recorded action emits it,
/// and which of its outputs we want. Branches on the producer kind:
///
///   - `Write`: hash the recorded `content` locally.
///   - `ExpandTemplate`: apply substitutions via
///     `materialise_expand_template`, hash the result.
///   - `Symlink`: walk `resolve_symlink_chain` to the terminal, then
///     recurse via the appropriate kind branch.
///   - `Run` / `RunShell`: `ctx.compute(ActionDigestKey(producer))`,
///     `ctx.compute(ActionResultKey(action_digest))`, look up the
///     output path in `result.output_files`, return its digest.
///
/// Caller supplies the producer `ActionRef` and which output to
/// resolve. Discovery of the producer from a `File` alone is the
/// higher-level `HydratedTargetKey`'s job; v1 expects callers to
/// pre-walk the target's action list.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("OutputDigestKey({producer}#{output_index})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct OutputDigestKey {
    pub producer: ActionRef,
    pub output_index: usize,
}

#[async_trait]
impl Key for OutputDigestKey {
    type Value = SharedResult<ResolvedInput>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let analyzed = match ctx
            .compute(&AnalysisKey(
                self.producer.target.clone(),
                self.producer.config.clone(),
            ))
            .await
        {
            Ok(SharedResult::Ok(a)) => a,
            Ok(SharedResult::Err(e)) => return SharedResult::err((*e).clone()),
            Err(e) => return SharedResult::err(format!("DICE compute error: {e}")),
        };
        let action = match analyzed.actions.get(self.producer.action_index) {
            Some(a) => a.clone(),
            None => {
                return SharedResult::err(format!(
                    "ActionRef out of bounds: target {} has {} actions, asked for index {}",
                    self.producer.target,
                    analyzed.actions.len(),
                    self.producer.action_index,
                ));
            }
        };
        let output_file = match action.outputs.get(self.output_index) {
            Some(f) => f.clone(),
            None => {
                return SharedResult::err(format!(
                    "output_index out of bounds: action {:?} has {} outputs, asked for {}",
                    action.mnemonic,
                    action.outputs.len(),
                    self.output_index,
                ));
            }
        };
        match resolve_output_digest(
            &action,
            &output_file,
            &analyzed.actions,
            &self.producer,
            ctx,
        )
        .await
        {
            Ok(resolved) => SharedResult::ok(resolved),
            Err(e) => SharedResult::err(format!("{e}")),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (SharedResult::Ok(a), SharedResult::Ok(b)) => a == b,
            (SharedResult::Err(a), SharedResult::Err(b)) => a == b,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Resolve `file`'s digest given the producer action that emits it.
/// Async helper used inside both `OutputDigestKey::compute` and
/// `ActionDigestKey::compute` to handle the producer-branching logic
/// in one place.
async fn resolve_output_digest(
    action: &Action,
    file: &File,
    target_actions: &[Action],
    producer: &ActionRef,
    ctx: &mut DiceComputations<'_>,
) -> Result<ResolvedInput, CasError> {
    match &action.kind {
        ActionKind::Write {
            content,
            is_executable,
        } => Ok(ResolvedInput {
            digest: Digest::of_bytes(content.as_bytes()),
            is_executable: *is_executable || file.is_executable(),
        }),
        ActionKind::ExpandTemplate {
            substitutions,
            is_executable,
            ..
        } => {
            // The template body itself isn't on the recorded Action
            // surface in a derivable form (PR-tracked STUB on the
            // engine side). Hash the substitutions deterministically
            // as a stand-in until the engine surfaces the template
            // body — at which point this branch flips to
            // `materialise_expand_template`.
            let mut buf = Vec::new();
            for (k, v) in substitutions {
                buf.extend_from_slice(k.as_bytes());
                buf.push(0);
                buf.extend_from_slice(v.as_bytes());
                buf.push(0);
            }
            Ok(ResolvedInput {
                digest: Digest::of_bytes(&buf),
                is_executable: *is_executable || file.is_executable(),
            })
        }
        ActionKind::Symlink { .. } => {
            // Walk the chain to its terminal kind. The index is
            // built fresh per call — cheap (BTreeMap over Action
            // refs in the producing target) and avoids stuffing
            // another bag of state into DiceData for v1.
            let idx = index_actions_by_output(target_actions);
            match resolve_symlink_chain(file, &idx) {
                TerminalContent::Source(_) => {
                    // Symlink chain bottoms out at a source-or-
                    // unindexed File. Without the scope index
                    // covering the source we can't resolve here —
                    // surface a typed error rather than guessing.
                    Err(CasError::UnknownProducer { path: file.path() })
                }
                TerminalContent::Write {
                    content,
                    is_executable,
                } => Ok(ResolvedInput {
                    digest: Digest::of_bytes(content.as_bytes()),
                    is_executable: is_executable || file.is_executable(),
                }),
                TerminalContent::ExpandTemplate {
                    substitutions,
                    is_executable,
                    ..
                } => {
                    let mut buf = Vec::new();
                    for (k, v) in substitutions {
                        buf.extend_from_slice(k.as_bytes());
                        buf.push(0);
                        buf.extend_from_slice(v.as_bytes());
                        buf.push(0);
                    }
                    Ok(ResolvedInput {
                        digest: Digest::of_bytes(&buf),
                        is_executable: is_executable || file.is_executable(),
                    })
                }
                TerminalContent::Opaque(opaque_file) => {
                    // Cycle, depth-limit, or terminal Run output we
                    // can't resolve through the symlink walker — so
                    // a `Run`/`RunShell` action drives the cache
                    // lookup. The terminal kind branches identically
                    // to the direct Run/RunShell case below; produce
                    // a typed error here pointing to the opaque file
                    // for now. Lifting this needs the scope index
                    // (which producer ActionRef produces
                    // `opaque_file`'s path).
                    Err(CasError::UnknownProducer {
                        path: opaque_file.path(),
                    })
                }
            }
        }
        ActionKind::Run { .. } | ActionKind::RunShell { .. } => {
            // Recurse: compute the producer's REAPI Action digest
            // (which itself may chain through more OutputDigestKeys
            // for nested generated inputs), fetch the ActionResult
            // from the cache, and pluck this file's digest out.
            let action_digest_value = ctx
                .compute(&ActionDigestKey(producer.clone()))
                .await
                .map_err(|e| CasError::AcKey(format!("DICE compute error: {e}")))?;
            let action_digest = match action_digest_value {
                SharedResult::Ok(d) => d,
                SharedResult::Err(e) => return Err(CasError::AcKey((*e).clone())),
            };
            let result_value = ctx
                .compute(&ActionResultKey((*action_digest).clone()))
                .await
                .map_err(|e| CasError::AcKey(format!("DICE compute error: {e}")))?;
            let result = match result_value {
                SharedResult::Ok(r) => r,
                SharedResult::Err(e) => return Err(CasError::AcKey((*e).clone())),
            };
            let path = file.path();
            if let Some(of) = result.output_files.iter().find(|of| of.path == path) {
                return Ok(ResolvedInput {
                    digest: of.digest.clone(),
                    is_executable: of.is_executable,
                });
            }
            // Tree-artifact ROOT (the input path is the directory
            // itself, not a sub-path). Surface the `tree_digest`
            // verbatim — the digest of the serialised REAPI `Tree`
            // proto. Downstream consumers that pre-compute their own
            // merkle tree from this digest get a well-defined value;
            // a deeper fix here is the encoder learning to insert a
            // tree-artifact input as a `DirectoryNode { name, digest }`
            // (where `digest` is the Tree's root `Directory` digest,
            // REAPI v2.2's `root_directory_digest`) instead of a
            // `FileNode`. Returning a digest unblocks the path; the
            // encoder shape is tracked as a known gap.
            if let Some(od) = result.output_directories.iter().find(|od| od.path == path) {
                return Ok(ResolvedInput {
                    digest: od.tree_digest.clone(),
                    is_executable: false,
                });
            }
            Err(CasError::OutputNotInResult {
                path,
                action_digest: (*action_digest).clone(),
            })
        }
    }
}

// ---- ActionDigestKey ----

/// REAPI Action digest for one recorded action.
///
/// Composes by:
///
/// 1. `ctx.compute(AnalysisKey(target, config))` for the producer's
///    `AnalyzedTarget` (DICE-cached; same target queried across keys
///    re-uses the existing computation).
/// 2. Per input — concurrently — `ctx.compute(SourceFileDigestKey)`
///    for sources and `ctx.compute(OutputDigestKey)` for generated.
///    `try_join_all` over the input list schedules every leaf compute
///    in one DICE batch; the entire input root resolves in one
///    parallel pass.
/// 3. Calls `compute_action_cache_key` with a pre-populated sync
///    resolver. The REAPI Action proto encoding (Platform merge,
///    default_shell_env folding, merkle tree build) lives in
///    `loupe_core::ac_key`; this key is the DICE wrapper.
///
/// The crate's caller is responsible for knowing the producer
/// `ActionRef` of each generated input. The follow-up
/// `HydratedTargetKey` walks the target's transitive analysis to
/// build that index automatically.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ActionDigestKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ActionDigestKey(pub ActionRef);

#[async_trait]
impl Key for ActionDigestKey {
    type Value = SharedResult<Digest>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let analyzed: Arc<AnalyzedTarget> = match ctx
            .compute(&AnalysisKey(self.0.target.clone(), self.0.config.clone()))
            .await
        {
            Ok(SharedResult::Ok(a)) => a,
            Ok(SharedResult::Err(e)) => return SharedResult::err((*e).clone()),
            Err(e) => return SharedResult::err(format!("DICE compute error: {e}")),
        };
        let action = match analyzed.actions.get(self.0.action_index) {
            Some(a) => a.clone(),
            None => {
                return SharedResult::err(format!(
                    "ActionRef out of bounds: {} has {} actions, asked for index {}",
                    self.0.target,
                    analyzed.actions.len(),
                    self.0.action_index,
                ));
            }
        };
        match compute_action_digest_inner(&action, &analyzed, &self.0, ctx).await {
            Ok(d) => SharedResult::ok(d),
            Err(e) => SharedResult::err(format!("{e}")),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (SharedResult::Ok(a), SharedResult::Ok(b)) => a == b,
            (SharedResult::Err(a), SharedResult::Err(b)) => a == b,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn compute_action_digest_inner(
    action: &Action,
    analyzed: &AnalyzedTarget,
    self_ref: &ActionRef,
    ctx: &mut DiceComputations<'_>,
) -> Result<Digest, CasError> {
    // Pre-resolve every input via DICE. For each input we know the
    // request shape (SourceFileDigestKey vs OutputDigestKey) before
    // dispatch, so we classify and resolve in one parallel batch via
    // `try_compute_join`. DICE schedules sibling computes
    // concurrently — independent source-file hashes and independent
    // upstream-action lookups all run at once.
    let workspace = ctx.global_data().workspace_root().to_path_buf();

    // The local-target action list covers same-target chains; for
    // every other (cross-target / tree-artifact sub-path) input we
    // consult the global index. Materialise it once per action — DICE
    // memoises the underlying key so the per-action overhead is a
    // single hashtable hit after the first action runs.
    let global_index_arc = compute_global_index_if_needed(ctx, action, analyzed, &self_ref.config)
        .await
        .map_err(CasError::AcKey)?;

    // Pre-classify inputs into request items. Each item carries
    // everything its mapper invocation needs — the mapper closure
    // is `Copy` (DICE requirement) and stateless.
    let requests: Vec<InputRequest> = action
        .inputs
        .iter()
        .map(|input| {
            let path = input.path();
            if input.is_source() {
                InputRequest::Source {
                    path: path.clone(),
                    disk_path: workspace.join(&path),
                }
            } else {
                classify_generated(&path, analyzed, self_ref, global_index_arc.as_deref())
            }
        })
        .collect();

    let resolved = ctx
        .try_compute_join(requests, |c, request| {
            use futures::FutureExt;
            async move {
                match request {
                    InputRequest::Source { path, disk_path } => {
                        let result = c
                            .compute(&SourceFileDigestKey(disk_path))
                            .await
                            .map_err(|e| format!("DICE compute error: {e}"))?;
                        match result {
                            SharedResult::Ok(r) => Ok((path, (*r).clone())),
                            SharedResult::Err(e) => Err((*e).clone()),
                        }
                    }
                    InputRequest::Output {
                        path,
                        producer,
                        output_index,
                    } => {
                        let result = c
                            .compute(&OutputDigestKey {
                                producer: *producer,
                                output_index,
                            })
                            .await
                            .map_err(|e| format!("DICE compute error: {e}"))?;
                        match result {
                            SharedResult::Ok(r) => Ok((path, (*r).clone())),
                            SharedResult::Err(e) => Err((*e).clone()),
                        }
                    }
                    InputRequest::TreeSubPath {
                        path,
                        producer,
                        output_index,
                        sub_path,
                    } => match resolve_tree_subpath(c, *producer, output_index, &sub_path).await {
                        Ok(r) => Ok((path, r)),
                        Err(e) => Err(format!("{e}")),
                    },
                    InputRequest::UnknownGenerated {
                        path,
                        target_display,
                    } => Err(format!(
                        "no producer action found in target {target_display} for \
                         generated input {path}",
                    )),
                }
            }
            .boxed()
        })
        .await
        .map_err(CasError::AcKey)?;

    let inputs_by_path: BTreeMap<String, (Digest, bool)> = resolved
        .into_iter()
        .map(|(path, r)| (path, (r.digest, r.is_executable)))
        .collect();
    compute_action_cache_key(action, &analyzed.configuration, &inputs_by_path)
        .map_err(|e| CasError::AcKey(format!("{e}")))
}

/// Internal classification of an `Action::inputs[i]`. Each variant
/// carries everything the parallel `try_compute_join` mapper needs
/// for that item — the mapper closure stays stateless and `Copy`.
/// `ActionRef` is boxed because it carries `Configuration` (heavy);
/// keeps the enum's stack footprint sane.
#[derive(Clone)]
enum InputRequest {
    Source {
        path: String,
        disk_path: PathBuf,
    },
    Output {
        path: String,
        producer: Box<ActionRef>,
        output_index: usize,
    },
    /// Input path is `<tree_artifact_root>/<sub_path>`: look up the
    /// producer's REAPI ActionResult, grab the tree's CAS digest from
    /// `output_directories`, fetch + decode the `Tree` blob, and find
    /// `sub_path` inside it.
    TreeSubPath {
        /// The full input path (used as the key the resolver expects).
        path: String,
        producer: Box<ActionRef>,
        /// Index of the tree-artifact output on the producer.
        output_index: usize,
        /// Path inside the tree artifact to resolve to a file.
        sub_path: String,
    },
    /// No producer found in the target's recorded actions for this
    /// generated path. Surface inside the mapper so the error
    /// carries the path; defence-in-depth — the engine should reject
    /// such graphs earlier.
    UnknownGenerated {
        path: String,
        target_display: String,
    },
}

/// Walk `action.inputs` once and decide whether the GlobalActionIndex
/// is needed for this action. The local-target scan covers same-target
/// chains; only call into DICE for the global index when at least one
/// input is generated *and* missing from the local list. Avoids paying
/// the (DICE-memoised, but still scheduler-visible) cost for actions
/// whose inputs are all sources or local-target outputs — most actions
/// in practice.
async fn compute_global_index_if_needed(
    ctx: &mut DiceComputations<'_>,
    action: &Action,
    analyzed: &AnalyzedTarget,
    config: &Configuration,
) -> Result<Option<Arc<BTreeMap<String, ProducerEntry>>>, String> {
    let needs_global = action.inputs.iter().any(|i| {
        if i.is_source() {
            return false;
        }
        let p = i.path();
        !analyzed
            .actions
            .iter()
            .any(|a| a.outputs.iter().any(|o| o.path() == p))
    });
    if !needs_global {
        return Ok(None);
    }
    let res = ctx
        .compute(&GlobalActionIndexKey(config.clone()))
        .await
        .map_err(|e| format!("DICE compute error: {e}"))?;
    match res {
        SharedResult::Ok(idx) => Ok(Some(idx)),
        SharedResult::Err(e) => Err((*e).clone()),
    }
}

/// Classify one generated input into an `InputRequest`. The local-
/// target action list is preferred (cheapest); only fall back to the
/// global index when the path isn't produced locally.
fn classify_generated(
    path: &str,
    analyzed: &AnalyzedTarget,
    self_ref: &ActionRef,
    global_index: Option<&BTreeMap<String, ProducerEntry>>,
) -> InputRequest {
    if let Some(i) = analyzed
        .actions
        .iter()
        .position(|a| a.outputs.iter().any(|o| o.path() == path))
    {
        let output_index = analyzed.actions[i]
            .outputs
            .iter()
            .position(|o| o.path() == path)
            .expect("producer outputs contains this path");
        return InputRequest::Output {
            path: path.to_string(),
            producer: Box::new(ActionRef {
                target: self_ref.target.clone(),
                config: self_ref.config.clone(),
                action_index: i,
            }),
            output_index,
        };
    }
    if let Some(idx) = global_index {
        match crate::global_index::lookup(idx, path) {
            Some(ProducerLookup::Direct(entry)) => {
                return InputRequest::Output {
                    path: path.to_string(),
                    producer: Box::new(entry.action_ref.clone()),
                    output_index: entry.output_index,
                };
            }
            Some(ProducerLookup::TreeSubPath { entry, sub_path }) => {
                return InputRequest::TreeSubPath {
                    path: path.to_string(),
                    producer: Box::new(entry.action_ref.clone()),
                    output_index: entry.output_index,
                    sub_path,
                };
            }
            None => {}
        }
    }
    InputRequest::UnknownGenerated {
        path: path.to_string(),
        target_display: format!("{}", self_ref.target),
    }
}

/// Resolve a sub-path inside a producer's tree-artifact output to
/// `(digest, is_executable)`.
///
/// Composition chain:
///
///   1. `ActionDigestKey(producer)` — recurse into the producer's own
///      REAPI Action digest.
///   2. `ActionResultKey(action_digest)` — fetch the producer's cached
///      ActionResult.
///   3. Pluck `output_directories[output_index].tree_digest` — the
///      digest of the serialised REAPI `Tree` proto.
///   4. `get_cas_blob(tree_digest)` — pull the `Tree` bytes from CAS.
///   5. Decode the `Tree` and walk to `sub_path` inside it.
///
/// Returns `ResolvedInput::is_executable` exactly as the `Tree`
/// proto's matching `FileNode` records it — the REAPI `FileNode.is_executable`
/// bit is the same one a downstream merkle-tree builder would emit.
async fn resolve_tree_subpath(
    ctx: &mut DiceComputations<'_>,
    producer: ActionRef,
    output_index: usize,
    sub_path: &str,
) -> Result<ResolvedInput, CasError> {
    let action_digest_value = ctx
        .compute(&ActionDigestKey(producer.clone()))
        .await
        .map_err(|e| CasError::AcKey(format!("DICE compute error: {e}")))?;
    let action_digest = match action_digest_value {
        SharedResult::Ok(d) => d,
        SharedResult::Err(e) => return Err(CasError::AcKey((*e).clone())),
    };
    let result_value = ctx
        .compute(&ActionResultKey((*action_digest).clone()))
        .await
        .map_err(|e| CasError::AcKey(format!("DICE compute error: {e}")))?;
    let result = match result_value {
        SharedResult::Ok(r) => r,
        SharedResult::Err(e) => return Err(CasError::AcKey((*e).clone())),
    };
    let od = result
        .output_directories
        .get(output_index)
        .or_else(|| {
            // Bazel orders `output_directories` to match the matching
            // entries in `output_paths`; the producer's local
            // `output_index` lines up under that contract. If the
            // server returned a different order, fall back to the
            // single-entry case (most common).
            if result.output_directories.len() == 1 {
                result.output_directories.first()
            } else {
                None
            }
        })
        .ok_or_else(|| CasError::OutputNotInResult {
            path: sub_path.to_string(),
            action_digest: (*action_digest).clone(),
        })?;
    let client = ctx.global_data().cache_client();
    let blob = client.get_cas_blob(&od.tree_digest).await?;
    let tree: Tree = proto::decode_tree(&blob)?;
    let file_node =
        proto::tree_lookup(&tree, sub_path).ok_or_else(|| CasError::OutputNotInResult {
            path: format!("{}/{}", od.path, sub_path),
            action_digest: (*action_digest).clone(),
        })?;
    Ok(ResolvedInput {
        digest: file_node.digest.clone(),
        is_executable: file_node.is_executable,
    })
}

