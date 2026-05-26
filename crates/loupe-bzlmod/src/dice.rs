//! DICE wiring: `MaterializedRepoKey` + the engine-builder data the
//! key reads from (`HasBzlmodCache`, `HasHttpClient`, `HasRepoSpecs`).
//!
//! Defined here, computed here. loupe-core reaches in by calling
//! `ctx.compute(&MaterializedRepoKey(canonical))`.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use dice::{
    CancellationContext, DiceComputations, DiceData, DiceDataBuilder, DiceTransactionUpdater,
    InjectedKey, Key, NoValueSerialize, ValueSerialize,
};
use dupe::Dupe;
use pagable::{Pagable, pagable_typetag};

use crate::materialize::materialize;
use crate::repo_spec::RepoSpec;

/// DICE-stored `Ok(Arc<T>) | Err(Arc<String>)`. Mirrors loupe-core's
/// `SharedResult` shape but lives here to keep the two crates'
/// dependency direction one-way (core → bzlmod, never the reverse).
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub enum BzlmodResult<T: Allocative + 'static> {
    Ok(Arc<T>),
    Err(Arc<String>),
}

impl<T: Allocative + Clone + 'static> Dupe for BzlmodResult<T> {}

impl<T: Allocative + 'static> BzlmodResult<T> {
    pub fn err(msg: impl Into<String>) -> Self {
        BzlmodResult::Err(Arc::new(msg.into()))
    }
    pub fn ok(value: T) -> Self {
        BzlmodResult::Ok(Arc::new(value))
    }
}

#[derive(Clone)]
pub struct BzlmodCache(pub PathBuf);

impl allocative::Allocative for BzlmodCache {
    fn visit<'a, 'b: 'a>(&self, _visitor: &'a mut allocative::Visitor<'b>) {}
}

impl std::fmt::Debug for BzlmodCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("BzlmodCache").field(&self.0).finish()
    }
}

pub trait HasBzlmodCache {
    fn bzlmod_cache(&self) -> Option<&Path>;
}

impl HasBzlmodCache for DiceData {
    fn bzlmod_cache(&self) -> Option<&Path> {
        self.get::<BzlmodCache>().ok().map(|c| c.0.as_path())
    }
}

pub trait SetBzlmodCache {
    fn set_bzlmod_cache(&mut self, dir: PathBuf);
}

impl SetBzlmodCache for DiceDataBuilder {
    fn set_bzlmod_cache(&mut self, dir: PathBuf) {
        self.set(BzlmodCache(dir));
    }
}

#[derive(Clone)]
pub struct BzlmodHttpClient(pub reqwest::Client);

impl allocative::Allocative for BzlmodHttpClient {
    fn visit<'a, 'b: 'a>(&self, _visitor: &'a mut allocative::Visitor<'b>) {}
}

impl std::fmt::Debug for BzlmodHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BzlmodHttpClient").finish()
    }
}

pub trait HasHttpClient {
    fn http_client(&self) -> Option<reqwest::Client>;
}

impl HasHttpClient for DiceData {
    fn http_client(&self) -> Option<reqwest::Client> {
        self.get::<BzlmodHttpClient>().ok().map(|c| c.0.clone())
    }
}

pub trait SetHttpClient {
    fn set_http_client(&mut self, client: reqwest::Client);
}

impl SetHttpClient for DiceDataBuilder {
    fn set_http_client(&mut self, client: reqwest::Client) {
        self.set(BzlmodHttpClient(client));
    }
}

/// Effective env (`host snapshot` + `--repo_env` overrides) seen by
/// every `repository_rule` impl. Resolved once at `EngineBuilder::build`
/// time so the same map gets used for marker check + rule eval.
#[derive(Clone, Default)]
pub struct BzlmodDeclaredEnv(pub Arc<BTreeMap<String, String>>);

impl allocative::Allocative for BzlmodDeclaredEnv {
    fn visit<'a, 'b: 'a>(&self, _visitor: &'a mut allocative::Visitor<'b>) {}
}

impl std::fmt::Debug for BzlmodDeclaredEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BzlmodDeclaredEnv")
            .field("count", &self.0.len())
            .finish()
    }
}

pub trait HasDeclaredEnv {
    fn declared_env(&self) -> Arc<BTreeMap<String, String>>;
}

/// The main module's workspace root path. Exposed to
/// `repository_rule` impls through
/// `repository_ctx.workspace_root` so rules like
/// `aspect_tools_telemetry`'s `tel_repository` (which probes
/// `tools/bazel` relative to the workspace) can resolve paths
/// against the user's source tree.
#[derive(Clone, Default)]
pub struct BzlmodWorkspaceRoot(pub Option<PathBuf>);

impl allocative::Allocative for BzlmodWorkspaceRoot {
    fn visit<'a, 'b: 'a>(&self, _visitor: &'a mut allocative::Visitor<'b>) {}
}

impl std::fmt::Debug for BzlmodWorkspaceRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("BzlmodWorkspaceRoot").field(&self.0).finish()
    }
}

pub trait HasBzlmodWorkspaceRoot {
    fn bzlmod_workspace_root(&self) -> Option<PathBuf>;
}

impl HasBzlmodWorkspaceRoot for DiceData {
    fn bzlmod_workspace_root(&self) -> Option<PathBuf> {
        self.get::<BzlmodWorkspaceRoot>()
            .ok()
            .and_then(|r| r.0.clone())
    }
}

pub trait SetBzlmodWorkspaceRoot {
    fn set_bzlmod_workspace_root(&mut self, root: PathBuf);
}

impl SetBzlmodWorkspaceRoot for DiceDataBuilder {
    fn set_bzlmod_workspace_root(&mut self, root: PathBuf) {
        self.set(BzlmodWorkspaceRoot(Some(root)));
    }
}

impl HasDeclaredEnv for DiceData {
    fn declared_env(&self) -> Arc<BTreeMap<String, String>> {
        self.get::<BzlmodDeclaredEnv>()
            .ok()
            .map(|e| Arc::clone(&e.0))
            .unwrap_or_default()
    }
}

pub trait SetDeclaredEnv {
    fn set_declared_env(&mut self, env: BTreeMap<String, String>);
}

impl SetDeclaredEnv for DiceDataBuilder {
    fn set_declared_env(&mut self, env: BTreeMap<String, String>) {
        self.set(BzlmodDeclaredEnv(Arc::new(env)));
    }
}

/// Mutable map of `canonical → RepoSpec` on DiceData. Initial entries
/// come from manually-supplied specs + `MODULE.bazel.lock` auto-load
/// at engine-build time. After `module_extension` evaluation, the
/// engine merges captured extension specs in via [`ExtendRepoSpecs`].
///
/// Singleton `InjectedKey` carrying the full `canonical → RepoSpec`
/// map. Updated exclusively via `DiceTransactionUpdater::changed_to`
/// at engine-builder time (initial specs + BCR resolution + each
/// round of extension eval). Downstream readers go through
/// [`RepoSpecKey`] for per-canonical invalidation granularity —
/// reading this key directly would make every consumer recompute on
/// any spec change.
///
/// Mirrors Buck2's `LegacyBuckConfigForCellKey` + property-projection
/// pattern (one injected raw blob + a derived per-entry projection key).
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("RepoSpecsKey")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct RepoSpecsKey;

impl InjectedKey for RepoSpecsKey {
    type Value = Arc<BTreeMap<String, RepoSpec>>;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Per-canonical projection of [`RepoSpecsKey`]. Compute reads the
/// injected map and returns the entry for `self.0`. DICE's
/// equality-driven change propagation gives fine-grained
/// invalidation: a recompute that yields the same `Option<…>` as
/// before doesn't dirty downstream `BzlLoadKey` / `MaterializedRepoKey`
/// readers — even though every `RepoSpecKey` recomputes whenever
/// `RepoSpecsKey` is updated.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("RepoSpecKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct RepoSpecKey(pub String);

#[async_trait]
impl Key for RepoSpecKey {
    type Value = Option<Arc<RepoSpec>>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        match ctx.compute(&RepoSpecsKey).await {
            Ok(map) => map.get(&self.0).cloned().map(Arc::new),
            Err(_) => None,
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Force the singleton [`RepoSpecsKey`]. Always succeeds when the key
/// was injected (the engine builder injects an empty map even when no
/// specs are configured).
pub async fn force_repo_specs_map(
    ctx: &mut DiceComputations<'_>,
) -> Result<Arc<BTreeMap<String, RepoSpec>>, String> {
    ctx.compute(&RepoSpecsKey)
        .await
        .map_err(|e| format!("RepoSpecsKey: {e}"))
}

/// Force [`RepoSpecKey`] — the per-canonical projection. Returns
/// `None` when no spec is registered for `canonical` (the caller is
/// then expected to fall through to any embedded `RepoRoot`s that
/// already carry this canonical, e.g. compiled-in `@bazel_tools`).
pub async fn force_repo_spec(
    ctx: &mut DiceComputations<'_>,
    canonical: &str,
) -> Result<Option<Arc<RepoSpec>>, String> {
    ctx.compute(&RepoSpecKey(canonical.to_string()))
        .await
        .map_err(|e| format!("RepoSpecKey({canonical}): {e}"))
}

/// Inject the initial empty map at builder time. Must be called before
/// any DICE compute that touches a `RepoSpecKey` / `RepoSpecsKey`,
/// otherwise the InjectedKey's panic-on-compute fires.
pub trait SetRepoSpecs {
    fn set_repo_specs(&mut self, specs: BTreeMap<String, RepoSpec>);
}

impl SetRepoSpecs for DiceDataBuilder {
    fn set_repo_specs(&mut self, _specs: BTreeMap<String, RepoSpec>) {
        // Builder-side no-op: the initial value is injected post-`build()`
        // via `inject_repo_specs(updater, ...)`. Kept as a method so the
        // engine builder's call site stays declarative.
    }
}

/// Inject `specs` as the current value of [`RepoSpecsKey`] on
/// `updater`. The caller must `commit().await` the updater to actually
/// publish the value; until then, downstream computes see the previous
/// value. Equivalent to Buck2's
/// `DiceTransactionUpdater::set_legacy_config_external_data` pattern.
pub fn inject_repo_specs(
    updater: &mut DiceTransactionUpdater,
    specs: BTreeMap<String, RepoSpec>,
) -> Result<(), String> {
    updater
        .changed_to([(RepoSpecsKey, Arc::new(specs))])
        .map_err(|e| format!("inject_repo_specs: {e}"))
}

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("MaterializedRepoKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct MaterializedRepoKey(pub String);

#[async_trait]
impl Key for MaterializedRepoKey {
    type Value = BzlmodResult<PathBuf>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let canonical = self.0.clone();
        let spec = match force_repo_spec(ctx, &canonical).await {
            Ok(Some(s)) => (*s).clone(),
            Ok(None) => {
                return BzlmodResult::err(format!(
                    "no RepoSpec registered for canonical `{canonical}` \
                     (did the engine builder load MODULE.bazel.lock?)"
                ));
            }
            Err(e) => return BzlmodResult::err(e),
        };
        let Some(cache) = ctx.global_data().bzlmod_cache().map(Path::to_path_buf) else {
            return BzlmodResult::err(
                "BzlmodCache not set on DiceData; \
                 call EngineBuilder::with_bzlmod_cache(...)",
            );
        };
        let Some(client) = ctx.global_data().http_client() else {
            return BzlmodResult::err(
                "BzlmodHttpClient not set on DiceData; \
                 call EngineBuilder::with_http_client(...)",
            );
        };

        let env = ctx.global_data().declared_env();
        let workspace_root = ctx.global_data().bzlmod_workspace_root();

        // Channel actor: the eval thread (inside `materialize` →
        // `spawn_blocking`) synchronously blocks on this resolver when
        // a `ctx.path(Label(...))` fires. We sit in a `select!` loop
        // alongside the materialise future, fielding each request by
        // recursively computing `MaterializedRepoKey(child_canonical)`
        // and sending back the resolved path. When the materialise
        // future completes (success or failure), we break the loop.
        let (req_tx, mut req_rx) =
            tokio::sync::mpsc::channel::<crate::label_resolver::LabelReq>(16);
        let (resp_tx, resp_rx) = tokio::sync::mpsc::channel::<crate::label_resolver::LabelResp>(16);
        let resolver: Arc<dyn crate::label_resolver::LabelResolver> =
            Arc::new(crate::label_resolver::ChannelLabelResolver {
                req_tx,
                resp_rx: Arc::new(tokio::sync::Mutex::new(resp_rx)),
            });

        let mat_fut = materialize(&client, &cache, &spec, env, workspace_root, resolver);
        tokio::pin!(mat_fut);

        let result = loop {
            tokio::select! {
                biased;
                Some(req) = req_rx.recv() => {
                    let child = ctx.compute(&MaterializedRepoKey(req.canonical.clone())).await;
                    let path_result: crate::label_resolver::LabelResp = match child {
                        Ok(BzlmodResult::Ok(dir)) => {
                            let mut p = (*dir).clone();
                            if !req.package.is_empty() {
                                p = p.join(&req.package);
                            }
                            if !req.name.is_empty() {
                                p = p.join(&req.name);
                            }
                            Ok(p)
                        }
                        Ok(BzlmodResult::Err(e)) => Err(format!(
                            "label resolve `@{}//{}:{}`: {}",
                            req.canonical, req.package, req.name, e
                        )),
                        Err(e) => Err(format!(
                            "label resolve `@{}//{}:{}`: {e}",
                            req.canonical, req.package, req.name
                        )),
                    };
                    let _ = resp_tx.send(path_result).await;
                }
                r = &mut mat_fut => break r,
            }
        };

        match result {
            Ok(p) => BzlmodResult::ok(p),
            Err(e) => BzlmodResult::err(format!("MaterializedRepoKey({canonical}): {e:#}")),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}
