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
    CancellationContext, DiceComputations, DiceData, DiceDataBuilder, Key, NoValueSerialize,
    ValueSerialize,
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
/// Wrapped in `Arc<RwLock<…>>` so the engine can grow it after
/// construction — DiceData itself is set once at build time, but the
/// inner map can be extended.
#[derive(Clone, Default)]
pub struct BzlmodRepoSpecs(pub Arc<std::sync::RwLock<BTreeMap<String, RepoSpec>>>);

impl allocative::Allocative for BzlmodRepoSpecs {
    fn visit<'a, 'b: 'a>(&self, _visitor: &'a mut allocative::Visitor<'b>) {}
}

impl std::fmt::Debug for BzlmodRepoSpecs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.0.read().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("BzlmodRepoSpecs")
            .field("count", &count)
            .finish()
    }
}

pub trait HasRepoSpecs {
    fn repo_spec(&self, canonical: &str) -> Option<RepoSpec>;
    fn known_canonicals(&self) -> Vec<String>;
}

impl HasRepoSpecs for DiceData {
    fn repo_spec(&self, canonical: &str) -> Option<RepoSpec> {
        self.get::<BzlmodRepoSpecs>()
            .ok()
            .and_then(|s| s.0.read().ok()?.get(canonical).cloned())
    }

    fn known_canonicals(&self) -> Vec<String> {
        self.get::<BzlmodRepoSpecs>()
            .ok()
            .and_then(|s| s.0.read().ok().map(|m| m.keys().cloned().collect()))
            .unwrap_or_default()
    }
}

pub trait SetRepoSpecs {
    fn set_repo_specs(&mut self, specs: BTreeMap<String, RepoSpec>);
}

impl SetRepoSpecs for DiceDataBuilder {
    fn set_repo_specs(&mut self, specs: BTreeMap<String, RepoSpec>) {
        self.set(BzlmodRepoSpecs(Arc::new(std::sync::RwLock::new(specs))));
    }
}

/// Add specs to the existing `BzlmodRepoSpecs`. Existing entries
/// (manually-supplied / lockfile-loaded) take precedence over
/// later-merged ones.
pub trait ExtendRepoSpecs {
    fn extend_repo_specs(&self, more: BTreeMap<String, RepoSpec>);
}

impl ExtendRepoSpecs for DiceData {
    fn extend_repo_specs(&self, more: BTreeMap<String, RepoSpec>) {
        if let Ok(specs) = self.get::<BzlmodRepoSpecs>()
            && let Ok(mut w) = specs.0.write()
        {
            for (k, v) in more {
                w.entry(k).or_insert(v);
            }
        }
    }
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
        let Some(spec) = ctx.global_data().repo_spec(&canonical) else {
            return BzlmodResult::err(format!(
                "no RepoSpec registered for canonical `{canonical}` \
                 (did the engine builder load MODULE.bazel.lock?)"
            ));
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

        // Channel actor: the eval thread (inside `materialize` →
        // `spawn_blocking`) synchronously blocks on this resolver when
        // a `ctx.path(Label(...))` fires. We sit in a `select!` loop
        // alongside the materialise future, fielding each request by
        // recursively computing `MaterializedRepoKey(child_canonical)`
        // and sending back the resolved path. When the materialise
        // future completes (success or failure), we break the loop.
        let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<crate::label_resolver::LabelReq>(16);
        let (resp_tx, resp_rx) =
            tokio::sync::mpsc::channel::<crate::label_resolver::LabelResp>(16);
        let resolver: Arc<dyn crate::label_resolver::LabelResolver> =
            Arc::new(crate::label_resolver::ChannelLabelResolver {
                req_tx,
                resp_rx: Arc::new(tokio::sync::Mutex::new(resp_rx)),
            });

        let mat_fut = materialize(&client, &cache, &spec, env, resolver);
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
