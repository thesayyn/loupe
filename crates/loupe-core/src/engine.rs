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
    AnalysisKey, BzlLoadKey, DiscoverPackagesKey, ModuleGraphKey, PackageKey, SetBzlmodLockfilePath,
    SetExternalRepoRoots, SetPlatformsExpr, SetWorkspaceRoot,
};
use loupe_labels::{Label, PackageId};
use crate::module_graph::ModuleGraph;
use crate::package::Package;
use crate::repo_root::RepoRoot;

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
        let skip: std::collections::BTreeSet<&str> = skip_kinds.iter().copied().collect();
        let packages = self.discover_packages()?;
        let mut report = crate::cquery::CqueryReport::default();
        let mut analyzed: Vec<Arc<AnalyzedTarget>> = Vec::new();
        for pkg in packages {
            let package = match self.query_package(&pkg) {
                Ok(p) => p,
                Err(e) => {
                    report.package_errors.push(PackageError {
                        package: pkg,
                        error: format!("{e:#}"),
                    });
                    continue;
                }
            };
            for target in package.targets.values() {
                if skip.contains(target.kind.as_str()) {
                    report.skipped.push(SkippedTarget {
                        label: target.label.clone(),
                        kind: target.kind.clone(),
                    });
                    continue;
                }
                match self.analyze_target(&target.label) {
                    Ok(at) => analyzed.push(at),
                    Err(e) => report.failed.push(FailedTarget {
                        label: target.label.clone(),
                        kind: target.kind.clone(),
                        error: format!("{e:#}"),
                    }),
                }
            }
        }
        analyzed.sort_by(|a, b| a.label.cmp(&b.label));
        report.analyzed = analyzed.len();
        report.output = crate::cquery::run_script(&script, &analyzed)?;
        Ok(report)
    }

    #[instrument(skip(self, skip_kinds))]
    pub fn analyze_workspace(&self, skip_kinds: &[&str]) -> Result<WorkspaceAnalysis> {
        let skip: std::collections::BTreeSet<&str> = skip_kinds.iter().copied().collect();
        let packages = self.discover_packages()?;
        let mut out = WorkspaceAnalysis::default();
        for pkg in packages {
            let package = match self.query_package(&pkg) {
                Ok(p) => p,
                Err(e) => {
                    out.package_errors.push(PackageError {
                        package: pkg,
                        error: format!("{e:#}"),
                    });
                    continue;
                }
            };
            for target in package.targets.values() {
                if skip.contains(target.kind.as_str()) {
                    out.skipped.push(SkippedTarget {
                        label: target.label.clone(),
                        kind: target.kind.clone(),
                    });
                    continue;
                }
                match self.analyze_target(&target.label) {
                    Ok(at) => out.analyzed.push(at),
                    Err(e) => out.failed.push(FailedTarget {
                        label: target.label.clone(),
                        kind: target.kind.clone(),
                        error: format!("{e:#}"),
                    }),
                }
            }
        }
        Ok(out)
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
            let txn = open_txn(&dice).await;
            let canonicals = txn.global_data().known_canonicals();
            let mut report = FetchReport::default();
            for canonical in canonicals {
                let key = MaterializedRepoKey(canonical.clone());
                let mut local = open_txn(&dice).await;
                match local.compute(&key).await {
                    Ok(BzlmodResult::Ok(path)) => {
                        report.materialised.push((canonical, (*path).clone()))
                    }
                    Ok(BzlmodResult::Err(e)) => report
                        .errors
                        .push((canonical, Arc::unwrap_or_clone(e))),
                    Err(e) => report.errors.push((canonical, format!("{e}"))),
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
        modules: Vec<loupe_bzlmod::ModuleView>,
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

        let output = loupe_bzlmod::eval_module_extension(frozen_value, &module, modules)
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
    graph
        .effective_repo_mapping(&pkg)
        .into_iter()
        .map(|(k, v)| (k, v.as_str().to_string()))
        .collect()
}

async fn open_txn(dice: &Arc<Dice>) -> DiceTransaction {
    dice.updater().commit().await
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
    repo_env_overrides:
        std::collections::BTreeMap<String, loupe_bzlmod::env_policy::EnvOverride>,
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

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut builder = Dice::builder();
        builder.set_workspace_root(self.workspace_root.clone());
        // Resolve the effective env once at engine-build time so the
        // same map drives both rule eval and marker revalidation.
        {
            let host = loupe_bzlmod::env_policy::host_env_snapshot();
            let resolved =
                loupe_bzlmod::env_policy::resolve_env(&host, &self.repo_env_overrides);
            builder.set_declared_env(resolved);
        }
        let mut roots: Vec<RepoRoot> = crate::builtins::primary_embedded_roots();
        roots.extend(self.external_repo_roots);
        roots.extend(crate::builtins::fallback_embedded_roots());
        builder.set_external_repo_roots(roots);
        builder.set_bzlmod_lockfile_path(self.bzlmod_lockfile_path.clone());
        builder.set_platforms_expr(self.platforms_expr);
        if let Some(cache) = self.bzlmod_cache {
            builder.set_bzlmod_cache(cache);
            let client = self.http_client.unwrap_or_else(reqwest::Client::new);
            builder.set_http_client(client);

            // Auto-load `generatedRepoSpecs` from MODULE.bazel.lock when
            // the caller didn't pre-supply specs. Manually-passed specs
            // take precedence; lockfile entries fill in everything else.
            // Errors downgrade to warnings — a missing or malformed
            // lockfile shouldn't block engine construction, since users
            // can still provide specs via `with_repo_specs` or rely on
            // `external_repo_root` for vendored deps.
            let lockfile_path = self
                .bzlmod_lockfile_path
                .unwrap_or_else(|| self.workspace_root.join("MODULE.bazel.lock"));
            let mut effective_specs = self.repo_specs;
            match loupe_bzlmod::lockfile_specs::load_from_path(&lockfile_path) {
                Ok(loaded) => {
                    for (canonical, spec) in loaded {
                        effective_specs.entry(canonical).or_insert(spec);
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
            builder.set_repo_specs(effective_specs);
        }
        let dice = builder.build(dice::DetectCycles::Disabled);
        let engine = Engine {
            workspace_root: self.workspace_root,
            dice,
            runtime,
            config: self.config,
        };
        let graph = engine.module_graph()?;

        // Registry resolution: if `with_registry(...)` was called,
        // walk the workspace's `bazel_dep`s through the BCR-format
        // registry, run MVS, and merge the resulting Http specs into
        // BzlmodRepoSpecs. Must run BEFORE use_extension eval — an
        // extension's defining `.bzl` may live in a registry-resolved
        // module (e.g. `@rules_oci//oci:extensions.bzl`).
        let mut transitive: Vec<loupe_bzlmod::resolution::TransitiveModuleInfo> = Vec::new();
        if let Some(registry) = self.registry.clone() {
            let output = engine.resolve_via_registry_full(&registry, &graph)?;
            transitive = output.transitive;
            let specs: std::collections::BTreeMap<_, _> = output
                .modules
                .into_iter()
                .map(|(k, m)| (k, m.spec))
                .collect();
            engine.merge_repo_specs(specs);
            // Extension `.bzl`s reach registry-resolved repos through
            // the regular lazy-fetch path (BzlLoadKey →
            // MaterializedRepoKey), so no eager fetch is needed here.
        }

        // Auto-evaluate captured `use_extension(...)` declarations from
        // MODULE.bazel and merge resulting specs into the engine's
        // BzlmodRepoSpecs. Only fires when there are records to process.
        if !graph.use_extensions.is_empty() {
            let extra_specs = evaluate_use_extensions(&engine, &graph, &transitive)?;
            engine.merge_repo_specs(extra_specs);
        }
        Ok(engine)
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
) -> Result<std::collections::BTreeMap<String, loupe_bzlmod::RepoSpec>> {
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

    let main_pkg = PackageId::new(graph.main.canonical.clone(), loupe_labels::PackagePath::root());
    let main_name = graph.main.name.clone();
    let main_version = graph.main.version.clone();
    let owner = graph.main.canonical.as_str().to_string();
    let sep = graph.canonical_separator();

    // For each transitive module, pre-compute its label-resolution
    // mapping (`apparent → canonical`) plus its own self-reference.
    // Used to canonicalise each transitive `use_extension` bzl_label
    // so we can match it against the main module's bzl_label
    // (which is canonicalised through `graph.repo_mapping`).
    let mut transitive_canonical_extensions: Vec<(Label, String, &loupe_bzlmod::resolution::TransitiveModuleInfo, &loupe_bzlmod::resolution::UseExtensionLite)> =
        Vec::new();
    for m in transitive {
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
            let Ok(canon) = Label::resolve(&ue.bzl_label, &pkg, &mapping) else {
                continue;
            };
            transitive_canonical_extensions.push((canon, ue.name.clone(), m, ue));
        }
    }

    let mut out = std::collections::BTreeMap::new();
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
        let mut modules = vec![ModuleView {
            name: main_name.clone(),
            version: main_version.clone(),
            is_root: true,
            tags: main_tags,
        }];

        // Append any transitive modules that invoke the same extension.
        for (t_label, t_name, t_module, t_ue) in &transitive_canonical_extensions {
            if t_label == &bzl_label && t_name == &rec.name {
                let t_tags: Vec<ModuleTagInvocation> = t_ue
                    .tags
                    .iter()
                    .map(|t| ModuleTagInvocation {
                        class: t.class.clone(),
                        attrs: t.attrs.clone(),
                    })
                    .collect();
                modules.push(ModuleView {
                    name: t_module.name.clone(),
                    version: t_module.version.clone(),
                    is_root: false,
                    tags: t_tags,
                });
            }
        }

        let specs = engine
            .build_module_extension_specs(&bzl_label, &rec.name, &owner, sep, &rec.name, modules)
            .with_context(|| format!("evaluating use_extension `{}`", rec.name))?;
        for (k, v) in specs {
            out.entry(k).or_insert(v);
        }
    }
    Ok(out)
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
    fn module_graph_records_main_module_name() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let graph = engine.module_graph().unwrap();
        assert_eq!(graph.main.name, "demo");
    }
}
