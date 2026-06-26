//! Public engine API.
//!
//! Phase 2: only `list_package` is implemented. `analyze` and
//! `analyze_transitive` arrive in Phase 5. The `Configuration` field
//! is plumbed through even though v1 has a single config — DESIGN.md
//! mandates plumbing every dimension that future phases will need.
//!
//! Internally, queries route through DICE: `list_package` blocks on a
//! `PackageKey` compute, which transitively forces `ModuleGraphKey`.
//! `BzlLoadKey` is degenerate in Phase 2 (the BUILD evaluator stubs
//! its own loaded modules) but is a real DICE key so Phase 3 can swap
//! its compute fn without touching Engine.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use dice::{Dice, DiceTransaction};
use tokio::runtime::Runtime;
use tracing::instrument;

use crate::analysis::{AnalyzedTarget, Configuration};
use crate::dice_keys::{
    AnalysisKey, BzlLoadKey, DiscoverPackagesKey, ModuleGraphKey, PackageKey,
    SetBzlmodLockfilePath, SetExternalRepoRoots, SetPlatformsExpr, SetWorkspaceRoot, SharedResult,
};
use crate::module_graph::ModuleGraph;
use crate::package::Package;
use crate::repo_root::RepoRoot;
use loupe_labels::{Label, PackageId};

/// Top-level analysis engine. See `DESIGN.md` for the public surface.
pub struct Engine {
    workspace_root: PathBuf,
    dice: Arc<Dice>,
    runtime: Runtime,
    #[allow(dead_code)]
    config: Configuration,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Construct an engine rooted at `workspace_root`. Eagerly forces a
    /// `ModuleGraphKey` compute so a malformed workspace fails fast
    /// rather than at first query time.
    #[instrument(skip(config))]
    pub fn new(workspace_root: PathBuf, config: Configuration) -> Result<Self> {
        EngineBuilder::new(workspace_root, config).build()
    }

    /// Builder for configuring optional fields (external repo root, …)
    /// without breaking the two-arg [`Engine::new`] callers.
    pub fn builder(workspace_root: PathBuf, config: Configuration) -> EngineBuilder {
        EngineBuilder::new(workspace_root, config)
    }

    pub fn workspace_root(&self) -> &std::path::Path {
        &self.workspace_root
    }

    /// Force `ModuleGraphKey` and return the parsed graph.
    #[instrument(skip(self))]
    pub fn module_graph(&self) -> Result<Arc<ModuleGraph>> {
        let dice = Arc::clone(&self.dice);
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let v = txn
                .compute(&ModuleGraphKey)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            v.into_anyhow()
        })
    }

    /// Force `PackageKey` and return the targets in stable
    /// (sorted-by-name) order — the Phase 2 deliverable.
    #[instrument(skip(self), fields(workspace = %self.workspace_root.display()))]
    pub fn list_package(&self, package: &PackageId) -> Result<Vec<Label>> {
        Ok(self.query_package(package)?.labels())
    }

    /// Force `PackageKey` and return the full evaluated `Package`
    /// (target shapes, not just labels). Phase 3 callers use this to
    /// inspect rule kinds and recorded attrs.
    #[instrument(skip(self), fields(workspace = %self.workspace_root.display()))]
    pub fn query_package(&self, package: &PackageId) -> Result<Arc<Package>> {
        let dice = Arc::clone(&self.dice);
        let key = PackageKey(package.clone());
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let v = txn
                .compute(&key)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            v.into_anyhow()
        })
    }

    /// Sorted list of every package in the workspace. Routed through
    /// `DiscoverPackagesKey` so a second walk in the same engine is a
    /// DICE cache hit — the `analyze_workspace` loop is idempotent on
    /// rerun, and so is any other caller that wants the package list.
    #[instrument(skip(self))]
    pub fn discover_packages(&self) -> Result<Vec<PackageId>> {
        let dice = Arc::clone(&self.dice);
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let v = txn
                .compute(&DiscoverPackagesKey)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            v.into_anyhow().map(|arc| (*arc).clone())
        })
    }

    /// Configured analysis of one target. Forces `AnalysisKey` —
    /// transitively forcing `PackageKey`, `BzlLoadKey`, and (Phase
    /// 5+) any label-dep `AnalysisKey`s — and returns the
    /// `AnalyzedTarget` carrying captured providers.
    #[instrument(skip(self))]
    pub fn analyze_target(&self, label: &Label) -> Result<Arc<AnalyzedTarget>> {
        let dice = Arc::clone(&self.dice);
        let key = AnalysisKey(label.clone(), self.config.clone());
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let v = txn
                .compute(&key)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            v.into_anyhow()
        })
    }

    /// Bazel-style `cquery <label>` default output: one line per
    /// resolved label in canonical form. Matches what
    /// `bazel cquery //:world` prints by default.
    #[instrument(skip(self))]
    pub fn cquery(&self, label: &Label) -> Result<String> {
        let analyzed = self.analyze_target(label)?;
        Ok(analyzed.label.to_string())
    }

    /// `cquery <label> --output=starlark --starlark:expr=<expr>` — analyse
    /// `label` and evaluate `expr` over it with `target` bound to the
    /// configured target. The real evaluator (not a pattern match): see
    /// [`crate::cquery::eval_expr_over_target`].
    pub fn cquery_expr(&self, label: &Label, expr: &str) -> Result<String> {
        let analyzed = self.analyze_target(label)?;
        crate::cquery::eval_expr_over_target(&analyzed, expr)
    }

    /// Walk every package in the workspace, list every target, and
    /// run `analyze_target` on each. Targets whose `kind` is in
    /// `skip_kinds` are recorded in `skipped` and not analyzed (e.g.
    /// `oci_image_index` uses Starlark config transitions, which v1
    /// doesn't model). Per-target and per-package errors are
    /// collected, not propagated — the whole-workspace walk completes
    /// regardless so callers can see the full failure surface.
    /// Walk every package in the workspace, analyze each non-skipped
    /// target, then evaluate `main(targets)` from the user script at
    /// `script_path` over the analyzed set. Analysis failures don't
    /// abort the walk — they land in `CqueryReport.failed` and the
    /// surviving targets are still passed to the script.
    ///
    /// See `crate::cquery` for the script contract.
    #[instrument(skip(self, skip_kinds))]
    pub fn cquery_script(
        &self,
        script_path: &std::path::Path,
        skip_kinds: &[&str],
    ) -> Result<crate::cquery::CqueryReport> {
        let script = crate::cquery::load_script(script_path)?;
        let skip: std::collections::BTreeSet<String> =
            skip_kinds.iter().map(|s| s.to_string()).collect();
        let dice = Arc::clone(&self.dice);
        let config = self.config.clone();
        let mut workspace = self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            analyze_workspace_in_txn(&mut txn, &skip, &config).await
        })?;
        workspace.analyzed.sort_by(|a, b| a.label.cmp(&b.label));
        let mut report = crate::cquery::CqueryReport {
            analyzed: workspace.analyzed.len(),
            skipped: workspace.skipped,
            failed: workspace.failed,
            package_errors: workspace.package_errors,
            ..crate::cquery::CqueryReport::default()
        };
        report.output = crate::cquery::run_script(&script, &workspace.analyzed)?;
        Ok(report)
    }

    #[instrument(skip(self, skip_kinds))]
    pub fn analyze_workspace(&self, skip_kinds: &[&str]) -> Result<WorkspaceAnalysis> {
        let skip: std::collections::BTreeSet<String> =
            skip_kinds.iter().map(|s| s.to_string()).collect();
        let dice = Arc::clone(&self.dice);
        let config = self.config.clone();
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            analyze_workspace_in_txn(&mut txn, &skip, &config).await
        })
    }

    /// Resolve `graph.deps` against `registry` (transitive MVS) and
    /// return the resulting canonical → RepoSpec map. Drives async
    /// HTTP through the engine's own tokio runtime — internal helper
    /// used by [`EngineBuilder::build`].
    pub fn resolve_via_registry(
        &self,
        registry: &loupe_bzlmod::registry::Registry,
        graph: &crate::module_graph::ModuleGraph,
    ) -> Result<std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>> {
        Ok(self
            .resolve_via_registry_full(registry, graph)?
            .modules
            .into_iter()
            .map(|(k, m)| (k, m.spec))
            .collect())
    }

    /// Full registry resolution: specs map + transitive module info
    /// (their own `bazel_dep`s and `use_extension` records). Callers
    /// that need tag aggregation across modules pull this and feed
    /// the transitive vec into [`evaluate_use_extensions`].
    pub fn resolve_via_registry_full(
        &self,
        registry: &loupe_bzlmod::registry::Registry,
        graph: &crate::module_graph::ModuleGraph,
    ) -> Result<loupe_bzlmod::resolution::ResolveOutput> {
        use anyhow::anyhow;
        use loupe_bzlmod::HasBzlmodCache;
        use loupe_bzlmod::HasHttpClient;
        use loupe_bzlmod::resolution::BazelDepRequest;
        use loupe_bzlmod::resolution::resolve_full;

        let roots: Vec<BazelDepRequest> = graph
            .deps
            .iter()
            .map(|d| BazelDepRequest {
                name: d.apparent.clone(),
                version: d.version.clone(),
            })
            .filter(|r| !r.version.is_empty())
            .collect();
        if roots.is_empty() {
            return Ok(loupe_bzlmod::resolution::ResolveOutput::default());
        }

        let dice = Arc::clone(&self.dice);
        let registry = registry.clone();
        let sep = graph.canonical_separator();
        self.runtime.block_on(async move {
            let txn = open_txn(&dice).await;
            let client = txn
                .global_data()
                .http_client()
                .ok_or_else(|| anyhow!("registry resolution requires an http client"))?;
            let cache = txn
                .global_data()
                .bzlmod_cache()
                .ok_or_else(|| anyhow!("registry resolution requires with_bzlmod_cache(...)"))?
                .to_path_buf();
            let best_effort: std::collections::BTreeSet<String> =
                crate::module_graph::loupe_default_dep_names()
                    .into_iter()
                    .collect();
            let output = resolve_full(&client, &cache, &registry, roots, sep, &best_effort)
                .await
                .map_err(|e| anyhow!("registry resolution: {e}"))?;
            anyhow::Ok(output)
        })
    }

    /// Refresh the `BzlmodGraphKey` injection from the current
    /// Snapshot the current `RepoSpecsKey` value. Cheap (`Arc::clone`).
    /// Used by `EngineBuilder::build` to detect real progress between
    /// extension-eval rounds — content comparison on the resulting
    /// `Arc<BTreeMap>` distinguishes "spec map actually grew" from
    /// "extensions re-ran and re-captured identical specs."
    pub fn repo_specs_snapshot(
        &self,
    ) -> Result<Arc<std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>>> {
        let dice = Arc::clone(&self.dice);
        self.runtime.block_on(async move {
            let mut txn = dice.updater().existing_state().await;
            txn.compute(&loupe_bzlmod::RepoSpecsKey)
                .await
                .map_err(|e| anyhow::anyhow!("RepoSpecsKey: {e}"))
        })
    }

    /// `RepoSpecsKey` value plus the parsed-main `graph` and the BCR
    /// `transitive` list. Call after `merge_repo_specs` advances the
    /// spec set so `ext_generated` reflects newly-registered canonicals.
    ///
    /// Threaded as an `Engine` method so `EngineBuilder::build` and any
    /// future bookkeeping (file watchers, etc.) share one code path.
    pub fn apply_bzlmod_snapshot(
        &self,
        graph: &crate::module_graph::ModuleGraph,
        transitive: &[loupe_bzlmod::resolution::TransitiveModuleInfo],
        ext_generated: &[loupe_bzlmod::bzlmod_graph::ExtGenRecord],
    ) -> Result<()> {
        let dice = Arc::clone(&self.dice);
        let main_graph = graph.clone();
        let transitive = transitive.to_vec();
        let ext_generated = ext_generated.to_vec();
        self.runtime.block_on(async move {
            let mut updater = dice.updater();
            let snapshot = build_bzlmod_graph(&main_graph, &transitive, &ext_generated);
            loupe_bzlmod::inject_bzlmod_graph(&mut updater, Some(snapshot))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let _ = updater.commit().await;
            anyhow::Ok(())
        })
    }

    /// Merge `extra` into the engine's `RepoSpecsKey` value. Existing
    /// entries (manually-supplied / lockfile-loaded / earlier extension
    /// rounds) win; `extra` only fills in canonicals that aren't
    /// already covered.
    ///
    /// Mutation flows through `DiceTransactionUpdater::changed_to` so
    /// every `RepoSpecKey(canonical)` projection invalidates structurally
    /// — `BzlLoadKey` / `MaterializedRepoKey` readers re-run only for
    /// the canonicals whose value actually changed (the projection's
    /// equality check short-circuits the rest).
    pub fn merge_repo_specs(
        &self,
        extra: std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>,
    ) -> Result<()> {
        if extra.is_empty() {
            return Ok(());
        }
        let dice = Arc::clone(&self.dice);
        self.runtime.block_on(async move {
            let mut updater = dice.updater();
            let current = {
                let mut txn = updater.existing_state().await;
                txn.compute(&loupe_bzlmod::RepoSpecsKey)
                    .await
                    .map_err(|e| anyhow::anyhow!("RepoSpecsKey: {e}"))?
            };
            let mut merged: std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec> =
                (*current).clone();
            for (k, v) in extra {
                merged.entry(k).or_insert(v);
            }
            loupe_bzlmod::inject_repo_specs(&mut updater, merged)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            updater.commit().await;
            anyhow::Ok(())
        })
    }

    /// Materialise every bzlmod-registered repo into the configured
    /// cache. Useful as a warm-up step in CI before running queries
    /// against a fresh checkout — mirrors `bazel mod vendor` in
    /// purpose, though it sources URLs from `MODULE.bazel.lock`'s
    /// `generatedRepoSpecs` (or specs supplied via `with_repo_specs`)
    /// rather than the BCR registry directly.
    ///
    /// Returns the canonicals materialised plus any per-canonical
    /// errors (other canonicals still succeed). When no bzlmod cache
    /// is configured, returns an empty report immediately.
    #[instrument(skip(self))]
    pub fn fetch_all(&self) -> Result<FetchReport> {
        use loupe_bzlmod::{BzlmodResult, MaterializedRepoKey, force_repo_specs_map};
        let dice = Arc::clone(&self.dice);
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let mut canonicals: std::collections::BTreeSet<String> = force_repo_specs_map(&mut txn)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .keys()
                .cloned()
                .collect();
            // Lazy extension demand: under the new model, ext-generated
            // canonicals don't live in `RepoSpecsKey` until something
            // pulls them through `ExtMaterializedRepoKey`. `fetch_all`
            // is the explicit "warm everything" entry — it derives the
            // owner index from `ModuleGraphKey` + `BzlmodGraphKey` and
            // ensures every use_repo'd canonical materialises.
            if let Ok(demand) = crate::dice_keys::force_extension_demand(&mut txn).await {
                for k in demand.owner_index.keys() {
                    canonicals.insert(k.clone());
                }
            }
            let canonicals: Vec<String> = canonicals.into_iter().collect();
            // Materialise every registered canonical in parallel. Each
            // `MaterializedRepoKey` / `ExtMaterializedRepoKey` does HTTP
            // fetch + archive extract + (optionally) Starlark eval of
            // its repo_rule impl, all of which are independent across
            // canonicals. DICE memoizes on recompute and lets
            // `compute_join` dispatch concurrent sub-contexts.
            let results: Vec<(String, Result<Arc<PathBuf>, String>)> = txn
                .compute_join(canonicals, |ctx, canonical: String| {
                    use futures::FutureExt;
                    async move {
                        // Static spec first; fall through to the
                        // extension-demand path on miss.
                        let r = match ctx.compute(&MaterializedRepoKey(canonical.clone())).await {
                            Ok(BzlmodResult::Ok(path)) => Ok(path),
                            Ok(BzlmodResult::Err(_)) | Err(_) => {
                                match ctx
                                    .compute(&crate::dice_keys::ExtMaterializedRepoKey(
                                        canonical.clone(),
                                    ))
                                    .await
                                {
                                    Ok(crate::dice_keys::SharedResult::Ok(p)) => Ok(p),
                                    Ok(crate::dice_keys::SharedResult::Err(e)) => {
                                        Err(Arc::unwrap_or_clone(e))
                                    }
                                    Err(e) => Err(format!("{e}")),
                                }
                            }
                        };
                        (canonical, r)
                    }
                    .boxed()
                })
                .await;
            let mut report = FetchReport::default();
            for (canonical, result) in results {
                match result {
                    Ok(path) => report.materialised.push((canonical, (*path).clone())),
                    Err(e) => report.errors.push((canonical, e)),
                }
            }
            anyhow::Ok(report)
        })
    }

    /// Load `defining_bzl`, look up `symbol` in the resulting frozen
    /// module, and assemble a [`loupe_bzlmod::RepoSpec`] of kind
    /// `StarlarkRule` for the named `canonical` repo with `attrs`.
    ///
    /// Until module-extension eval lands, this is how a Starlark-defined
    /// repository rule gets registered for materialisation: production
    /// callers will replace this with extension impls capturing
    /// `http_archive(...)` invocations into specs. The shape stays the
    /// same: an `Arc<FrozenModule>` (heap-anchor) + a `FrozenValue`
    /// (pointer to the rule) + an attrs map.
    /// Evaluate a `module_extension(...)` by loading `extension_bzl` and
    /// downcasting `symbol`. Returns the canonical → `RepoSpec` map
    /// for every repo the extension's impl declares via
    /// `http_archive(...)` (or any other Starlark repo_rule).
    ///
    /// Canonical names follow the bazel convention
    /// `<owner_canonical><sep><ext_name><sep><repo_name>`. For a
    /// main-module extension, pass `owner_canonical = ""` and `sep = '+'`.
    ///
    /// `modules` is the list of modules visible to the extension
    /// (typically the root module + each `bazel_dep` that imports the
    /// extension). For v1 callers that only have one-module workspaces,
    /// pass a single-element vec with `is_root = true`.
    pub fn build_module_extension_specs(
        &self,
        extension_bzl: &Label,
        symbol: &str,
        owner_canonical: &str,
        sep: char,
        ext_name: &str,
        mut modules: Vec<loupe_bzlmod::ModuleView>,
    ) -> Result<ExtensionSpecs> {
        use anyhow::anyhow;
        use starlark::values::ValueLike;
        let dice = Arc::clone(&self.dice);
        let label = extension_bzl.clone();
        let loaded = self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let v = txn
                .compute(&BzlLoadKey(label))
                .await
                .map_err(|e| anyhow!("{e}"))?;
            v.into_anyhow()
        })?;
        let module = loaded.module.clone();
        let owned = module
            .get(symbol)
            .map_err(|e| anyhow!("loading `{symbol}` from `{extension_bzl}`: {e}"))?;
        let frozen_value = unsafe { owned.unchecked_frozen_value() };
        if frozen_value
            .downcast_ref::<loupe_bzlmod::FrozenModuleExtension>()
            .is_none()
        {
            return Err(anyhow!(
                "`{symbol}` in `{extension_bzl}` is not a module_extension \
                 (got type `{}`)",
                frozen_value.to_value().get_type()
            ));
        }

        let graph = self.module_graph()?;

        // Tag-class attr defaults: a call like `node.toolchain()` with no
        // args still needs `tag.name`, `tag.node_version`, etc. to be
        // readable in the extension impl. Bazel fills these from the
        // `tag_class(attrs = {name: attr.string(default=...), ...})`
        // declaration; we do the same before handing modules over to
        // the eval bridge. The graph also feeds per-using-module
        // canonicalisation for `attr.label` tag attrs.
        apply_tag_class_defaults(frozen_value, &mut modules, Some(&graph));

        let declared_env = {
            use loupe_bzlmod::dice::HasDeclaredEnv;
            let dice = Arc::clone(&self.dice);
            self.runtime
                .block_on(async move { open_txn(&dice).await.global_data().declared_env() })
        };
        let workspace_root = self.workspace_root.clone();
        let mapping = effective_repo_mapping_arc(&graph, extension_bzl);
        let bzlmod_cache = {
            use loupe_bzlmod::dice::HasBzlmodCache;
            let dice = Arc::clone(&self.dice);
            self.runtime.block_on(async move {
                open_txn(&dice)
                    .await
                    .global_data()
                    .bzlmod_cache()
                    .map(|p| p.to_path_buf())
            })
        };
        // Bazel runs module extensions under an isolated working
        // directory in the output base so an impl can't reach back
        // into user source. Per-extension subdir keyed by
        // `(defining_repo, ext_name)` matches Bazel's
        // `ModuleExtensionId` shape. Falls back to a process-lifetime
        // tempdir when no bzlmod cache is configured.
        let (extension_wd, _tempdir_guard) =
            extension_working_dir(extension_bzl, symbol, bzlmod_cache.as_deref())?;

        // Lazy materialise bridge: an extension impl can call
        // `module_ctx.path(Label("@external_repo//..."))` mid-eval. The
        // path is only meaningful if `@external_repo`'s
        // `MaterializedRepoKey` has already run — otherwise the dir
        // doesn't exist on disk and the next `mctx.execute(path)` /
        // `mctx.read(path)` blows up with ENOENT. Same channel-actor
        // pattern as `MaterializedRepoKey::compute`: blocking-thread
        // eval blocks on the resolver, async parent fields the
        // request, recursively computes the child `MaterializedRepoKey`,
        // and sends the path back.
        let (req_tx, mut req_rx) =
            tokio::sync::mpsc::channel::<loupe_bzlmod::label_resolver::LabelReq>(16);
        let (resp_tx, resp_rx) =
            tokio::sync::mpsc::channel::<loupe_bzlmod::label_resolver::LabelResp>(16);
        let resolver: Arc<dyn loupe_bzlmod::label_resolver::LabelResolver> =
            Arc::new(loupe_bzlmod::label_resolver::ChannelLabelResolver {
                req_tx,
                resp_rx: Arc::new(tokio::sync::Mutex::new(resp_rx)),
            });

        let module_for_eval = module;
        let mapping_for_eval = Arc::clone(&mapping);
        let bzlmod_cache_for_eval = bzlmod_cache;
        let workspace_root_for_eval = workspace_root;
        let extension_wd_for_eval = extension_wd;
        let declared_env_for_eval = declared_env;
        let resolver_for_eval = Arc::clone(&resolver);
        // HTTP client for `module_ctx.download` / `.download_and_extract`.
        // Falls back to a fresh `Client::new()` when none was wired
        // through `EngineBuilder::with_http_client(...)` — matches the
        // BCR fetch path's behaviour.
        let client_for_eval = {
            use loupe_bzlmod::dice::HasHttpClient;
            let dice = Arc::clone(&self.dice);
            self.runtime
                .block_on(async move { open_txn(&dice).await.global_data().http_client() })
                .unwrap_or_default()
        };
        let handle_for_eval = self.runtime.handle().clone();

        let dice = Arc::clone(&self.dice);
        let output = self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let eval_handle = tokio::task::spawn_blocking(move || {
                loupe_bzlmod::eval_module_extension(
                    frozen_value,
                    &module_for_eval,
                    modules,
                    declared_env_for_eval,
                    workspace_root_for_eval,
                    extension_wd_for_eval,
                    mapping_for_eval,
                    bzlmod_cache_for_eval,
                    resolver_for_eval,
                    client_for_eval,
                    handle_for_eval,
                )
            });
            tokio::pin!(eval_handle);

            loop {
                tokio::select! {
                    biased;
                    Some(req) = req_rx.recv() => {
                        use loupe_bzlmod::{BzlmodResult, MaterializedRepoKey};
                        let child = txn.compute(&MaterializedRepoKey(req.canonical.clone())).await;
                        let path_result: loupe_bzlmod::label_resolver::LabelResp = match child {
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
                                "label resolve `@@{}//{}:{}`: {}",
                                req.canonical, req.package, req.name, e
                            )),
                            Err(e) => Err(format!(
                                "label resolve `@@{}//{}:{}`: {e}",
                                req.canonical, req.package, req.name
                            )),
                        };
                        let _ = resp_tx.send(path_result).await;
                    }
                    r = &mut eval_handle => {
                        break r.map_err(|e| anyhow!("module_extension eval join: {e}"))?
                            .map_err(|e| anyhow!("module_extension eval: {e}"));
                    }
                }
            }
        })?;

        let mut specs = std::collections::BTreeMap::new();
        let mut ext_records: Vec<loupe_bzlmod::bzlmod_graph::ExtGenRecord> = Vec::new();
        for call in output.repos {
            let canonical = format!("{owner_canonical}{sep}{ext_name}{sep}{}", call.name);
            let merged_attrs =
                merge_repo_rule_defaults(call.rule_fv, &canonical, extension_bzl, call.attrs);
            ext_records.push(loupe_bzlmod::bzlmod_graph::ExtGenRecord {
                canonical: canonical.clone(),
                owner_canonical: owner_canonical.to_string(),
                ext_name: ext_name.to_string(),
                repo_short: call.name.clone(),
            });
            specs.insert(
                canonical.clone(),
                loupe_bzlmod::RepoSpec {
                    canonical,
                    kind: loupe_bzlmod::RepoSpecKind::StarlarkRule {
                        module: call.rule_module,
                        rule_fv: call.rule_fv,
                        attrs: merged_attrs,
                        rule_source_digest: Arc::clone(&loaded.source_digest),
                        repo_mapping: Arc::clone(&mapping),
                    },
                },
            );
        }
        Ok(ExtensionSpecs {
            specs,
            ext_generated: ext_records,
        })
    }

    pub fn build_starlark_repo_spec(
        &self,
        canonical: String,
        defining_bzl: &Label,
        symbol: &str,
        attrs: std::collections::BTreeMap<String, loupe_bzlmod::RepoAttrValue>,
    ) -> Result<loupe_bzlmod::RepoSpec> {
        use anyhow::anyhow;
        use starlark::values::ValueLike;
        let dice = Arc::clone(&self.dice);
        let label = defining_bzl.clone();
        let loaded = self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let v = txn
                .compute(&BzlLoadKey(label))
                .await
                .map_err(|e| anyhow!("{e}"))?;
            v.into_anyhow()
        })?;
        let module = loaded.module.clone();
        let source_digest = Arc::clone(&loaded.source_digest);
        let owned = module
            .get(symbol)
            .map_err(|e| anyhow!("loading `{symbol}` from `{defining_bzl}`: {e}"))?;
        let frozen_value = unsafe { owned.unchecked_frozen_value() };
        if frozen_value
            .downcast_ref::<loupe_bzlmod::FrozenRepositoryRule>()
            .is_none()
        {
            return Err(anyhow!(
                "`{symbol}` in `{defining_bzl}` is not a repository_rule \
                 (got type `{}`)",
                frozen_value.to_value().get_type()
            ));
        }
        let graph = self.module_graph()?;
        let mapping = effective_repo_mapping_arc(&graph, defining_bzl);
        let merged_attrs = merge_repo_rule_defaults(frozen_value, &canonical, defining_bzl, attrs);
        Ok(loupe_bzlmod::RepoSpec {
            canonical,
            kind: loupe_bzlmod::RepoSpecKind::StarlarkRule {
                module,
                rule_fv: frozen_value,
                attrs: merged_attrs,
                rule_source_digest: source_digest,
                repo_mapping: mapping,
            },
        })
    }
}

/// Result of [`Engine::fetch_all`]. Both buckets are populated in
/// canonical-name order; missing buckets are empty.
#[derive(Debug, Default)]
pub struct FetchReport {
    pub materialised: Vec<(String, PathBuf)>,
    pub errors: Vec<(String, String)>,
}

/// Output of [`Engine::build_module_extension_specs`]. The
/// `ext_generated` records describe the `(owner, ext, repo_short)`
/// breakdown of each canonical the extension declared — captured
/// here at construction so downstream callers never have to parse
/// canonical names to recover the structure.
#[derive(Debug, Default)]
pub struct ExtensionSpecs {
    pub specs: std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>,
    pub ext_generated: Vec<loupe_bzlmod::bzlmod_graph::ExtGenRecord>,
}

/// Result of [`Engine::analyze_workspace`]. All four buckets are
/// populated; ordering follows package then target traversal order.
#[derive(Debug, Default)]
pub struct WorkspaceAnalysis {
    pub analyzed: Vec<Arc<AnalyzedTarget>>,
    pub skipped: Vec<SkippedTarget>,
    pub failed: Vec<FailedTarget>,
    pub package_errors: Vec<PackageError>,
}

#[derive(Debug, Clone)]
pub struct SkippedTarget {
    pub label: Label,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct FailedTarget {
    pub label: Label,
    pub kind: String,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct PackageError {
    pub package: PackageId,
    pub error: String,
}

/// `Arc<RepoMapping>` for the rule's defining `.bzl`. Surface for
/// loupe-bzlmod's `LabelResolver` — the mapping's `owner` carries the
/// defining `.bzl`'s repo so `Label("@//pkg:x")` self-refs resolve
/// without any seeding work at the call site.
fn effective_repo_mapping_arc(
    graph: &ModuleGraph,
    defining_bzl: &Label,
) -> Arc<loupe_labels::RepoMapping> {
    let pkg = loupe_labels::PackageId::new(
        defining_bzl.package.repo.clone(),
        defining_bzl.package.path.clone(),
    );
    Arc::new(graph.effective_repo_mapping(&pkg))
}

/// Build an isolated working directory for one extension eval.
///
/// With a bzlmod cache configured: `<cache>/.extension_wd/<id>/`,
/// where `<id>` encodes the extension's identity
/// (`<defining_repo>+<pkg-with-slashes-collapsed>+<bzl_file>+<ext_name>`)
/// in a path-safe form. Persisted across runs so re-evaluations land
/// in the same dir.
///
/// Without a cache: a process-lifetime `tempfile::TempDir`. The
/// returned `Option<TempDir>` must be held by the caller for the
/// duration of the eval so the directory isn't reaped.
///
/// Either way the directory is pre-created on disk so the extension
/// impl can `cd` into it / write to it without an explicit mkdir.
pub(crate) fn extension_working_dir(
    defining_bzl: &Label,
    ext_name: &str,
    bzlmod_cache: Option<&std::path::Path>,
) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
    use anyhow::Context as _;
    let id = extension_wd_id(defining_bzl, ext_name);
    if let Some(cache) = bzlmod_cache {
        let dir = cache.join(".extension_wd").join(&id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating extension working directory `{}`", dir.display()))?;
        Ok((dir, None))
    } else {
        let td = tempfile::tempdir()
            .context("creating tempdir for module-extension working directory")?;
        let path = td.path().to_path_buf();
        Ok((path, Some(td)))
    }
}

/// Path-safe identifier for an extension's working directory. Encodes
/// the same `(defining_repo, package, .bzl_file, ext_name)` tuple
/// Bazel uses as `ModuleExtensionId`, with `/` replaced so the result
/// can be one directory component.
fn extension_wd_id(defining_bzl: &Label, ext_name: &str) -> String {
    let repo = defining_bzl.package.repo.as_str();
    let pkg = defining_bzl.package.path.as_str();
    let file = &defining_bzl.name;
    let raw = format!("{repo}+{pkg}+{file}+{ext_name}");
    raw.replace('/', "_")
}

async fn open_txn(dice: &Arc<Dice>) -> DiceTransaction {
    dice.updater().commit().await
}

/// Workspace driver shared by `analyze_workspace` and `cquery_script`.
/// Runs inside a single DICE transaction so PackageKey and AnalysisKey
/// fan-outs go through `compute_join` (parallel across packages /
/// across targets), and DICE dedup's overlapping deps across the wave
/// instead of paying per-call transaction overhead.
#[tracing::instrument(skip_all)]
async fn analyze_workspace_in_txn(
    txn: &mut DiceTransaction,
    skip: &std::collections::BTreeSet<String>,
    config: &Configuration,
) -> Result<WorkspaceAnalysis> {
    let pkgs_arc = txn
        .compute(&DiscoverPackagesKey)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into_anyhow()?;
    let packages: Vec<PackageId> = (*pkgs_arc).clone();
    tracing::info!(count = packages.len(), "discovered packages");

    // Wave 1: every PackageKey in parallel. DICE memoizes per package
    // and dedupes any concurrent reentry from a sibling AnalysisKey
    // touching the same package later.
    let pkg_results: Vec<(PackageId, Result<Arc<crate::package::Package>, String>)> = txn
        .compute_join(packages, |ctx, pkg_id| {
            use futures::FutureExt;
            async move {
                tracing::debug!(package = %pkg_id, "PackageKey scheduled");
                let r = match ctx.compute(&PackageKey(pkg_id.clone())).await {
                    Ok(SharedResult::Ok(p)) => Ok(p),
                    Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
                    Err(e) => Err(format!("{e}")),
                };
                (pkg_id, r)
            }
            .boxed()
        })
        .await;
    tracing::info!(count = pkg_results.len(), "PackageKey wave complete");

    // Collect targets to analyze (and bookkeeping for skipped /
    // package errors) in package-then-target-name order — preserved
    // because PackageKey results land in input order and
    // `Package.targets` is a BTreeMap.
    let mut out = WorkspaceAnalysis::default();
    let mut to_analyze: Vec<(Label, String, Configuration)> = Vec::new();
    // `tags=["manual"]` targets are excluded from Bazel's `//...` wildcard
    // expansion — they're often broken-by-design (an analysis_test's
    // failing subject builds only under its consumer's transition). We
    // still analyse them opportunistically (a manual target that analyses
    // cleanly is real coverage), but a manual target that *fails* is
    // recorded as skipped rather than failed, matching what `//...` would
    // have surfaced.
    let mut manual: std::collections::BTreeSet<Label> = std::collections::BTreeSet::new();
    for (pkg_id, result) in pkg_results {
        match result {
            Ok(pkg) => {
                for target in pkg.targets.values() {
                    if skip.contains(target.kind.as_str()) {
                        out.skipped.push(SkippedTarget {
                            label: target.label.clone(),
                            kind: target.kind.clone(),
                        });
                        continue;
                    }
                    if target_is_manual(target) {
                        manual.insert(target.label.clone());
                    }
                    to_analyze.push((target.label.clone(), target.kind.clone(), config.clone()));
                }
            }
            Err(error) => out.package_errors.push(PackageError {
                package: pkg_id,
                error,
            }),
        }
    }
    tracing::info!(
        targets = to_analyze.len(),
        skipped = out.skipped.len(),
        pkg_errors = out.package_errors.len(),
        "AnalysisKey wave starting",
    );

    // Wave 2: every AnalysisKey in parallel. The per-item Configuration
    // rides inside the tuple so the closure stays `Copy` (DICE's
    // `compute_join` mapper bound).
    let analyzed_results: Vec<(Label, String, Result<Arc<AnalyzedTarget>, String>)> = txn
        .compute_join(to_analyze, |ctx, (label, kind, cfg)| {
            use futures::FutureExt;
            async move {
                tracing::debug!(%label, "AnalysisKey scheduled");
                let r = match ctx.compute(&AnalysisKey(label.clone(), cfg)).await {
                    Ok(SharedResult::Ok(v)) => Ok(v),
                    Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
                    Err(e) => Err(format!("{e}")),
                };
                (label, kind, r)
            }
            .boxed()
        })
        .await;
    tracing::info!(count = analyzed_results.len(), "AnalysisKey wave complete");

    for (label, kind, result) in analyzed_results {
        match result {
            Ok(at) => out.analyzed.push(at),
            // A manual target that fails analysis standalone is recorded
            // as skipped, not failed — `//...` wouldn't have built it.
            Err(_) if manual.contains(&label) => out.skipped.push(SkippedTarget { label, kind }),
            Err(error) => out.failed.push(FailedTarget { label, kind, error }),
        }
    }
    Ok(out)
}

/// Whether a target carries `tags = ["manual"]` — Bazel excludes such
/// targets from `//...` wildcard expansion.
fn target_is_manual(target: &crate::package::UnconfiguredTarget) -> bool {
    use crate::package::AttrValue;
    matches!(target.attrs.get("tags"), Some(AttrValue::List(items))
        if items.iter().any(|v| matches!(v, AttrValue::String(s) if s == "manual")))
}

/// Fill every tag invocation's `attrs` map with defaults declared on
/// the matching `tag_class`. Tag calls like `node.toolchain()` with no
/// kwargs depend on this — the extension impl reads `tag.name`,
/// `tag.node_version`, … and Bazel populates those from
/// `attr.string(default=…)`-style declarations.
///
/// Walks `FrozenModuleExtension.tag_classes` (a Starlark dict of
/// `FrozenTagClass`) to build `class → attr_name → default AttrValue`,
/// then merges defaults into each `ModuleTagInvocation.attrs` without
/// overwriting values the caller actually supplied.
///
/// `graph` lets `attr.label`-typed tag attrs be canonicalised against
/// the using module's repo_mapping — `lock_file = "//:maven_install.json"`
/// in stardoc's MODULE.bazel must resolve to
/// `@@stardoc+//:maven_install.json`, not the root workspace's
/// `//:maven_install.json`. Pass `None` to skip canonicalisation
/// (acceptable for tests with no real module graph).
pub(crate) fn apply_tag_class_defaults(
    extension_fv: starlark::values::FrozenValue,
    modules: &mut [loupe_bzlmod::ModuleView],
    graph: Option<&crate::module_graph::ModuleGraph>,
) {
    use crate::starlark_types::attr::{AttrKind, FrozenAttrSpec};
    use loupe_bzlmod::{FrozenModuleExtension, FrozenTagClass, RepoAttrValue};
    use starlark::values::ValueLike;
    use starlark::values::dict::DictRef;

    let Some(ext) = extension_fv.downcast_ref::<FrozenModuleExtension>() else {
        return;
    };
    let Some(tag_classes_dict) = DictRef::from_value(ext.tag_classes.to_value()) else {
        return;
    };

    let mut defaults: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, RepoAttrValue>,
    > = std::collections::BTreeMap::new();
    // (class, attr) → AttrKind so we can coerce string-shaped tag
    // values to typed `RepoAttrValue` variants (e.g. `Label`) after
    // defaults are applied. Without this, `attr.label`-declared attrs
    // reach extension impls as bare strings and `.same_package_label`
    // / `.package` / `.name` calls raise "no such attribute".
    let mut kinds: std::collections::BTreeMap<(String, String), AttrKind> =
        std::collections::BTreeMap::new();
    for (class_key, tag_class_val) in tag_classes_dict.iter() {
        let Some(class_name) = class_key.unpack_str() else {
            continue;
        };
        let Some(tag_class) = tag_class_val.downcast_ref::<FrozenTagClass>() else {
            continue;
        };
        let Some(attrs_dict) = DictRef::from_value(tag_class.attrs.to_value()) else {
            continue;
        };
        let mut per_class: std::collections::BTreeMap<String, RepoAttrValue> =
            std::collections::BTreeMap::new();
        for (attr_key, attr_val) in attrs_dict.iter() {
            let Some(attr_name) = attr_key.unpack_str() else {
                continue;
            };
            let Some(spec) = attr_val.downcast_ref::<FrozenAttrSpec>() else {
                continue;
            };
            kinds.insert((class_name.to_string(), attr_name.to_string()), spec.kind);
            if let Some(repo) = attr_value_to_repo(&spec.default_attr_value()) {
                per_class.insert(attr_name.to_string(), repo);
            }
        }
        defaults.insert(class_name.to_string(), per_class);
    }

    for m in modules.iter_mut() {
        // Per-module repo_mapping used to canonicalise `attr.label`
        // strings. Built lazily — the first label-coercion in this
        // module triggers it, modules with no label-typed tags skip it.
        let mut mapping: Option<loupe_labels::RepoMapping> = None;
        let module_canonical = m.canonical.clone();
        for tag in m.tags.iter_mut() {
            if let Some(class_defaults) = defaults.get(&tag.class) {
                for (name, default) in class_defaults {
                    // Caller-supplied RepoAttrValue::None falls back to
                    // the declared default — same as missing. rules_buf's
                    // `buf.toolchains(sha256=None)` macro forwards
                    // optional kwargs that may be None; without this the
                    // extension impl's `toolchains.sha256` raised
                    // "no such attribute".
                    match tag.attrs.get(name) {
                        None => {
                            tag.attrs.insert(name.clone(), default.clone());
                        }
                        Some(loupe_bzlmod::RepoAttrValue::None) => {
                            tag.attrs.insert(name.clone(), default.clone());
                        }
                        Some(_) => {}
                    }
                }
            }
            // Coerce string-shaped values to typed variants where the
            // attr declares a stronger type:
            //   `attr.label`      String      → Label
            //   `attr.label_list` StringList  → LabelList
            // `LabelKeyedStringDict` would slot in here too but isn't
            // reached yet.
            for (attr_name, value) in tag.attrs.iter_mut() {
                let Some(kind) = kinds.get(&(tag.class.clone(), attr_name.clone())) else {
                    continue;
                };
                // Closure: canonicalise one label string against the
                // using module's repo_mapping. Anchors `//`-self-refs
                // on `module_canonical` and turns `@apparent//...`
                // into `@@canonical//...`. Falls back to the raw
                // string if no graph (test path) or parse fails.
                let canonicalise =
                    |raw: String, mapping: &mut Option<loupe_labels::RepoMapping>| -> String {
                        let Some(g) = graph else { return raw };
                        let map = mapping.get_or_insert_with(|| {
                            g.effective_repo_mapping(&loupe_labels::PackageId::new(
                                loupe_labels::CanonicalRepo::new(module_canonical.clone()),
                                loupe_labels::PackagePath::root(),
                            ))
                        });
                        let pkg = loupe_labels::PackageId::new(
                            loupe_labels::CanonicalRepo::new(module_canonical.clone()),
                            loupe_labels::PackagePath::root(),
                        );
                        loupe_labels::Label::resolve(&raw, &pkg, map)
                            .map(|l| l.to_string())
                            .unwrap_or(raw)
                    };

                match kind {
                    AttrKind::Label => {
                        if let loupe_bzlmod::RepoAttrValue::String(s) = value {
                            let raw = std::mem::take(s);
                            *value =
                                loupe_bzlmod::RepoAttrValue::Label(canonicalise(raw, &mut mapping));
                        }
                    }
                    AttrKind::LabelList => {
                        if let loupe_bzlmod::RepoAttrValue::StringList(items) = value {
                            let drained = std::mem::take(items);
                            let canon: Vec<String> = drained
                                .into_iter()
                                .map(|s| canonicalise(s, &mut mapping))
                                .collect();
                            *value = loupe_bzlmod::RepoAttrValue::LabelList(canon);
                        }
                    }
                    AttrKind::LabelKeyedStringDict => {
                        if let loupe_bzlmod::RepoAttrValue::StringDict(m) = value {
                            let drained = std::mem::take(m);
                            let canon: std::collections::BTreeMap<String, String> = drained
                                .into_iter()
                                .map(|(k, v)| (canonicalise(k, &mut mapping), v))
                                .collect();
                            *value = loupe_bzlmod::RepoAttrValue::LabelKeyedStringDict(canon);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Assemble the [`BzlmodGraph`] snapshot the final `Dice` exposes.
/// All inputs are in-memory; no FS reads. See the call site for the
/// rationale and how the snapshot becomes the authoritative source
/// for `ModuleGraphKey`.
fn build_bzlmod_graph(
    main_graph: &crate::module_graph::ModuleGraph,
    transitive: &[loupe_bzlmod::resolution::TransitiveModuleInfo],
    ext_generated: &[loupe_bzlmod::bzlmod_graph::ExtGenRecord],
) -> loupe_bzlmod::bzlmod_graph::BzlmodGraph {
    use loupe_bzlmod::bzlmod_graph::{BazelDepRecord, BzlmodGraph, ModuleRecord};

    let sep = main_graph.canonical_separator();

    // Main module record. Its `bazel_deps` carry full info (we
    // parsed it strictly); transitives lose `version` because the
    // BCR resolver hands `bazel_deps` back as `(apparent, canonical)`
    // tuples already canonicalised.
    let main = ModuleRecord {
        name: main_graph.main.name.clone(),
        version: main_graph.main.version.clone(),
        canonical: String::new(),
        is_root: true,
        bazel_deps: main_graph
            .deps
            .iter()
            .map(|d| BazelDepRecord {
                name: d.apparent.clone(),
                apparent: d.apparent.clone(),
                version: d.version.clone(),
                canonical: d.canonical.as_str().to_string(),
                dev_dependency: false,
            })
            .collect(),
        use_extensions: main_graph
            .use_extensions
            .iter()
            .map(convert_main_use_extension)
            .collect(),
        repo_rule_invocations: main_graph.repo_rule_invocations.clone(),
        registered_toolchains: main_graph
            .registered_toolchains
            .iter()
            .map(Label::to_string)
            .collect(),
        registered_execution_platforms: main_graph
            .registered_execution_platforms
            .iter()
            .map(Label::to_string)
            .collect(),
    };

    let transitive_records: Vec<ModuleRecord> = transitive
        .iter()
        .map(|m| ModuleRecord {
            name: m.name.clone(),
            version: m.version.clone(),
            canonical: m.canonical.clone(),
            is_root: false,
            bazel_deps: m
                .bazel_deps
                .iter()
                .map(|(apparent, canonical)| BazelDepRecord {
                    name: apparent.clone(),
                    apparent: apparent.clone(),
                    version: String::new(),
                    canonical: canonical.clone(),
                    dev_dependency: false,
                })
                .collect(),
            use_extensions: m.use_extensions.clone(),
            repo_rule_invocations: m.repo_rule_invocations.clone(),
            registered_toolchains: m.registered_toolchains.clone(),
            registered_execution_platforms: Vec::new(),
        })
        .collect();

    // Ext-generated records come straight from the resolver — each
    // module-extension evaluation records the (owner, ext, repo)
    // triple at construction time, so the graph never has to parse
    // canonical names back into their parts. Dedup by canonical so a
    // resolver that emits the same generated repo across rounds
    // doesn't blow the list up.
    let mut ext_generated_records: Vec<loupe_bzlmod::bzlmod_graph::ExtGenRecord> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for rec in ext_generated {
        if seen.insert(rec.canonical.clone()) {
            ext_generated_records.push(rec.clone());
        }
    }

    BzlmodGraph {
        main,
        transitive: transitive_records,
        ext_generated: ext_generated_records,
        sep,
    }
}

/// Lossy conversion from loupe-core's `UseExtensionRecord` (with
/// `TagAttrLiteral` attr shape) back to loupe-bzlmod's
/// `UseExtensionLite` (with `RepoAttrValue` attr shape). Only used
/// during `build_bzlmod_graph`; both shapes converge on the same
/// data so the round-trip is information-preserving for the surface
/// `BzlmodGraph` consumers read.
fn convert_main_use_extension(
    ue: &crate::module_graph::UseExtensionRecord,
) -> loupe_bzlmod::resolution::UseExtensionLite {
    use crate::module_graph::TagAttrLiteral;
    use loupe_bzlmod::RepoAttrValue;
    use loupe_bzlmod::resolution::{TagInvocationLite, UseExtensionLite};

    let tags = ue
        .tags
        .iter()
        .map(|t| TagInvocationLite {
            class: t.class.clone(),
            attrs: t
                .attrs
                .iter()
                .map(|(k, v)| {
                    let repo_val = match v {
                        TagAttrLiteral::String(s) => RepoAttrValue::String(s.clone()),
                        TagAttrLiteral::Int(i) => RepoAttrValue::Int(*i),
                        TagAttrLiteral::Bool(b) => RepoAttrValue::Bool(*b),
                        TagAttrLiteral::StringList(l) => RepoAttrValue::StringList(l.clone()),
                        TagAttrLiteral::StringDict(d) => RepoAttrValue::StringDict(d.clone()),
                    };
                    (k.clone(), repo_val)
                })
                .collect(),
        })
        .collect();
    UseExtensionLite {
        bzl_label: ue.bzl_label.clone(),
        name: ue.name.clone(),
        tags,
        dev_dependency: ue.dev_dependency,
        use_repo_names: ue.use_repo_names.clone(),
    }
}

/// Convert a loupe-core [`AttrValue`] into the loupe-bzlmod
/// [`RepoAttrValue`] surface — the subset usable as a tag/repo-rule
/// Extract declared attr defaults from a `repository_rule(attrs={...})`
/// declaration and merge them into a captured invocation's attrs.
/// Bazel populates `repository_ctx.attr.<name>` for every declared
/// attr using the schema's default when the invocation omitted the
/// attr (or passed `None` for it); without this Loupe-side merge,
/// `ctx.attr.build_file` and friends raise "no such attribute" on
/// any http_archive call that omits the optional attr.
///
/// A caller-supplied `RepoAttrValue::None` is treated the same as a
/// missing key — Bazel's behaviour for an explicit `rule(foo=None)`
/// is to fall back to the declared default.
pub(crate) fn merge_repo_rule_defaults(
    rule_fv: starlark::values::FrozenValue,
    canonical: &str,
    defining_bzl: &Label,
    mut attrs: std::collections::BTreeMap<String, loupe_bzlmod::RepoAttrValue>,
) -> std::collections::BTreeMap<String, loupe_bzlmod::RepoAttrValue> {
    use crate::starlark_types::attr::FrozenAttrSpec;
    use loupe_bzlmod::FrozenRepositoryRule;
    use starlark::values::ValueLike;
    use starlark::values::dict::DictRef;

    let _ = canonical;
    let Some(rule) = rule_fv.downcast_ref::<FrozenRepositoryRule>() else {
        return attrs;
    };
    let Some(attrs_dict) = DictRef::from_value(rule.attrs.to_value()) else {
        return attrs;
    };
    for (attr_key, attr_val) in attrs_dict.iter() {
        let Some(attr_name) = attr_key.unpack_str() else {
            continue;
        };
        // Treat `Some(RepoAttrValue::None)` (caller passed `None`
        // explicitly) the same as a missing key — Bazel's spec.
        let already_set = match attrs.get(attr_name) {
            None => false,
            Some(loupe_bzlmod::RepoAttrValue::None) => false,
            Some(_) => true,
        };
        if already_set {
            continue;
        }
        let Some(spec) = attr_val.downcast_ref::<FrozenAttrSpec>() else {
            continue;
        };
        let default = spec.default_attr_value();
        // Label-typed attrs whose user-set default is a raw string
        // (`attr.label(default = ":BUILD.bazel")`): resolve the
        // relative label against the rule's defining .bzl, then
        // surface as `RepoAttrValue::Label` so `ctx.attr.<name>`
        // becomes a Starlark `Label` value with `.workspace_name`,
        // `.package`, `.name` attrs — rules_python's `pythons_hub.bzl`
        // reads `_rules_python_workspace.workspace_name` and only
        // works when the attr is a real Label, not a bare string.
        // `unpack_path_arg` accepts both `LabelValue` and label-shaped
        // strings, so this doesn't regress `ctx.path(ctx.attr._t)`
        // call sites.
        if spec.kind.is_label_typed()
            && let crate::package::AttrValue::String(s) = &default
        {
            let resolved = resolve_relative_label_to_canonical_str(s, defining_bzl);
            attrs.insert(
                attr_name.to_string(),
                loupe_bzlmod::RepoAttrValue::Label(resolved),
            );
            continue;
        }
        if let Some(repo) = attr_value_to_repo(&default) {
            attrs.insert(attr_name.to_string(), repo);
        }
    }
    attrs
}

/// Anchor a label string at the defining `.bzl`'s package so the
/// resulting form flows through `unpack_path_arg` and resolves via
/// the rule's `repo_mapping`. Returns one of:
///
/// - `@@<canonical>//pkg:name` — already canonical, passes through.
/// - `@<apparent>//pkg:name` — apparent-keyed, runtime mapping
///   lookup handles `<apparent>`.
/// - `@//pkg:name` — empty-apparent self-reference (resolves via
///   the `"" → <defining_canonical>` mapping seed from
///   `effective_repo_mapping_strs`).
///
/// Inputs without a repo prefix (`:name`, `//pkg:name`, bare `name`)
/// adopt the defining .bzl's canonical via the empty-apparent form
/// so the runtime self-ref lookup picks them up without needing the
/// caller to know the canonical repo name.
fn resolve_relative_label_to_canonical_str(s: &str, defining_bzl: &Label) -> String {
    if s.starts_with("@@") || s.starts_with('@') {
        // Already repo-qualified — let the runtime resolver and
        // repo_mapping handle `@apparent` lookups.
        return s.to_string();
    }
    let owner_package = defining_bzl.package.path.as_str();
    if let Some(rest) = s.strip_prefix("//") {
        // `//pkg:name` — defining-module shape; surface via the
        // self-ref (empty apparent) so it lands in the rule's
        // own canonical at runtime.
        return format!("@//{rest}");
    }
    if let Some(name) = s.strip_prefix(':') {
        return format!("@//{owner_package}:{name}");
    }
    format!("@//{owner_package}:{s}")
}

/// Convert a loupe-core [`AttrValue`] into the loupe-bzlmod
/// [`RepoAttrValue`] surface — the subset usable as a tag/repo-rule
/// attribute. Returns `None` for kinds (`Label`, `Selector`, `Opaque`,
/// non-string homogeneous containers) that can't round-trip through
/// the simplified surface; callers fall back to "no default supplied".
fn attr_value_to_repo(av: &crate::package::AttrValue) -> Option<loupe_bzlmod::RepoAttrValue> {
    use crate::package::AttrValue;
    use loupe_bzlmod::RepoAttrValue;
    match av {
        AttrValue::None => Some(RepoAttrValue::None),
        AttrValue::String(s) => Some(RepoAttrValue::String(s.clone())),
        AttrValue::Int(i) => Some(RepoAttrValue::Int(*i)),
        AttrValue::Bool(b) => Some(RepoAttrValue::Bool(*b)),
        AttrValue::List(items) => {
            let strs: Option<Vec<String>> = items
                .iter()
                .map(|v| match v {
                    AttrValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            strs.map(RepoAttrValue::StringList)
        }
        AttrValue::Dict(m) => {
            // Try string→string first; fall back to string→list[string].
            let strings: Option<std::collections::BTreeMap<String, String>> = m
                .iter()
                .map(|(k, v)| match v {
                    AttrValue::String(s) => Some((k.clone(), s.clone())),
                    _ => None,
                })
                .collect();
            if let Some(d) = strings {
                return Some(RepoAttrValue::StringDict(d));
            }
            let lists: Option<std::collections::BTreeMap<String, Vec<String>>> = m
                .iter()
                .map(|(k, v)| match v {
                    AttrValue::List(items) => {
                        let strs: Option<Vec<String>> = items
                            .iter()
                            .map(|x| match x {
                                AttrValue::String(s) => Some(s.clone()),
                                _ => None,
                            })
                            .collect();
                        strs.map(|ss| (k.clone(), ss))
                    }
                    _ => None,
                })
                .collect();
            lists.map(RepoAttrValue::StringListDict)
        }
        AttrValue::Label(l) => Some(loupe_bzlmod::RepoAttrValue::Label(l.to_string())),
        // `attr.label_list()` defaults flow as `LabelList(Vec<Label>)` —
        // surface them as `RepoAttrValue::LabelList` so the tag
        // struct exposes a list of `Label` values (each with
        // `.package` / `.name` attrs), matching what Bazel emits.
        // Empty list when no default entries — covers bare
        // `attr.label_list()` declarations (e.g. `preupdate` on
        // rules_js's npm extension).
        AttrValue::LabelList(labels) => Some(loupe_bzlmod::RepoAttrValue::LabelList(
            labels.iter().map(Label::to_string).collect(),
        )),
        // `attr.label_keyed_string_dict()` defaults flow as
        // `LabelKeyedStringDict(BTreeMap<Label, String>)` — surface as
        // `RepoAttrValue::LabelKeyedStringDict` so the tag struct
        // exposes a dict keyed by `Label` values (each with
        // `.workspace_name` / `.package` / `.name`). Empty map when
        // no default entries — covers bare declarations like
        // rules_python's `whl_modifications` pip-extension attr.
        AttrValue::LabelKeyedStringDict(map) => {
            Some(loupe_bzlmod::RepoAttrValue::LabelKeyedStringDict(
                map.iter()
                    .map(|(label, s)| (label.to_string(), s.clone()))
                    .collect(),
            ))
        }
        AttrValue::Selector(_) | AttrValue::Opaque(_) => None,
    }
}

/// Builder for [`Engine`] — preserves the simple `Engine::new` API
/// while letting callers opt into registry-driven bzlmod resolution
/// and an on-disk bzlmod cache.
pub struct EngineBuilder {
    workspace_root: PathBuf,
    config: Configuration,
    bzlmod_lockfile_path: Option<PathBuf>,
    platforms_expr: Option<String>,
    bzlmod_cache: Option<PathBuf>,
    http_client: Option<reqwest::Client>,
    repo_specs: std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>,
    /// When set, the engine resolves the workspace's `bazel_dep`
    /// declarations against this registry (typically
    /// `https://bcr.bazel.build`) at build time. Without a registry,
    /// `bazel_dep`s must come from the lockfile or manual specs.
    registry: Option<loupe_bzlmod::registry::Registry>,
    /// Per-variable overrides layered on top of the host environment
    /// for `repository_rule` impls. Empty by default — the rule sees
    /// the full host env. Mirrors Bazel's `--repo_env`.
    repo_env_overrides: std::collections::BTreeMap<String, loupe_bzlmod::env_policy::EnvOverride>,
}

impl EngineBuilder {
    pub fn new(workspace_root: PathBuf, config: Configuration) -> Self {
        EngineBuilder {
            workspace_root,
            config,
            bzlmod_lockfile_path: None,
            platforms_expr: None,
            bzlmod_cache: None,
            http_client: None,
            repo_specs: std::collections::BTreeMap::new(),
            registry: None,
            repo_env_overrides: std::collections::BTreeMap::new(),
        }
    }

    /// Path to a `MODULE.bazel.lock` that carries the per-repo
    /// `recordedInputs` records. Bazel 7 writes the rich lockfile
    /// (`lockFileVersion: 26`) to `<output_base>/MODULE.bazel.lock`,
    /// not the workspace copy (which is `lockFileVersion: 11` and
    /// missing the records). When the workspace copy is rich enough
    /// (Bazel 8+), this override isn't needed — the default reads
    /// `<workspace>/MODULE.bazel.lock`.
    pub fn bzlmod_lockfile_path(mut self, path: PathBuf) -> Self {
        self.bzlmod_lockfile_path = Some(path);
        self
    }

    /// In-memory BUILD content the engine serves for the synthetic
    /// `@@_loupe_platforms//:` package. Pair with
    /// `Configuration::with_target_platform(@@_loupe_platforms//:current)`.
    pub fn platforms_expr(mut self, content: String) -> Self {
        self.platforms_expr = Some(content);
        self
    }

    /// Directory the bzlmod lazy-fetch path materialises repos into
    /// (`<cache>/<canonical>/`). Required for the `loupe-bzlmod`
    /// `MaterializedRepoKey` path to function; when unset, external
    /// repos cannot be resolved.
    pub fn with_bzlmod_cache(mut self, dir: PathBuf) -> Self {
        self.bzlmod_cache = Some(dir);
        self
    }

    /// HTTP client used for bzlmod fetches. Defaults to a fresh
    /// `reqwest::Client::new()` if a cache is configured but no
    /// client is provided.
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Pre-loaded `canonical → RepoSpec` map. Typical source is
    /// `loupe_bzlmod::lockfile_specs::load_from_workspace(...)`. The
    /// `loupe-bzlmod::MaterializedRepoKey` consults this when asked to
    /// materialise a canonical; a missing entry produces a typed
    /// `no RepoSpec registered` error rather than a silent fall-through.
    pub fn with_repo_specs(
        mut self,
        specs: std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>,
    ) -> Self {
        self.repo_specs = specs;
        self
    }

    /// Resolve `bazel_dep`s against `registry` at build time. Pair
    /// with [`Self::with_bzlmod_cache`] — the resolver caches
    /// `source.json` and `MODULE.bazel` fetches under the same root.
    pub fn with_registry(mut self, registry: loupe_bzlmod::registry::Registry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Shortcut for [`Self::with_registry`] pointed at
    /// `https://bcr.bazel.build` (the Bazel Central Registry).
    pub fn with_bcr(self) -> Self {
        self.with_registry(loupe_bzlmod::registry::Registry::bcr())
    }

    /// Per-variable overrides for the environment seen by every
    /// `repository_rule` impl. Mirrors Bazel's `--repo_env`:
    ///
    /// - `EnvOverride::Set(v)`  → `NAME=v` (overwrites host)
    /// - `EnvOverride::Inherit` → `NAME` (pin host value; useful when
    ///   paired with a stricter policy)
    /// - `EnvOverride::Unset`   → `NAME=` (remove from rule's view)
    ///
    /// Defaults to empty. Multiple calls accumulate; later entries
    /// overwrite earlier ones.
    pub fn with_repo_env(
        mut self,
        overrides: std::collections::BTreeMap<String, loupe_bzlmod::env_policy::EnvOverride>,
    ) -> Self {
        for (k, v) in overrides {
            self.repo_env_overrides.insert(k, v);
        }
        self
    }

    /// Convenience: set one override.
    pub fn with_repo_env_one(
        mut self,
        name: impl Into<String>,
        override_: loupe_bzlmod::env_policy::EnvOverride,
    ) -> Self {
        self.repo_env_overrides.insert(name.into(), override_);
        self
    }

    pub fn build(self) -> Result<Engine> {
        use loupe_bzlmod::dice::SetDeclaredEnv;
        use loupe_bzlmod::{SetBzlmodCache, SetHttpClient};

        // Build a fresh `Dice` from the EngineBuilder's configured state
        // plus an explicit per-call `initial_specs` snapshot.
        // `EngineBuilder::build` calls this twice: once to drive BCR
        // resolution + extension evaluation (the "preflight" pass —
        // accumulates every generated spec), then once more to hand
        // analysis a clean `Dice` with everything pre-loaded plus
        // the assembled `BzlmodGraph` snapshot. The second pass is
        // what makes lazy `BzlLoadKey` lookups for extension-
        // generated repos succeed — if we kept the preflight `Dice`,
        // every load that ran before its spec was registered would
        // stay cached as a failure inside DICE.
        // Build a fresh `Dice` with engine config wired into `DiceData`
        // (paths, env overrides, embedded roots, HTTP client). The
        // mutable, per-build state — repo specs and the bzlmod graph
        // snapshot — is *not* set here; it gets injected post-build via
        // `inject_repo_specs` / `inject_bzlmod_graph` so subsequent
        // updates can flow through `DiceTransactionUpdater::changed_to`
        // with proper invalidation.
        let build_dice = |workspace_root: &PathBuf,
                          bzlmod_lockfile_path: &Option<PathBuf>,
                          platforms_expr: &Option<String>,
                          bzlmod_cache: &Option<PathBuf>,
                          http_client: Option<reqwest::Client>,
                          repo_env_overrides: &std::collections::BTreeMap<
            String,
            loupe_bzlmod::env_policy::EnvOverride,
        >|
         -> Arc<Dice> {
            let mut builder = Dice::builder();
            builder.set_workspace_root(workspace_root.clone());
            let host = loupe_bzlmod::env_policy::host_env_snapshot();
            let resolved = loupe_bzlmod::env_policy::resolve_env(&host, repo_env_overrides);
            builder.set_declared_env(resolved);
            let mut roots: Vec<RepoRoot> = crate::builtins::primary_embedded_roots();
            if let Some(cache) = bzlmod_cache {
                roots.push(RepoRoot::Filesystem(cache.clone()));
            }
            roots.extend(crate::builtins::fallback_embedded_roots());
            builder.set_external_repo_roots(roots);
            builder.set_bzlmod_lockfile_path(bzlmod_lockfile_path.clone());
            builder.set_platforms_expr(platforms_expr.clone());
            if let Some(cache) = bzlmod_cache {
                builder.set_bzlmod_cache(cache.clone());
                builder.set_http_client(http_client.unwrap_or_default());
            }
            {
                use loupe_bzlmod::dice::SetBzlmodWorkspaceRoot;
                builder.set_bzlmod_workspace_root(workspace_root.clone());
            }
            builder.build(dice::DetectCycles::Enabled)
        };

        // Multi-thread so DICE's `compute_join` / `compute2` /
        // `try_compute_join` actually fan out across cores. The engine
        // surface stays synchronous — public methods call
        // `runtime.block_on(...)` from outside any tokio task, which is
        // legal on multi-thread. Repo-rule eval bridges sync→async via
        // `spawn_blocking` + `Handle::block_on`, which is also legal on
        // multi-thread (spawn_blocking lives on the blocking pool, not
        // a worker thread).
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        // Snapshot every EngineBuilder field the closure consumes.
        let workspace_root = self.workspace_root;
        let bzlmod_lockfile_path = self.bzlmod_lockfile_path;
        let platforms_expr = self.platforms_expr;
        let bzlmod_cache = self.bzlmod_cache;
        let http_client = self.http_client;
        let repo_env_overrides = self.repo_env_overrides;
        let config = self.config;
        let registry = self.registry;

        // Seed the initial spec map from manually-supplied specs +
        // lockfile auto-load. Same precedence as before: manual wins.
        let mut initial_specs = self.repo_specs;
        if bzlmod_cache.is_some() {
            let lockfile_path = bzlmod_lockfile_path
                .clone()
                .unwrap_or_else(|| workspace_root.join("MODULE.bazel.lock"));
            match loupe_bzlmod::lockfile_specs::load_from_path(&lockfile_path) {
                Ok(loaded) => {
                    for (canonical, spec) in loaded {
                        initial_specs.entry(canonical).or_insert(spec);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = %lockfile_path.display(),
                        error = %e,
                        "loupe-bzlmod: failed to auto-load specs from lockfile; \
                         falling back to manually-supplied specs"
                    );
                }
            }
        }

        // Single Dice for the whole lifecycle. The injected keys
        // (`RepoSpecsKey`, `BzlmodGraphKey`) start at their initial
        // values and are advanced via `DiceTransactionUpdater` as BCR
        // resolution and extension eval produce new state. No rebuilds.
        let dice = build_dice(
            &workspace_root,
            &bzlmod_lockfile_path,
            &platforms_expr,
            &bzlmod_cache,
            http_client,
            &repo_env_overrides,
        );
        // Seed the injected keys with their initial values. `Dice`'s
        // `InjectedKey` machinery panics if a key is computed before
        // being injected, so this MUST happen before any compute fires.
        {
            let dice_ref = Arc::clone(&dice);
            let initial_specs = initial_specs.clone();
            runtime.block_on(async move {
                let mut updater = dice_ref.updater();
                loupe_bzlmod::inject_repo_specs(&mut updater, initial_specs)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                loupe_bzlmod::inject_bzlmod_graph(&mut updater, None)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let _ = updater.commit().await;
                anyhow::Ok(())
            })?;
        }
        let engine = Engine {
            workspace_root,
            dice,
            runtime,
            config,
        };
        // Parse the main MODULE.bazel directly (no transitive walk) so
        // BCR resolution and extension eval see `graph.deps` /
        // `graph.use_extensions`. We don't force `ModuleGraphKey` here
        // — DICE would cache an empty-`registered_toolchains` graph
        // since the bzlmod cache hasn't been materialised yet.
        // `ModuleGraphKey` is left for the first analysis-side caller,
        // by which time extension eval has populated `.loupe/bzlmod`
        // and the transitive walk picks up every registered toolchain.
        let graph: Arc<ModuleGraph> = Arc::new(crate::module_graph::parse_with_lockfile_override(
            &engine.workspace_root,
            bzlmod_lockfile_path,
        )?);

        // Registry resolution: if `with_registry(...)` was called,
        // walk the workspace's `bazel_dep`s through the BCR-format
        // registry, run MVS, and merge the resulting Http specs into
        // BzlmodRepoSpecs. Must run BEFORE use_extension eval — an
        // extension's defining `.bzl` may live in a registry-resolved
        // module (e.g. `@rules_oci//oci:extensions.bzl`).
        let mut transitive: Vec<loupe_bzlmod::resolution::TransitiveModuleInfo> = Vec::new();
        if let Some(registry) = registry {
            let output = engine.resolve_via_registry_full(&registry, &graph)?;
            transitive = output.transitive;
            let specs: std::collections::BTreeMap<_, _> = output
                .modules
                .into_iter()
                .map(|(k, m)| (k, m.spec))
                .collect();
            engine.merge_repo_specs(specs)?;

            // Feed each transitive module's `register_toolchains(...)`
            // strings into the engine's side-channel so the next
            // `ModuleGraphKey` compute can canonicalise and surface
            // them as `graph.registered_toolchains`. Without this,
            // toolchain resolution would only see what the main module
            // explicitly registers; transitive registrations like
            // `aspect_bazel_lib`'s `@coreutils_toolchains//:all` would
            // get dropped on the floor until the module's archive is
            // materialised on disk.
            // Extension `.bzl`s reach registry-resolved repos through
            // the regular lazy-fetch path (BzlLoadKey →
            // MaterializedRepoKey), so no eager fetch is needed here.
            // The transitive `bazel_dep` / `register_toolchains` data
            // we used to plumb through DiceData side-channels now
            // lives in the `BzlmodGraph` snapshot we assemble at the
            // end of this function — single source of truth, no
            // mid-build mutation.
        }

        // Auto-evaluate captured `use_extension(...)` declarations from
        // MODULE.bazel (main + every transitive module's). The earlier
        // gate was `graph.use_extensions.is_empty()`, which dropped
        // transitive-only extensions like `aspect_bazel_lib`'s
        // `bazel_lib_toolchains` — a real fixture-blocker because
        // toolchain resolution can't reach the generated repos
        // otherwise. Always run; the function no-ops on an empty
        // group set.
        //
        // **No Dice rebuild between rounds.** Both the spec map
        // (`RepoSpecsKey`) and the resolved bzlmod graph
        // (`BzlmodGraphKey`) are `InjectedKey`s — applying new state
        // means `updater.changed_to(...) → commit()`, which advances
        // DICE's version and invalidates exactly the downstream keys
        // whose projection values changed. Stale `BzlLoadKey` Err
        // entries for canonicals that subsequently got specs invalidate
        // automatically; entries for canonicals whose specs didn't
        // change stay cached. This is the principled fix for the
        // multi-Dice rebuild that used to live here.
        // Lazy module-extension evaluation. **No extension impl runs
        // here.** The `BzlmodGraph` snapshot below carries every
        // `use_extension(...)` + `use_repo(ext, names)` record, and
        // `crate::dice_keys::force_extension_demand` derives the
        // canonical → ExtensionId + ExtensionId → ExtensionInputs
        // projections on-demand inside `maybe_materialise`,
        // `ModuleExtensionKey::compute`, and the toolchain-demand
        // walk. An extension whose outputs are never reached stays
        // cold for the engine's lifetime — critically, broken-but-
        // unreferenced extensions can no longer fail the engine
        // build.
        let ext_generated: Vec<loupe_bzlmod::bzlmod_graph::ExtGenRecord> = Vec::new();
        engine.apply_bzlmod_snapshot(&graph, &transitive, &ext_generated)?;

        // Without a bzlmod cache there's no place to materialise
        // extension-generated repos into. The engine still returns
        // successfully so analyses that don't reach lazy-fetched repos
        // work; the first demand that needs materialisation raises a
        // clear error.
        if bzlmod_cache.is_none() {
            return Ok(engine);
        }

        Ok(engine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_labels::{CanonicalRepo, PackagePath};
    use std::fs;

    fn write_workspace(dir: &std::path::Path) {
        fs::write(
            dir.join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(
            dir.join("defs.bzl"),
            r#"
def _hello_impl(ctx):
    return []

hello = rule(
    implementation = _hello_impl,
    attrs = {"greeting": attr.string(default = "hello")},
)
"#,
        )
        .unwrap();
        fs::write(
            dir.join("BUILD.bazel"),
            r#"
load(":defs.bzl", "hello")
hello(name = "world", greeting = "hi")
hello(name = "alpha")
"#,
        )
        .unwrap();
    }

    #[test]
    fn list_package_returns_targets_sorted() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let labels = engine
            .list_package(&PackageId::new(CanonicalRepo::MAIN, PackagePath::root()))
            .unwrap();
        assert_eq!(
            labels.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
            vec!["@@//:alpha", "@@//:world"],
        );
    }

    #[test]
    fn engine_construction_fails_on_bad_module_bazel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MODULE.bazel"), "").unwrap();
        let err = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap_err();
        assert!(err.to_string().contains("did not call module"));
    }

    #[test]
    fn discover_packages_walks_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/BUILD"), "").unwrap();
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let pkgs = engine.discover_packages().unwrap();
        assert_eq!(
            pkgs.iter()
                .map(|p| p.path.as_str().to_string())
                .collect::<Vec<_>>(),
            vec!["", "sub"],
        );
    }

    fn write_workspace_with_skipped_and_failing(dir: &std::path::Path) {
        // Two rule kinds: `hello` (analyzes cleanly) and `image_index`
        // (the rule we'll ask the bulk driver to skip). A third target
        // uses `hello` but with a non-existent label dep to force a
        // per-target failure.
        fs::write(
            dir.join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(
            dir.join("defs.bzl"),
            r#"
def _hello_impl(ctx):
    return []

def _broken_impl(ctx):
    fail("on purpose")

hello = rule(
    implementation = _hello_impl,
    attrs = {},
)

broken = rule(
    implementation = _broken_impl,
    attrs = {},
)

image_index = rule(
    implementation = _hello_impl,
    attrs = {},
)
"#,
        )
        .unwrap();
        fs::write(
            dir.join("BUILD.bazel"),
            r#"
load(":defs.bzl", "hello", "broken", "image_index")
hello(name = "ok")
broken(name = "broken")
image_index(name = "skipme")
"#,
        )
        .unwrap();
    }

    #[test]
    fn analyze_workspace_is_idempotent_on_rerun() {
        // Two back-to-back walks over the same workspace produce the
        // same buckets. Routes through `DiscoverPackagesKey`, so on
        // the second call DICE serves the package list from cache and
        // each per-target `AnalysisKey` is a hit too.
        let dir = tempfile::tempdir().unwrap();
        write_workspace_with_skipped_and_failing(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();

        let first = engine.analyze_workspace(&["image_index"]).unwrap();
        let second = engine.analyze_workspace(&["image_index"]).unwrap();

        let labels = |r: &WorkspaceAnalysis| {
            (
                r.analyzed
                    .iter()
                    .map(|a| a.label.to_string())
                    .collect::<Vec<_>>(),
                r.skipped
                    .iter()
                    .map(|s| s.label.to_string())
                    .collect::<Vec<_>>(),
                r.failed
                    .iter()
                    .map(|f| f.label.to_string())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(labels(&first), labels(&second));
        assert!(first.package_errors.is_empty() && second.package_errors.is_empty());
    }

    #[test]
    fn analyze_workspace_buckets_results() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace_with_skipped_and_failing(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let report = engine.analyze_workspace(&["image_index"]).unwrap();
        let analyzed: Vec<_> = report
            .analyzed
            .iter()
            .map(|a| a.label.to_string())
            .collect();
        assert_eq!(analyzed, vec!["@@//:ok"]);
        let skipped: Vec<_> = report
            .skipped
            .iter()
            .map(|s| (s.label.to_string(), s.kind.clone()))
            .collect();
        assert_eq!(
            skipped,
            vec![("@@//:skipme".to_string(), "image_index".to_string())]
        );
        let failed: Vec<_> = report.failed.iter().map(|f| f.label.to_string()).collect();
        assert_eq!(failed, vec!["@@//:broken"]);
        assert!(report.package_errors.is_empty());
    }

    #[test]
    fn attr_value_to_repo_handles_supported_kinds() {
        use crate::package::AttrValue;
        use loupe_bzlmod::RepoAttrValue;

        assert!(matches!(
            attr_value_to_repo(&AttrValue::None),
            Some(RepoAttrValue::None)
        ));
        assert_eq!(
            attr_value_to_repo(&AttrValue::String("v".to_string())),
            Some(RepoAttrValue::String("v".to_string()))
        );
        assert_eq!(
            attr_value_to_repo(&AttrValue::Int(42)),
            Some(RepoAttrValue::Int(42))
        );
        assert_eq!(
            attr_value_to_repo(&AttrValue::Bool(true)),
            Some(RepoAttrValue::Bool(true))
        );
        let list = AttrValue::List(vec![
            AttrValue::String("a".into()),
            AttrValue::String("b".into()),
        ]);
        assert_eq!(
            attr_value_to_repo(&list),
            Some(RepoAttrValue::StringList(vec!["a".into(), "b".into()]))
        );
        // Heterogeneous / label-typed kinds intentionally drop out so
        // the caller falls back to "no default" rather than smuggling
        // them through as something they aren't.
        let label_list = AttrValue::List(vec![AttrValue::Int(1)]);
        assert!(attr_value_to_repo(&label_list).is_none());
    }

    #[test]
    fn apply_tag_class_defaults_fills_missing_attrs() {
        use loupe_bzlmod::{ModuleTagInvocation, ModuleView, RepoAttrValue};

        // Workspace with a .bzl defining a module_extension whose
        // single tag_class declares two attrs with defaults. Mirrors
        // the rules_nodejs shape: `node.toolchain()` is invoked with
        // no kwargs but the impl reads `tag.name` / `tag.node_version`
        // off it.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("ext.bzl"),
            r#"
def _impl(module_ctx):
    pass

ext = module_extension(
    implementation = _impl,
    tag_classes = {
        "toolchain": tag_class(attrs = {
            "name": attr.string(default = "DEFAULT_NAME"),
            "version": attr.string(default = "1.0"),
        }),
    },
)
"#,
        )
        .unwrap();
        fs::write(dir.path().join("BUILD.bazel"), "").unwrap();

        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let ext_label = Label::resolve(
            "//:ext.bzl",
            &PackageId::new(CanonicalRepo::MAIN, PackagePath::root()),
            &Default::default(),
        )
        .unwrap();

        // Reach into the engine the same way `build_module_extension_specs`
        // does: force `BzlLoadKey`, pull the symbol off the frozen module,
        // and downcast.
        let dice = Arc::clone(&engine.dice);
        let label = ext_label.clone();
        let loaded = engine
            .runtime
            .block_on(async move {
                let mut txn = open_txn(&dice).await;
                let v = txn
                    .compute(&BzlLoadKey(label))
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                v.into_anyhow()
            })
            .unwrap();
        let owned = loaded.module.get("ext").unwrap();
        let ext_fv = unsafe { owned.unchecked_frozen_value() };

        // One tag invocation, class "toolchain", zero kwargs — the
        // shape rules_nodejs's `node.toolchain()` lands as.
        let mut modules = vec![ModuleView {
            name: "demo".into(),
            version: "0.0.0".into(),
            is_root: true,
            canonical: String::new(),
            tags: vec![ModuleTagInvocation {
                class: "toolchain".into(),
                attrs: std::collections::BTreeMap::new(),
            }],
        }];
        apply_tag_class_defaults(ext_fv, &mut modules, None);
        let attrs = &modules[0].tags[0].attrs;
        assert_eq!(
            attrs.get("name"),
            Some(&RepoAttrValue::String("DEFAULT_NAME".into())),
            "default for `name` must be filled in"
        );
        assert_eq!(
            attrs.get("version"),
            Some(&RepoAttrValue::String("1.0".into())),
            "default for `version` must be filled in"
        );
    }

    #[test]
    fn apply_tag_class_defaults_does_not_overwrite_supplied_attrs() {
        use loupe_bzlmod::{ModuleTagInvocation, ModuleView, RepoAttrValue};

        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("ext.bzl"),
            r#"
def _impl(module_ctx):
    pass

ext = module_extension(
    implementation = _impl,
    tag_classes = {
        "toolchain": tag_class(attrs = {
            "name": attr.string(default = "DEFAULT_NAME"),
        }),
    },
)
"#,
        )
        .unwrap();
        fs::write(dir.path().join("BUILD.bazel"), "").unwrap();

        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let ext_label = Label::resolve(
            "//:ext.bzl",
            &PackageId::new(CanonicalRepo::MAIN, PackagePath::root()),
            &Default::default(),
        )
        .unwrap();
        let dice = Arc::clone(&engine.dice);
        let label = ext_label.clone();
        let loaded = engine
            .runtime
            .block_on(async move {
                let mut txn = open_txn(&dice).await;
                let v = txn
                    .compute(&BzlLoadKey(label))
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                v.into_anyhow()
            })
            .unwrap();
        let owned = loaded.module.get("ext").unwrap();
        let ext_fv = unsafe { owned.unchecked_frozen_value() };

        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("name".into(), RepoAttrValue::String("explicit".into()));
        let mut modules = vec![ModuleView {
            name: "demo".into(),
            version: "0.0.0".into(),
            is_root: true,
            canonical: String::new(),
            tags: vec![ModuleTagInvocation {
                class: "toolchain".into(),
                attrs,
            }],
        }];
        apply_tag_class_defaults(ext_fv, &mut modules, None);
        assert_eq!(
            modules[0].tags[0].attrs.get("name"),
            Some(&RepoAttrValue::String("explicit".into())),
            "user-supplied attr value must win over tag_class default"
        );
    }

    #[test]
    fn apply_tag_class_defaults_replaces_caller_supplied_none() {
        use loupe_bzlmod::{ModuleTagInvocation, ModuleView, RepoAttrValue};

        // rules_buf shape: the `toolchains` tag_class declares
        // `sha256 = attr.string()`. Callers may invoke
        // `buf.toolchains(name=..., version=..., sha256=None)` —
        // explicit None for the optional checksum kwarg. The
        // extension impl then does `toolchains.sha256` and used to
        // hit "no such attribute" because tag.attrs carried sha256=None
        // and `or_insert_with` saw the key as present.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("ext.bzl"),
            r#"
def _impl(module_ctx):
    pass

ext = module_extension(
    implementation = _impl,
    tag_classes = {
        "toolchains": tag_class(attrs = {
            "name": attr.string(default = "buf_toolchains"),
            "version": attr.string(),
            "sha256": attr.string(),
        }),
    },
)
"#,
        )
        .unwrap();
        fs::write(dir.path().join("BUILD.bazel"), "").unwrap();

        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let ext_label = Label::resolve(
            "//:ext.bzl",
            &PackageId::new(CanonicalRepo::MAIN, PackagePath::root()),
            &Default::default(),
        )
        .unwrap();
        let dice = Arc::clone(&engine.dice);
        let label = ext_label.clone();
        let loaded = engine
            .runtime
            .block_on(async move {
                let mut txn = open_txn(&dice).await;
                let v = txn
                    .compute(&BzlLoadKey(label))
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                v.into_anyhow()
            })
            .unwrap();
        let owned = loaded.module.get("ext").unwrap();
        let ext_fv = unsafe { owned.unchecked_frozen_value() };

        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("version".into(), RepoAttrValue::String("v1.0".into()));
        // Caller passes None for sha256 explicitly — should be
        // replaced by tag_class default `""` rather than left as None.
        attrs.insert("sha256".into(), RepoAttrValue::None);
        let mut modules = vec![ModuleView {
            name: "demo".into(),
            version: "0.0.0".into(),
            is_root: true,
            canonical: String::new(),
            tags: vec![ModuleTagInvocation {
                class: "toolchains".into(),
                attrs,
            }],
        }];
        apply_tag_class_defaults(ext_fv, &mut modules, None);
        let after = &modules[0].tags[0].attrs;
        assert_eq!(
            after.get("sha256"),
            Some(&RepoAttrValue::String("".into())),
            "None falls back to tag_class default"
        );
        assert_eq!(
            after.get("version"),
            Some(&RepoAttrValue::String("v1.0".into())),
            "user-supplied non-None value wins"
        );
        assert_eq!(
            after.get("name"),
            Some(&RepoAttrValue::String("buf_toolchains".into())),
            "unset attr fills from declared default"
        );
    }

    #[test]
    fn module_graph_records_main_module_name() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let graph = engine.module_graph().unwrap();
        assert_eq!(graph.main.name, "demo");
    }

    #[test]
    fn merge_repo_rule_defaults_fills_unset_attrs() {
        use loupe_bzlmod::{RepoAttrValue, RepoSpecKind};

        // Mirrors http_archive's shape: declares `url` (required-ish
        // string, no default), `build_file` (label, no default), and
        // `_template` (label_list, default = []). The invocation only
        // passes `url`; Bazel still fills `build_file` (as None) and
        // `_template` (as []). Loupe must do the same so
        // `ctx.attr.build_file == None` and `ctx.attr._template`
        // iterates as empty.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("defs.bzl"),
            r#"
def _impl(ctx):
    if ctx.attr.url != "https://example/x.tgz":
        fail("url not set correctly")
    if ctx.attr.build_file != None:
        fail("build_file default expected to be None, got %s" % ctx.attr.build_file)
    if ctx.attr.flag != False:
        fail("flag default expected to be False, got %s" % ctx.attr.flag)
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(
    implementation = _impl,
    attrs = {
        "url": attr.string(),
        "build_file": attr.label(),
        "flag": attr.bool(),
    },
)
"#,
        )
        .unwrap();
        fs::write(dir.path().join("BUILD.bazel"), "").unwrap();

        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let defining_bzl = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "defs.bzl");
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert(
            "url".to_string(),
            RepoAttrValue::String("https://example/x.tgz".to_string()),
        );
        let spec = engine
            .build_starlark_repo_spec("ext+".to_string(), &defining_bzl, "my_repo", attrs)
            .unwrap();
        let merged = match spec.kind {
            RepoSpecKind::StarlarkRule { attrs, .. } => attrs,
            _ => panic!("expected StarlarkRule"),
        };
        assert_eq!(
            merged.get("url"),
            Some(&RepoAttrValue::String("https://example/x.tgz".into()))
        );
        // Non-label-typed boolean default is merged.
        assert_eq!(merged.get("flag"), Some(&RepoAttrValue::Bool(false)));
        // Empty label default (None) is merged so `ctx.attr.build_file`
        // evaluates as None rather than raising.
        assert_eq!(merged.get("build_file"), Some(&RepoAttrValue::None));
    }

    #[test]
    fn merge_repo_rule_defaults_resolves_label_typed_string_default() {
        use loupe_bzlmod::{RepoAttrValue, RepoSpecKind};

        // `attr.label(default = ":BUILD.bazel")` captures a relative
        // label string. Resolve it to `@@<defining_canonical>//<pkg>:name`
        // at merge time so `ctx.attr._template` surfaces a label string
        // that `unpack_path_arg`'s `@@`-branch parses cleanly.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("defs.bzl"),
            r#"
def _impl(ctx):
    ctx.file("MODULE.bazel", "module(name=\"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name=\"all\")\n")

my_repo = repository_rule(
    implementation = _impl,
    attrs = {
        "_template_relative": attr.label(default = ":BUILD.bazel"),
        "_template_absolute": attr.label(default = "//pkg:BUILD.bazel"),
        "_template_canonical": attr.label(default = "@@dep+//pkg:BUILD.bazel"),
    },
)
"#,
        )
        .unwrap();
        fs::write(dir.path().join("BUILD.bazel"), "").unwrap();

        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let defining_bzl = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "defs.bzl");
        let spec = engine
            .build_starlark_repo_spec(
                "ext+".to_string(),
                &defining_bzl,
                "my_repo",
                std::collections::BTreeMap::new(),
            )
            .unwrap();
        let merged = match spec.kind {
            RepoSpecKind::StarlarkRule { attrs, .. } => attrs,
            _ => panic!("expected StarlarkRule"),
        };
        // `:BUILD.bazel` lands as @-self-ref so the runtime mapping
        // seed (`"" → <defining_canonical>`) resolves it.
        assert_eq!(
            merged.get("_template_relative"),
            Some(&RepoAttrValue::Label("@//:BUILD.bazel".to_string()))
        );
        // `//pkg:BUILD.bazel` also surfaces as @-self-ref so the
        // resolution happens at runtime (no canonical hard-coded).
        assert_eq!(
            merged.get("_template_absolute"),
            Some(&RepoAttrValue::Label("@//pkg:BUILD.bazel".to_string()))
        );
        // Already-`@@`-prefixed strings pass through.
        assert_eq!(
            merged.get("_template_canonical"),
            Some(&RepoAttrValue::Label("@@dep+//pkg:BUILD.bazel".to_string()))
        );
    }

    #[test]
    fn effective_repo_mapping_arc_seeds_at_self_ref() {
        // `@//pkg:label` (apparent name empty) must resolve to the
        // owning module of the defining .bzl. The seeding is now
        // folded into `RepoMapping::owner`, so checking the owner
        // field is what we assert on instead of an explicit "" entry.
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let graph = engine.module_graph().unwrap();

        // .bzl owned by main → owner is main.
        let main_bzl = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "defs.bzl");
        let main_map = effective_repo_mapping_arc(&graph, &main_bzl);
        assert_eq!(main_map.owner.as_ref(), Some(&CanonicalRepo::MAIN));
        assert_eq!(main_map.resolve_apparent(""), Some(CanonicalRepo::MAIN));

        // .bzl owned by an external repo → owner is that repo.
        let ext_bzl = Label::new(
            CanonicalRepo::new("rules_multitool+"),
            PackagePath::root(),
            "multitool.bzl",
        );
        let ext_map = effective_repo_mapping_arc(&graph, &ext_bzl);
        assert_eq!(
            ext_map.owner.as_ref(),
            Some(&CanonicalRepo::new("rules_multitool+"))
        );
        assert_eq!(
            ext_map.resolve_apparent(""),
            Some(CanonicalRepo::new("rules_multitool+"))
        );
    }
}
