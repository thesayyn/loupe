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
            let output = resolve_full(&client, &cache, &registry, roots, sep)
                .await
                .map_err(|e| anyhow!("registry resolution: {e}"))?;
            anyhow::Ok(output)
        })
    }

    /// Merge `extra` into the engine's `BzlmodRepoSpecs`. Existing
    /// entries (manually-supplied / lockfile-loaded) win; `extra`
    /// only fills in canonicals that aren't already covered. Used by
    /// [`EngineBuilder::build`] to push module-extension-derived
    /// specs in after `ModuleGraphKey` has been forced.
    pub fn merge_repo_specs(
        &self,
        extra: std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>,
    ) {
        use loupe_bzlmod::ExtendRepoSpecs;
        if extra.is_empty() {
            return;
        }
        let dice = Arc::clone(&self.dice);
        self.runtime.block_on(async move {
            let txn = open_txn(&dice).await;
            txn.global_data().extend_repo_specs(extra);
        });
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
        use loupe_bzlmod::{BzlmodResult, HasRepoSpecs, MaterializedRepoKey};
        let dice = Arc::clone(&self.dice);
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let canonicals = txn.global_data().known_canonicals();
            // Materialise every registered canonical in parallel. Each
            // `MaterializedRepoKey` does HTTP fetch + archive extract +
            // (optionally) Starlark eval of its repo_rule impl, all of
            // which are independent across canonicals. DICE memoizes on
            // recompute and lets `compute_join` dispatch concurrent
            // sub-contexts.
            let results: Vec<(String, Result<Arc<PathBuf>, String>)> = txn
                .compute_join(canonicals, |ctx, canonical| {
                    use futures::FutureExt;
                    async move {
                        let r = match ctx.compute(&MaterializedRepoKey(canonical.clone())).await {
                            Ok(BzlmodResult::Ok(path)) => Ok(path),
                            Ok(BzlmodResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
                            Err(e) => Err(format!("{e}")),
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
    ) -> Result<std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>> {
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

        // Tag-class attr defaults: a call like `node.toolchain()` with no
        // args still needs `tag.name`, `tag.node_version`, etc. to be
        // readable in the extension impl. Bazel fills these from the
        // `tag_class(attrs = {name: attr.string(default=...), ...})`
        // declaration; we do the same before handing modules over to
        // the eval bridge.
        apply_tag_class_defaults(frozen_value, &mut modules);

        let declared_env = {
            use loupe_bzlmod::dice::HasDeclaredEnv;
            let dice = Arc::clone(&self.dice);
            self.runtime
                .block_on(async move { open_txn(&dice).await.global_data().declared_env() })
        };
        let output =
            loupe_bzlmod::eval_module_extension(frozen_value, &module, modules, declared_env)
                .map_err(|e| anyhow!("module_extension eval: {e}"))?;

        let graph = self.module_graph()?;
        let mapping = Arc::new(effective_repo_mapping_strs(&graph, extension_bzl));

        let mut out = std::collections::BTreeMap::new();
        for call in output.repos {
            let canonical = format!("{owner_canonical}{sep}{ext_name}{sep}{}", call.name);
            out.insert(
                canonical.clone(),
                loupe_bzlmod::RepoSpec {
                    canonical,
                    kind: loupe_bzlmod::RepoSpecKind::StarlarkRule {
                        module: call.rule_module,
                        rule_fv: call.rule_fv,
                        attrs: call.attrs,
                        rule_source_digest: Arc::clone(&loaded.source_digest),
                        repo_mapping: Arc::clone(&mapping),
                    },
                },
            );
        }
        Ok(out)
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
        let mapping = Arc::new(effective_repo_mapping_strs(&graph, defining_bzl));
        Ok(loupe_bzlmod::RepoSpec {
            canonical,
            kind: loupe_bzlmod::RepoSpecKind::StarlarkRule {
                module,
                rule_fv: frozen_value,
                attrs,
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

/// Convert the rule's defining `.bzl`'s effective repo_mapping into the
/// flat `apparent → canonical_str` shape loupe-bzlmod's `LabelResolver`
/// expects.
fn effective_repo_mapping_strs(
    graph: &ModuleGraph,
    defining_bzl: &Label,
) -> std::collections::BTreeMap<String, String> {
    let pkg = loupe_labels::PackageId::new(
        defining_bzl.package.repo.clone(),
        defining_bzl.package.path.clone(),
    );
    let mut out: std::collections::BTreeMap<String, String> = graph
        .effective_repo_mapping(&pkg)
        .into_iter()
        .map(|(k, v)| (k, v.as_str().to_string()))
        .collect();
    // Seed `"" → <defining_bzl_canonical>` for labels shaped `@//pkg:label`
    // (apparent name empty — Bazel's "label in the owning module" form).
    // Without this, rules calling `Label("@//foo:bar")` from inside an
    // external repo hit `no repo_mapping entry for apparent name @`.
    out.entry(String::new())
        .or_insert_with(|| defining_bzl.package.repo.as_str().to_string());
    out
}

async fn open_txn(dice: &Arc<Dice>) -> DiceTransaction {
    dice.updater().commit().await
}

/// Workspace driver shared by `analyze_workspace` and `cquery_script`.
/// Runs inside a single DICE transaction so PackageKey and AnalysisKey
/// fan-outs go through `compute_join` (parallel across packages /
/// across targets), and DICE dedup's overlapping deps across the wave
/// instead of paying per-call transaction overhead.
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

    // Wave 1: every PackageKey in parallel. DICE memoizes per package
    // and dedupes any concurrent reentry from a sibling AnalysisKey
    // touching the same package later.
    let pkg_results: Vec<(PackageId, Result<Arc<crate::package::Package>, String>)> = txn
        .compute_join(packages, |ctx, pkg_id| {
            use futures::FutureExt;
            async move {
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

    // Collect targets to analyze (and bookkeeping for skipped /
    // package errors) in package-then-target-name order — preserved
    // because PackageKey results land in input order and
    // `Package.targets` is a BTreeMap.
    let mut out = WorkspaceAnalysis::default();
    let mut to_analyze: Vec<(Label, String, Configuration)> = Vec::new();
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
                    to_analyze.push((target.label.clone(), target.kind.clone(), config.clone()));
                }
            }
            Err(error) => out.package_errors.push(PackageError {
                package: pkg_id,
                error,
            }),
        }
    }

    // Wave 2: every AnalysisKey in parallel. The per-item Configuration
    // rides inside the tuple so the closure stays `Copy` (DICE's
    // `compute_join` mapper bound).
    let analyzed_results: Vec<(Label, String, Result<Arc<AnalyzedTarget>, String>)> = txn
        .compute_join(to_analyze, |ctx, (label, kind, cfg)| {
            use futures::FutureExt;
            async move {
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

    for (label, kind, result) in analyzed_results {
        match result {
            Ok(at) => out.analyzed.push(at),
            Err(error) => out.failed.push(FailedTarget { label, kind, error }),
        }
    }
    Ok(out)
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
fn apply_tag_class_defaults(
    extension_fv: starlark::values::FrozenValue,
    modules: &mut [loupe_bzlmod::ModuleView],
) {
    use crate::starlark_types::attr::FrozenAttrSpec;
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
            if let Some(repo) = attr_value_to_repo(&spec.default_attr_value()) {
                per_class.insert(attr_name.to_string(), repo);
            }
        }
        defaults.insert(class_name.to_string(), per_class);
    }

    for m in modules.iter_mut() {
        for tag in m.tags.iter_mut() {
            let Some(class_defaults) = defaults.get(&tag.class) else {
                continue;
            };
            for (name, default) in class_defaults {
                tag.attrs
                    .entry(name.clone())
                    .or_insert_with(|| default.clone());
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
    final_specs: &std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>,
) -> loupe_bzlmod::bzlmod_graph::BzlmodGraph {
    use loupe_bzlmod::bzlmod_graph::{BazelDepRecord, BzlmodGraph, ExtGenRecord, ModuleRecord};

    let sep = main_graph.canonical_separator();
    let dbl = format!("{sep}{sep}");

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
            registered_toolchains: m.registered_toolchains.clone(),
            registered_execution_platforms: Vec::new(),
        })
        .collect();

    // Ext-generated records: every spec canonical whose name
    // matches the `<owner><sep><sep><ext><sep><repo>` (or the
    // well-known-owner single-sep) shape.
    let mut ext_generated: Vec<ExtGenRecord> = Vec::new();
    for canonical in final_specs.keys() {
        if let Some((owner_short, rest)) = canonical.split_once(&dbl) {
            let owner_canonical = format!("{owner_short}{sep}");
            let Some((ext_name, repo_short)) = rest.rsplit_once(sep) else {
                continue;
            };
            ext_generated.push(ExtGenRecord {
                canonical: canonical.clone(),
                owner_canonical,
                ext_name: ext_name.to_string(),
                repo_short: repo_short.to_string(),
            });
            continue;
        }
        for known in ["platforms", "bazel_tools", "local_config_platform"] {
            let prefix = format!("{known}{sep}");
            if let Some(rest) = canonical.strip_prefix(&prefix)
                && let Some((ext_name, repo_short)) = rest.split_once(sep)
            {
                ext_generated.push(ExtGenRecord {
                    canonical: canonical.clone(),
                    owner_canonical: known.to_string(),
                    ext_name: ext_name.to_string(),
                    repo_short: repo_short.to_string(),
                });
                break;
            }
        }
    }

    BzlmodGraph {
        main,
        transitive: transitive_records,
        ext_generated,
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

/// BFS from main through `graph.deps` + every transitive module's
/// `bazel_deps` to assign a depth to each module's canonical. Main is
/// depth 0, its direct deps are depth 1, deps-of-deps are depth 2,
/// etc. Used to drive a "leaves-first" ordering for extension
/// evaluation so a deeper module's outputs are registered before any
/// shallower module's extensions try to load them.
fn compute_module_depths(
    graph: &crate::module_graph::ModuleGraph,
    transitive: &[loupe_bzlmod::resolution::TransitiveModuleInfo],
) -> std::collections::BTreeMap<String, usize> {
    let mut depths: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut frontier: std::collections::VecDeque<(String, usize)> =
        std::collections::VecDeque::new();
    depths.insert(graph.main.canonical.as_str().to_string(), 0);
    for d in &graph.deps {
        let canonical = d.canonical.as_str().to_string();
        if depths.insert(canonical.clone(), 1).is_none() {
            frontier.push_back((canonical, 1));
        }
    }
    // Index transitive modules by canonical for cheap lookup.
    let by_canonical: std::collections::BTreeMap<
        &str,
        &loupe_bzlmod::resolution::TransitiveModuleInfo,
    > = transitive
        .iter()
        .map(|m| (m.canonical.as_str(), m))
        .collect();
    while let Some((canonical, depth)) = frontier.pop_front() {
        let Some(m) = by_canonical.get(canonical.as_str()) else {
            continue;
        };
        for (_, dep_canonical) in &m.bazel_deps {
            let next_depth = depth + 1;
            match depths.entry(dep_canonical.clone()) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(next_depth);
                    frontier.push_back((dep_canonical.clone(), next_depth));
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    if next_depth < *o.get() {
                        o.insert(next_depth);
                        frontier.push_back((dep_canonical.clone(), next_depth));
                    }
                }
            }
        }
    }
    // Any transitive module the BFS didn't reach (e.g. a dev-only
    // path) still needs *some* depth; pick the deepest currently
    // assigned plus one so they sort first.
    let max_depth = depths.values().copied().max().unwrap_or(0);
    for m in transitive {
        depths.entry(m.canonical.clone()).or_insert(max_depth + 1);
    }
    depths
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
        AttrValue::Label(_)
        | AttrValue::LabelList(_)
        | AttrValue::LabelKeyedStringDict(_)
        | AttrValue::Selector(_)
        | AttrValue::Opaque(_) => None,
    }
}

/// Builder for [`Engine`] — preserves the simple `Engine::new` API
/// while letting callers opt into the external-repo root needed for
/// loading `.bzl`s from vendored bzlmod deps.
pub struct EngineBuilder {
    workspace_root: PathBuf,
    config: Configuration,
    external_repo_roots: Vec<RepoRoot>,
    bzlmod_lockfile_path: Option<PathBuf>,
    platforms_expr: Option<String>,
    bzlmod_cache: Option<PathBuf>,
    http_client: Option<reqwest::Client>,
    repo_specs: std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>,
    /// When set, the engine resolves the workspace's `bazel_dep`
    /// declarations against this registry (typically
    /// `https://bcr.bazel.build`) at build time. Without a registry,
    /// `bazel_dep`s must come from the lockfile, manual specs, or
    /// `external_repo_root`.
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
            external_repo_roots: Vec::new(),
            bzlmod_lockfile_path: None,
            platforms_expr: None,
            bzlmod_cache: None,
            http_client: None,
            repo_specs: std::collections::BTreeMap::new(),
            registry: None,
            repo_env_overrides: std::collections::BTreeMap::new(),
        }
    }

    /// Add a directory holding external bzlmod repos. Each canonical
    /// lives at `<root>/<canonical_name>/`. Typical inputs:
    /// `<workspace>/vendor` (output of `bazel mod vendor`) and
    /// `<bazel info output_base>/external`. Multiple calls accumulate;
    /// repo lookup tries each root in call order and returns the first
    /// match. `bazel mod vendor` only vendors bzlmod-declared repos —
    /// `local = True` repository_rule outputs (e.g. `host_platform_repo`)
    /// only live in `<output_base>/external`, so passing both lets the
    /// resolver fall back.
    pub fn external_repo_root(mut self, root: PathBuf) -> Self {
        self.external_repo_roots.push(RepoRoot::Filesystem(root));
        self
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
    /// repos must come from `external_repo_root(...)` as before.
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
        use loupe_bzlmod::{SetBzlmodCache, SetHttpClient, SetRepoSpecs};

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
        let build_dice =
            |workspace_root: &PathBuf,
             external_repo_roots: &[RepoRoot],
             bzlmod_lockfile_path: &Option<PathBuf>,
             platforms_expr: &Option<String>,
             bzlmod_cache: &Option<PathBuf>,
             http_client: Option<reqwest::Client>,
             repo_env_overrides: &std::collections::BTreeMap<
                String,
                loupe_bzlmod::env_policy::EnvOverride,
            >,
             initial_specs: std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>,
             resolved_bzlmod: Option<loupe_bzlmod::bzlmod_graph::BzlmodGraph>|
             -> Arc<Dice> {
                use loupe_bzlmod::bzlmod_graph::SetResolvedBzlmod;
                let mut builder = Dice::builder();
                builder.set_workspace_root(workspace_root.clone());
                let host = loupe_bzlmod::env_policy::host_env_snapshot();
                let resolved = loupe_bzlmod::env_policy::resolve_env(&host, repo_env_overrides);
                builder.set_declared_env(resolved);
                let mut roots: Vec<RepoRoot> = crate::builtins::primary_embedded_roots();
                roots.extend(external_repo_roots.iter().cloned());
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
                builder.set_repo_specs(initial_specs);
                if let Some(graph) = resolved_bzlmod {
                    builder.set_resolved_bzlmod(graph);
                }
                builder.build(dice::DetectCycles::Disabled)
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

        // Snapshot every EngineBuilder field that the closure consumes
        // so we can construct two Dice instances from one set of
        // inputs.
        let workspace_root = self.workspace_root;
        let external_repo_roots = self.external_repo_roots;
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
                         falling back to manually-supplied specs and external_repo_root"
                    );
                }
            }
        }

        // Preflight Dice. Drives BCR resolution and extension eval;
        // accumulates the final spec map plus the transitive-side
        // channels. Its `BzlLoadKey` cache will carry stale failures
        // for any repo that wasn't yet materialised when first asked
        // — that's why we throw it away before returning to the
        // caller.
        let preflight_dice = build_dice(
            &workspace_root,
            &external_repo_roots,
            &bzlmod_lockfile_path,
            &platforms_expr,
            &bzlmod_cache,
            http_client.clone(),
            &repo_env_overrides,
            initial_specs.clone(),
            None,
        );
        let engine = Engine {
            workspace_root: workspace_root.clone(),
            dice: preflight_dice,
            runtime,
            config: config.clone(),
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
            bzlmod_lockfile_path.clone(),
            &[],
        )?);

        // Registry resolution: if `with_registry(...)` was called,
        // walk the workspace's `bazel_dep`s through the BCR-format
        // registry, run MVS, and merge the resulting Http specs into
        // BzlmodRepoSpecs. Must run BEFORE use_extension eval — an
        // extension's defining `.bzl` may live in a registry-resolved
        // module (e.g. `@rules_oci//oci:extensions.bzl`).
        let mut transitive: Vec<loupe_bzlmod::resolution::TransitiveModuleInfo> = Vec::new();
        if let Some(registry) = registry.clone() {
            let output = engine.resolve_via_registry_full(&registry, &graph)?;
            transitive = output.transitive;
            let specs: std::collections::BTreeMap<_, _> = output
                .modules
                .into_iter()
                .map(|(k, m)| (k, m.spec))
                .collect();
            engine.merge_repo_specs(specs);

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
        // Fixed-point loop with Dice rebuild between rounds. Each
        // `evaluate_use_extensions` call evaluates as many pending
        // extensions as it can against the current Dice instance. If
        // any remain pending and we made progress, rebuild Dice (to
        // flush stale `BzlLoadKey` failures) and retry. Stops when
        // pending empties OR a pass makes no progress.
        let mut engine = engine;
        let mut round = 0;

        loop {
            round += 1;
            let output = evaluate_use_extensions(&engine, &graph, &transitive)?;
            engine.merge_repo_specs(output.specs);
            if output.pending.is_empty() {
                break;
            }
            if let Some(err) = output.last_err.as_ref()
                && round > 8
            {
                // Tolerate unevaluatable extensions. Many transitively-
                // pulled modules (`rules_python`, `rules_jvm_external`,
                // …) declare extensions that exercise corners of the
                // repo-rule API Loupe doesn't model yet; failing the
                // whole pipeline on them dooms fixtures that don't
                // actually reach those repos. Analysis still surfaces
                // the missing-repo errors per-target if it ends up
                // needing one.
                tracing::warn!(
                    round,
                    pending = output.pending.len(),
                    "evaluate_use_extensions: stopping after {round} rounds; \
                     {} extensions never converged. Last error: {err:#}",
                    output.pending.len()
                );
                break;
            }
            // Snapshot state and rebuild Dice. The pending extensions
            // failed inside the previous Dice because of cached
            // `BzlLoadKey` failures for repos that have *since* been
            // spec'd. The fresh Dice clears those caches.
            let snapshot_specs = {
                use loupe_bzlmod::HasRepoSpecs;
                let dice = Arc::clone(&engine.dice);
                engine.runtime.block_on(async move {
                    let txn = open_txn(&dice).await;
                    txn.global_data().all_repo_specs()
                })
            };
            let runtime = engine.runtime;
            drop(engine.dice);
            let new_dice = build_dice(
                &workspace_root,
                &external_repo_roots,
                &bzlmod_lockfile_path,
                &platforms_expr,
                &bzlmod_cache,
                http_client.clone(),
                &repo_env_overrides,
                snapshot_specs,
                None,
            );
            engine = Engine {
                workspace_root: workspace_root.clone(),
                dice: new_dice,
                runtime,
                config: config.clone(),
            };
        }

        // Final rebuild — even if the last round had zero pending,
        // the surviving Dice may carry stale `BzlLoadKey` Ok entries
        // that don't match the final spec set, so a clean rebuild
        // for analysis is still worthwhile.
        let final_specs = {
            use loupe_bzlmod::HasRepoSpecs;
            let dice = Arc::clone(&engine.dice);
            engine.runtime.block_on(async move {
                let txn = open_txn(&dice).await;
                txn.global_data().all_repo_specs()
            })
        };

        // Assemble the in-memory `BzlmodGraph` snapshot the final
        // `Dice`'s `ModuleGraphKey` will read. Sources:
        //
        // - `graph`: parsed main `MODULE.bazel` — gives the main
        //   module record (identity, bazel_deps with proper sep,
        //   use_extensions, register_toolchains).
        // - `transitive`: BCR resolver output — every transitive
        //   module's parsed `MODULE.bazel`.
        // - `final_specs`: every spec we registered (BCR-resolved
        //   modules + extension-generated repos). The latter group's
        //   canonicals are derived from their `<owner><sep><sep><ext><sep><repo>`
        //   shape.
        //
        // This is the one place where in-memory state is converted
        // into the source-agnostic snapshot. Once stored on the
        // final `Dice`, `ModuleGraphKey` synthesises `ModuleGraph`
        // from it — no `read_dir`, no race against lazy
        // materialisation, no side channels.
        let bzlmod_graph = build_bzlmod_graph(&graph, &transitive, &final_specs);

        let final_dice = build_dice(
            &workspace_root,
            &external_repo_roots,
            &bzlmod_lockfile_path,
            &platforms_expr,
            &bzlmod_cache,
            http_client,
            &repo_env_overrides,
            final_specs,
            Some(bzlmod_graph),
        );
        Ok(Engine {
            workspace_root,
            dice: final_dice,
            runtime: engine.runtime,
            config,
        })
    }
}

/// Walk `graph.use_extensions`, evaluate each `module_extension`'s
/// impl through [`Engine::build_module_extension_specs`], and merge
/// the captured specs into one map. Stops at the first eval failure
/// so a broken extension surfaces as an engine-build error rather
/// than a silently-incomplete dep graph.
fn evaluate_use_extensions(
    engine: &Engine,
    graph: &std::sync::Arc<crate::module_graph::ModuleGraph>,
    transitive: &[loupe_bzlmod::resolution::TransitiveModuleInfo],
) -> Result<EvaluateOutput> {
    use anyhow::Context as _;
    use loupe_bzlmod::ModuleTagInvocation;
    use loupe_bzlmod::ModuleView;
    use loupe_bzlmod::RepoAttrValue;

    fn convert_attr(a: &crate::module_graph::TagAttrLiteral) -> RepoAttrValue {
        match a {
            crate::module_graph::TagAttrLiteral::String(s) => RepoAttrValue::String(s.clone()),
            crate::module_graph::TagAttrLiteral::Int(i) => RepoAttrValue::Int(*i),
            crate::module_graph::TagAttrLiteral::Bool(b) => RepoAttrValue::Bool(*b),
            crate::module_graph::TagAttrLiteral::StringList(l) => {
                RepoAttrValue::StringList(l.clone())
            }
            crate::module_graph::TagAttrLiteral::StringDict(d) => {
                RepoAttrValue::StringDict(d.clone())
            }
        }
    }

    let main_pkg = PackageId::new(
        graph.main.canonical.clone(),
        loupe_labels::PackagePath::root(),
    );
    let main_name = graph.main.name.clone();
    let main_version = graph.main.version.clone();
    let owner = graph.main.canonical.as_str().to_string();
    let sep = graph.canonical_separator();

    // For each transitive module, pre-compute its label-resolution
    // mapping (`apparent → canonical`) plus its own self-reference.
    // Used to canonicalise each transitive `use_extension` bzl_label
    // so we can match it against the main module's bzl_label
    // (which is canonicalised through `graph.repo_mapping`).
    let mut transitive_canonical_extensions: Vec<(
        Label,
        String,
        &loupe_bzlmod::resolution::TransitiveModuleInfo,
        &loupe_bzlmod::resolution::UseExtensionLite,
    )> = Vec::new();
    for m in transitive {
        // Skip dev-only use_extension records from non-root modules.
        // Per Bazel: a `use_extension(..., dev_dependency = True)` is
        // ignored entirely when its declaring module isn't the root.
        // Aspect_rules_js's `node_dev` extension is the canonical case
        // — it registers transitive-only toolchains the upstream
        // extension explicitly refuses.
        let mut mapping: std::collections::BTreeMap<String, loupe_labels::CanonicalRepo> =
            std::collections::BTreeMap::new();
        // The module's own apparent name maps to its canonical.
        mapping.insert(
            m.name.clone(),
            loupe_labels::CanonicalRepo::new(m.canonical.clone()),
        );
        for (apparent, canonical) in &m.bazel_deps {
            mapping.insert(
                apparent.clone(),
                loupe_labels::CanonicalRepo::new(canonical.clone()),
            );
        }
        let pkg = PackageId::new(
            loupe_labels::CanonicalRepo::new(m.canonical.clone()),
            loupe_labels::PackagePath::root(),
        );
        for ue in &m.use_extensions {
            if ue.dev_dependency {
                continue;
            }
            let Ok(canon) = Label::resolve(&ue.bzl_label, &pkg, &mapping) else {
                continue;
            };
            transitive_canonical_extensions.push((canon, ue.name.clone(), m, ue));
        }
    }

    // Build (canonical bzl_label, ext_name) → ordered list of
    // (module_view, tag invocations). Main goes first as the root
    // module; transitive modules follow in resolution order. The same
    // extension declared by both main and transitives produces one
    // evaluation with the merged module list — Bazel's "extension runs
    // once with every consumer's tags" semantics.
    type ExtKey = (Label, String);
    let mut grouped: std::collections::BTreeMap<ExtKey, Vec<ModuleView>> =
        std::collections::BTreeMap::new();

    for rec in &graph.use_extensions {
        let bzl_label = Label::resolve(&rec.bzl_label, &main_pkg, &graph.repo_mapping)
            .with_context(|| format!("resolving use_extension bzl_label `{}`", rec.bzl_label))?;
        let main_tags: Vec<ModuleTagInvocation> = rec
            .tags
            .iter()
            .map(|t| ModuleTagInvocation {
                class: t.class.clone(),
                attrs: t
                    .attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), convert_attr(v)))
                    .collect(),
            })
            .collect();
        grouped
            .entry((bzl_label, rec.name.clone()))
            .or_default()
            .push(ModuleView {
                name: main_name.clone(),
                version: main_version.clone(),
                is_root: true,
                tags: main_tags,
            });
    }

    // Append transitive modules' use_extensions to the same groups, or
    // start a new group if main doesn't declare the same extension.
    // Transitive extensions still need evaluation: `aspect_bazel_lib`
    // self-declares `bazel_lib_toolchains` and registers its outputs
    // via `register_toolchains(...)`, even when main never names the
    // extension — without this loop those generated repos are missing.
    for (t_label, t_name, t_module, t_ue) in &transitive_canonical_extensions {
        let t_tags: Vec<ModuleTagInvocation> = t_ue
            .tags
            .iter()
            .map(|t| ModuleTagInvocation {
                class: t.class.clone(),
                attrs: t.attrs.clone(),
            })
            .collect();
        grouped
            .entry((t_label.clone(), t_name.clone()))
            .or_default()
            .push(ModuleView {
                name: t_module.name.clone(),
                version: t_module.version.clone(),
                is_root: false,
                tags: t_tags,
            });
    }

    // Order groups so deeper-in-the-dep-graph modules' extensions run
    // first. The dependency-of-an-extension's output is itself almost
    // always a deeper module — e.g. `aspect_rules_js`'s `pnpm` loads
    // from `bazel_features`, which is one hop further from main. If
    // `pnpm` runs first, DICE caches the failed `BzlLoadKey` for
    // `@bazel_features_globals//:globals.bzl` and every subsequent
    // load (even after `version_extension` registers the spec) sees
    // the stale failure. Processing deeper modules first sidesteps
    // that DICE invalidation problem without requiring a real
    // dependency analysis.
    let depths: std::collections::BTreeMap<String, usize> =
        compute_module_depths(graph, transitive);
    let mut pending: Vec<((Label, String), Vec<ModuleView>)> = grouped.into_iter().collect();
    // Sort key: shallower modules first (depth ascending). Rationale:
    // load chains in bzlmod always go from a consumer toward its
    // bazel_deps; an extension declared by module M only loads bzls
    // from modules M depends on. If we process M's extensions BEFORE
    // any of M's deps' extensions, M's own loads can't reference any
    // not-yet-generated repos from those deps — because there are no
    // cross-extension load edges that go "downward" through the dep
    // graph. Shallower-first puts modules close to main (which sit
    // ABOVE the deepest modules in the dep graph) first; their
    // extensions only load bzls from their own deps, which haven't
    // generated anything yet — guaranteed safe.
    //
    // Wait — that's not right either. Re-derivation: an extension's
    // .bzl loads from the SAME module's other .bzl files and from
    // its bazel_deps. If we process M's extensions FIRST, and M's
    // bazel_dep N has an extension that generates a repo M's .bzl
    // loads, then M's extension eval triggers a load of N's
    // ext-generated repo, which fails because N hasn't run yet.
    //
    // So: DEEPER modules' extensions first. Their .bzl files only
    // load from their own (even deeper) deps; nothing they load can
    // depend on a shallower module's not-yet-run extension. This is
    // what the descending sort below produces.
    pending.sort_by(|a, b| {
        let depth_a = depths
            .get(a.0.0.package.repo.as_str())
            .copied()
            .unwrap_or(usize::MAX);
        let depth_b = depths
            .get(b.0.0.package.repo.as_str())
            .copied()
            .unwrap_or(usize::MAX);
        depth_b
            .cmp(&depth_a)
            .then_with(|| a.0.0.cmp(&b.0.0))
            .then_with(|| a.0.1.cmp(&b.0.1))
    });
    let mut out = std::collections::BTreeMap::new();
    let mut pass = 0;
    loop {
        pass += 1;
        let mut next_pending: Vec<((Label, String), Vec<ModuleView>)> = Vec::new();
        let mut last_err: Option<anyhow::Error> = None;
        let mut progressed = false;
        for ((bzl_label, ext_name), modules) in pending.into_iter() {
            // Owner = the module that *defines* the extension (the .bzl's
            // repo). For main-defined extensions this is `main.canonical`
            // and matches Bazel's `<empty>+ext+repo` shape; for
            // transitively-defined extensions (e.g. `aspect_bazel_lib`'s
            // `bazel_lib_toolchains`) it's `aspect_bazel_lib~`, producing
            // `aspect_bazel_lib~~toolchains~<repo>` — the canonical Bazel
            // emits.
            let owner_canonical = bzl_label.package.repo.as_str();
            let owner_for_canonical: String = if owner_canonical == graph.main.canonical.as_str() {
                owner.clone()
            } else {
                owner_canonical.to_string()
            };
            let specs_result = engine
                .build_module_extension_specs(
                    &bzl_label,
                    &ext_name,
                    &owner_for_canonical,
                    sep,
                    &ext_name,
                    modules.clone(),
                )
                .with_context(|| format!("evaluating use_extension `{}`", ext_name));
            match specs_result {
                Ok(specs) => {
                    // Merge each extension's specs back into the
                    // engine's `BzlmodRepoSpecs` immediately so
                    // subsequent extensions can resolve loads of repos
                    // this one generated.
                    engine.merge_repo_specs(specs.clone());
                    for (k, v) in specs {
                        out.entry(k).or_insert(v);
                    }
                    progressed = true;
                }
                Err(e) => {
                    last_err = Some(e);
                    next_pending.push(((bzl_label, ext_name), modules));
                }
            }
        }
        if next_pending.is_empty() || !progressed {
            // Either we're done or we've hit a fixed point inside
            // this Dice instance. Return what succeeded plus whichever
            // groups still need evaluating; the caller decides whether
            // to rebuild Dice and try again or to surface `last_err`.
            return Ok(EvaluateOutput {
                specs: out,
                pending: next_pending,
                last_err,
            });
        }
        pending = next_pending;
        if pass > 32 {
            anyhow::bail!(
                "evaluate_use_extensions: exceeded {} retry passes — \
                 likely a circular dependency or unmodelled extension surface",
                pass
            );
        }
    }
}

/// Output of one `evaluate_use_extensions` call: every spec the
/// engine managed to derive plus any groups that couldn't be
/// evaluated against the current `Dice` instance. Callers iterate by
/// rebuilding `Dice` (to flush stale failed `BzlLoadKey` entries)
/// and re-driving with `pending` until it empties or progress stops.
struct EvaluateOutput {
    specs: std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>,
    pending: Vec<((Label, String), Vec<loupe_bzlmod::ModuleView>)>,
    last_err: Option<anyhow::Error>,
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
            tags: vec![ModuleTagInvocation {
                class: "toolchain".into(),
                attrs: std::collections::BTreeMap::new(),
            }],
        }];
        apply_tag_class_defaults(ext_fv, &mut modules);
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
            tags: vec![ModuleTagInvocation {
                class: "toolchain".into(),
                attrs,
            }],
        }];
        apply_tag_class_defaults(ext_fv, &mut modules);
        assert_eq!(
            modules[0].tags[0].attrs.get("name"),
            Some(&RepoAttrValue::String("explicit".into())),
            "user-supplied attr value must win over tag_class default"
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
    fn effective_repo_mapping_strs_seeds_at_self_ref() {
        // `@//pkg:label` (apparent name empty) must resolve to the
        // owning module of the defining .bzl. Without this seed,
        // rules calling `Label("@//foo:bar")` from inside an external
        // repo hit `no repo_mapping entry for apparent name @`.
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let graph = engine.module_graph().unwrap();

        // .bzl owned by main → `""` seed maps to main's canonical.
        let main_bzl = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "defs.bzl");
        let main_map = effective_repo_mapping_strs(&graph, &main_bzl);
        assert_eq!(
            main_map.get(""),
            Some(&CanonicalRepo::MAIN.as_str().to_string())
        );

        // .bzl owned by an external repo → `""` seed maps to that repo.
        let ext_bzl = Label::new(
            CanonicalRepo::new("rules_multitool+"),
            PackagePath::root(),
            "multitool.bzl",
        );
        let ext_map = effective_repo_mapping_strs(&graph, &ext_bzl);
        assert_eq!(ext_map.get(""), Some(&"rules_multitool+".to_string()));
    }
}
