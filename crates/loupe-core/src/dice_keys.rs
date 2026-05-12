//! DICE keys and the workspace data they read from.
//!
//! Phase 3: `BzlLoadKey` is real — its compute fn parses the target
//! `.bzl`, recursively forces `BzlLoadKey` for every transitive `load`,
//! and returns a `FrozenModule`. `PackageKey` does the same for the
//! BUILD file: forces `BzlLoadKey` for each load statement, then runs
//! the synchronous BUILD evaluator with the resolved modules in hand.
//! `ModuleGraphKey` parses `MODULE.bazel`; results plus the v1 repo
//! mapping flow into label resolution at every load.
//!
//! Errors propagate as `SharedResult<T> = Ok(Arc<T>) | Err(Arc<String>)`
//! — `anyhow::Error` doesn't implement `Allocative`, and
//! `SharedResult<T>` is the smallest shape that satisfies DICE's
//! `Value: Allocative + Dupe + Send + Sync + 'static` bound while
//! still letting the engine surface stay `anyhow::Result<T>` shaped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use dice::{
    CancellationContext, DiceComputations, DiceData, DiceDataBuilder, Key, NoValueSerialize,
    ValueSerialize,
};
use dupe::Dupe;
use pagable::{Pagable, pagable_typetag};
use starlark::environment::FrozenModule;

use crate::analysis::{self, AnalyzedTarget, Configuration};
use crate::build_eval;
use crate::bzl_eval;
use crate::label::{Label, PackageId};
use crate::module_graph::{self, ModuleGraph};
use crate::package::Package;

/// `Ok(Arc<T>)` or a stringified error. Both arms are cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub enum SharedResult<T: Allocative + 'static> {
    Ok(Arc<T>),
    Err(Arc<String>),
}

impl<T: Allocative + Clone + 'static> Dupe for SharedResult<T> {}

impl<T: Allocative + 'static> SharedResult<T> {
    pub fn err(msg: impl Into<String>) -> Self {
        SharedResult::Err(Arc::new(msg.into()))
    }
    pub fn ok(value: T) -> Self {
        SharedResult::Ok(Arc::new(value))
    }
    pub fn into_anyhow(self) -> anyhow::Result<Arc<T>> {
        match self {
            SharedResult::Ok(v) => Ok(v),
            SharedResult::Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }
}

#[derive(Debug, Clone, Allocative)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

pub trait HasWorkspaceRoot {
    fn workspace_root(&self) -> &Path;
}

impl HasWorkspaceRoot for DiceData {
    fn workspace_root(&self) -> &Path {
        self.get::<WorkspaceRoot>()
            .expect("WorkspaceRoot was not set on DiceData")
            .as_path()
    }
}

pub trait SetWorkspaceRoot {
    fn set_workspace_root(&mut self, root: PathBuf);
}

impl SetWorkspaceRoot for DiceDataBuilder {
    fn set_workspace_root(&mut self, root: PathBuf) {
        self.set(WorkspaceRoot(root));
    }
}

/// Optional root of the external-repo tree. When set, label loads
/// targeting non-main repos (`@<canonical>//pkg:foo`) resolve to
/// `<root>/<canonical>/<pkg>/<file>`. Layout matches both `bazel info
/// output_base`'s `external/` directory and `bazel mod vendor`'s
/// vendor directory.
#[derive(Debug, Clone, Allocative)]
pub struct ExternalRepoRoot(Option<PathBuf>);

impl ExternalRepoRoot {
    pub fn as_path(&self) -> Option<&Path> {
        self.0.as_deref()
    }
}

pub trait HasExternalRepoRoot {
    fn external_repo_root(&self) -> Option<&Path>;
}

impl HasExternalRepoRoot for DiceData {
    fn external_repo_root(&self) -> Option<&Path> {
        self.get::<ExternalRepoRoot>()
            .map(|r| r.as_path())
            .unwrap_or(None)
    }
}

pub trait SetExternalRepoRoot {
    fn set_external_repo_root(&mut self, root: Option<PathBuf>);
}

impl SetExternalRepoRoot for DiceDataBuilder {
    fn set_external_repo_root(&mut self, root: Option<PathBuf>) {
        self.set(ExternalRepoRoot(root));
    }
}

/// Optional override for the `MODULE.bazel.lock` path. Defaults to
/// `<workspace>/MODULE.bazel.lock` when unset; explicit override is
/// used when the rich (`lockFileVersion: 26`) copy lives at
/// `<output_base>/MODULE.bazel.lock` instead.
#[derive(Debug, Clone, Allocative)]
pub struct BzlmodLockfilePath(Option<PathBuf>);

impl BzlmodLockfilePath {
    pub fn as_path(&self) -> Option<&Path> {
        self.0.as_deref()
    }
}

pub trait HasBzlmodLockfilePath {
    fn bzlmod_lockfile_path(&self) -> Option<&Path>;
}

impl HasBzlmodLockfilePath for DiceData {
    fn bzlmod_lockfile_path(&self) -> Option<&Path> {
        self.get::<BzlmodLockfilePath>()
            .map(|p| p.as_path())
            .unwrap_or(None)
    }
}

pub trait SetBzlmodLockfilePath {
    fn set_bzlmod_lockfile_path(&mut self, path: Option<PathBuf>);
}

impl SetBzlmodLockfilePath for DiceDataBuilder {
    fn set_bzlmod_lockfile_path(&mut self, path: Option<PathBuf>) {
        self.set(BzlmodLockfilePath(path));
    }
}

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ModuleGraphKey")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ModuleGraphKey;

#[async_trait]
impl Key for ModuleGraphKey {
    type Value = SharedResult<ModuleGraph>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let root = ctx.global_data().workspace_root().to_path_buf();
        let lockfile_override = ctx
            .global_data()
            .bzlmod_lockfile_path()
            .map(|p| p.to_path_buf());
        let external_root = ctx
            .global_data()
            .external_repo_root()
            .map(Path::to_path_buf);
        match module_graph::parse_with_lockfile_override(
            &root,
            lockfile_override,
            external_root.as_deref(),
        ) {
            Ok(g) => SharedResult::ok(g),
            Err(e) => SharedResult::err(format!("{e:#}")),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// What `BzlLoadKey` returns. Carries an `Arc<FrozenModule>` so DICE's
/// `Dupe` bound is cheap; equality is pointer identity (a recompute
/// produces a fresh frozen heap, so downstream invalidates).
#[derive(Debug, Clone, Allocative)]
pub struct LoadedBzl {
    pub module: Arc<FrozenModule>,
}

impl PartialEq for LoadedBzl {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.module, &other.module)
    }
}
impl Eq for LoadedBzl {}
impl Dupe for LoadedBzl {}

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BzlLoadKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct BzlLoadKey(pub Label);

#[async_trait]
impl Key for BzlLoadKey {
    type Value = SharedResult<LoadedBzl>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let graph = match force_module_graph(ctx).await {
            Ok(g) => g,
            Err(e) => return SharedResult::err(e),
        };
        match compute_bzl_load(ctx, &self.0, graph.as_ref()).await {
            Ok(lb) => SharedResult::Ok(Arc::new(lb)),
            Err(e) => SharedResult::err(e),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("PackageKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct PackageKey(pub PackageId);

#[async_trait]
impl Key for PackageKey {
    type Value = SharedResult<Package>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let graph = match force_module_graph(ctx).await {
            Ok(g) => g,
            Err(e) => return SharedResult::err(e),
        };
        match compute_package(ctx, &self.0, graph.as_ref()).await {
            Ok(pkg) => SharedResult::ok(pkg),
            Err(e) => SharedResult::err(e),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Sorted list of every package in the workspace. Singleton DICE key
/// so a second `Engine::analyze_workspace` (or any other walk) hits
/// cache instead of re-stat'ing the tree. Pairs with `PackageKey` —
/// once this returns, every per-package compute is already DICE-keyed.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("DiscoverPackagesKey")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct DiscoverPackagesKey;

#[async_trait]
impl Key for DiscoverPackagesKey {
    type Value = SharedResult<Vec<PackageId>>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let root = ctx.global_data().workspace_root().to_path_buf();
        match crate::workspace::discover_packages(&root) {
            Ok(pkgs) => SharedResult::ok(pkgs),
            Err(e) => SharedResult::err(format!("{e:#}")),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Effective constraint set of a `Configuration`'s target platform.
/// `Some(set)` when a platform was configured and at least one
/// `platform(...)` target loaded; `None` when no platform was
/// configured or the platform target couldn't be loaded (callers
/// should treat that as "no constraint check"). Per-`Configuration`
/// memoization means every target+toolchain in one cquery shares
/// one resolved set — Bazel re-derives it per-target.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct TargetPlatformConstraints(pub Option<std::collections::BTreeSet<Label>>);

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("TargetPlatformConstraintsKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct TargetPlatformConstraintsKey(pub Configuration);

#[async_trait]
impl Key for TargetPlatformConstraintsKey {
    type Value = SharedResult<TargetPlatformConstraints>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let graph = match force_module_graph(ctx).await {
            Ok(g) => g,
            Err(e) => return SharedResult::err(e),
        };
        let constraints = match self.0.target_platform() {
            Some(p) => load_target_platform_constraints(ctx, p, &graph.repo_mapping).await,
            None => None,
        };
        SharedResult::ok(TargetPlatformConstraints(constraints))
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// `select()`-resolved attrs for a target+config. Resolves every
/// `Selector` against `TargetPlatformConstraintsKey`'s output, so
/// downstream label-dep collection and `run_analysis` see concrete
/// `AttrValue`s. Source-file / native-only targets (no `bzl_label`)
/// short-circuit to an empty map — they never hit `run_analysis`.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct MaterialisedAttrs(pub BTreeMap<String, crate::package::AttrValue>);

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("MaterialisedAttrsKey({_0}, {_1})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct MaterialisedAttrsKey(pub Label, pub Configuration);

#[async_trait]
impl Key for MaterialisedAttrsKey {
    type Value = SharedResult<MaterialisedAttrs>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        match compute_materialised_attrs(ctx, &self.0, &self.1).await {
            Ok(m) => SharedResult::ok(m),
            Err(e) => SharedResult::err(e),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Resolved toolchains for a target+config. Reads the rule's
/// `toolchains=[...]` declaration off its defining `.bzl`, walks
/// `ModuleGraph::registered_toolchains`, and force-analyzes the
/// first compatible match per declared type. Returns an empty
/// `Toolchains` for source-file / native-only targets.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ResolvedToolchainsKey({_0}, {_1})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ResolvedToolchainsKey(pub Label, pub Configuration);

#[async_trait]
impl Key for ResolvedToolchainsKey {
    type Value = SharedResult<crate::starlark_types::toolchains::Toolchains>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        match compute_resolved_toolchains(ctx, &self.0, &self.1).await {
            Ok(t) => SharedResult::ok(t),
            Err(e) => SharedResult::err(e),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        // `Toolchains` holds `Arc<AnalyzedTarget>`s whose FrozenValue
        // fields can't implement `Eq`. Pointer-compare the
        // `SharedResult` payload — a recompute always allocates a
        // fresh `Toolchains`, so this gives the right "did the value
        // change" answer. Same trick `LoadedBzl` uses.
        match (x, y) {
            (SharedResult::Ok(a), SharedResult::Ok(b)) => Arc::ptr_eq(a, b),
            (SharedResult::Err(a), SharedResult::Err(b)) => a == b,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Analyze a configured target. Forces the target's `PackageKey`
/// (yielding the `UnconfiguredTarget`), then `BzlLoadKey` for the
/// rule's defining `.bzl` (yielding the `FrozenRuleClass`), and
/// finally runs the synchronous analysis driver in `crate::analysis`.
///
/// As of the DICE-key decomposition: this key orchestrates three
/// sub-keys — `MaterialisedAttrsKey` (selects → concrete attrs),
/// `ResolvedToolchainsKey` (rule's `toolchains=` → `Toolchains`),
/// and recursive `AnalysisKey` for every label-typed dep — then
/// invokes `analysis::run_analysis`. Each sub-key is independently
/// memoized so e.g. changing the target platform invalidates
/// `TargetPlatformConstraintsKey` but reusable bzl loads stay hot.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("AnalysisKey({_0}, {_1})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct AnalysisKey(pub Label, pub Configuration);

#[async_trait]
impl Key for AnalysisKey {
    type Value = SharedResult<AnalyzedTarget>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        match compute_analysis(ctx, &self.0, &self.1).await {
            Ok(v) => SharedResult::ok(v),
            Err(e) => SharedResult::err(e),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

async fn compute_analysis(
    ctx: &mut DiceComputations<'_>,
    target_label: &Label,
    config: &Configuration,
) -> Result<AnalyzedTarget, String> {
    // Force the package to discover whether the target needs the full
    // pipeline or short-circuits as a source file / native-only kind.
    // `MaterialisedAttrsKey` would also re-force `PackageKey` — DICE
    // memoizes, so this lookup is free on the second hit.
    let pkg = force_package(ctx, &target_label.package).await?;

    let (bzl_label, source_symbol) = match pkg.targets.get(target_label.name()) {
        None => {
            // STUB: Bazel auto-discovers source files referenced by
            // label even without an `exports_files(...)`. v1
            // synthesises an UnconfiguredTarget on the fly so the
            // analysis chain returns a source-file DefaultInfo.
            // Doesn't check the file actually exists on disk.
            let synthetic = crate::package::UnconfiguredTarget {
                label: target_label.clone(),
                kind: "source_file".to_string(),
                bzl_source: "<auto-discovered>".to_string(),
                bzl_label: None,
                source_symbol: None,
                attrs: std::collections::BTreeMap::new(),
            };
            return Ok(synthesise_source_file_analysis(&synthetic, config));
        }
        Some(t) => match (&t.bzl_label, &t.source_symbol) {
            (Some(l), Some(s)) => (l.clone(), s.clone()),
            // Native rules without a Starlark impl. `alias` keeps its
            // bespoke synthesis (provider forwarding can't be a
            // regular `rule(...)`). User-space rules from
            // `bzl/builtins_core/exports.bzl` (filegroup, sh_*,
            // cc_library, genrule, test_suite) carry `bzl_label =
            // Some(@_builtins//...)` and reach the `(Some, Some)`
            // arm above. Other native kinds still in `native_rules.rs`
            // (config_setting, platform, constraint_*, toolchain*)
            // are loading-only — they record an UnconfiguredTarget
            // for the resolver, never invoke an impl. They land here
            // and get a synthetic single-File DefaultInfo, which is
            // the right shape for `bazel cquery` introspection.
            _ => match t.kind.as_str() {
                "alias" => {
                    return synthesise_alias_analysis(ctx, t, config).await;
                }
                "filegroup" => {
                    return Ok(synthesise_filegroup_analysis(t, config));
                }
                "_loupe_command_line_option" => {
                    return Ok(synthesise_command_line_option_analysis(t, config));
                }
                _ => return Ok(synthesise_source_file_analysis(t, config)),
            },
        },
    };

    // Force decomposed sub-keys: materialised attrs (selects
    // resolved against the configured platform), resolved
    // toolchains (rule's declared types matched against
    // `register_toolchains` registrations), and the rule's
    // defining `.bzl` (for the FrozenRuleClass and workspace
    // name). DICE drives parallel/dedup'd execution.
    let m_attrs = force_materialised_attrs(ctx, target_label, config).await?;
    let resolved_deps = resolve_label_deps(ctx, &m_attrs.0, config).await?;
    let toolchains = force_resolved_toolchains(ctx, target_label, config).await?;
    let bzl = force_bzl_load(ctx, bzl_label).await?;
    let graph = force_module_graph(ctx).await?;

    let target = pkg
        .targets
        .get(target_label.name())
        .expect("present above; PackageKey is purely functional");
    analysis::run_analysis(
        target,
        &m_attrs.0,
        Arc::clone(&bzl.module),
        &source_symbol,
        &graph.main.name,
        config,
        &resolved_deps,
        (*toolchains).clone(),
    )
    .map_err(|e| format!("{e:#}"))
}

/// `MaterialisedAttrsKey` compute — pulls the package + module
/// graph + per-config platform constraints, then resolves every
/// `select()` in the target's attrs to a concrete branch. Caller
/// (typically `AnalysisKey`) only sees a `BTreeMap<String,
/// AttrValue>` with no `Selector` variants left.
async fn compute_materialised_attrs(
    ctx: &mut DiceComputations<'_>,
    target_label: &Label,
    config: &Configuration,
) -> Result<MaterialisedAttrs, String> {
    let pkg = force_package(ctx, &target_label.package).await?;
    let Some(target) = pkg.targets.get(target_label.name()) else {
        // No target = source file path; materialised attrs are
        // empty (callers short-circuit before reaching here).
        return Ok(MaterialisedAttrs(BTreeMap::new()));
    };
    let graph = force_module_graph(ctx).await?;
    let plat_constraints = force_target_platform_constraints(ctx, config).await?;

    let target_attrs = target.attrs.clone();
    let select_matches = compute_select_matches(
        ctx,
        &target_attrs,
        &target_label.package,
        &graph.repo_mapping,
        config,
        plat_constraints.0.as_ref(),
    )
    .await?;
    let materialised = materialise_attrs(
        &target_attrs,
        &target_label.package,
        &graph.repo_mapping,
        &select_matches,
    )
    .map_err(|e| format!("{e:#}"))?;
    Ok(MaterialisedAttrs(materialised))
}

/// `ResolvedToolchainsKey` compute — reads the rule's declared
/// `toolchains=[...]` from its frozen `.bzl`, then runs the
/// registered-toolchains scan against the per-config platform
/// constraints. Empty `Toolchains` for source-file / native targets.
async fn compute_resolved_toolchains(
    ctx: &mut DiceComputations<'_>,
    target_label: &Label,
    config: &Configuration,
) -> Result<crate::starlark_types::toolchains::Toolchains, String> {
    use crate::starlark_types::toolchains::Toolchains;

    let pkg = force_package(ctx, &target_label.package).await?;
    let graph = force_module_graph(ctx).await?;
    let Some(target) = pkg.targets.get(target_label.name()) else {
        return Ok(Toolchains {
            by_type: BTreeMap::new(),
            repo_mapping: Arc::new(graph.repo_mapping.clone()),
        });
    };
    let (bzl_label, source_symbol) = match (&target.bzl_label, &target.source_symbol) {
        (Some(l), Some(s)) => (l.clone(), s.clone()),
        _ => {
            return Ok(Toolchains {
                by_type: BTreeMap::new(),
                repo_mapping: Arc::new(graph.repo_mapping.clone()),
            });
        }
    };
    drop(pkg);

    let bzl = force_bzl_load(ctx, bzl_label.clone()).await?;
    let declared = read_rule_toolchain_types(&bzl.module, &source_symbol);
    let graph = force_module_graph(ctx).await?;
    let plat_constraints = force_target_platform_constraints(ctx, config).await?;
    resolve_declared_toolchains(
        ctx,
        &declared,
        &bzl_label.package,
        config,
        &graph,
        plat_constraints.0.as_ref(),
    )
    .await
}

/// Walk materialised attrs, force `AnalysisKey` for each label-typed
/// dep, and stitch the results into a `BTreeMap` keyed by attr name.
async fn resolve_label_deps(
    ctx: &mut DiceComputations<'_>,
    materialised_attrs: &BTreeMap<String, crate::package::AttrValue>,
    config: &Configuration,
) -> Result<BTreeMap<String, ResolvedDep>, String> {
    let mut resolved_deps: BTreeMap<String, ResolvedDep> = BTreeMap::new();
    for (attr_name, kind) in collect_dep_labels(materialised_attrs) {
        match kind {
            DepLabels::Single(label) => {
                let dep = force_analysis(ctx, label, config).await?;
                resolved_deps.insert(attr_name, ResolvedDep::Single(dep));
            }
            DepLabels::List(labels) => {
                let mut out = Vec::with_capacity(labels.len());
                for l in labels {
                    out.push(force_analysis(ctx, l, config).await?);
                }
                resolved_deps.insert(attr_name, ResolvedDep::List(out));
            }
        }
    }
    Ok(resolved_deps)
}

/// Look up the rule symbol on the frozen `.bzl` and return the
/// `toolchains=[...]` list it declared. Returns `[]` if anything
/// goes wrong — analysis can still succeed without resolved
/// toolchains (the rule impl just hits `WildcardStruct` on every
/// `ctx.toolchains[T]` access).
fn read_rule_toolchain_types(bzl: &FrozenModule, rule_symbol: &str) -> Vec<String> {
    use crate::starlark_types::rule::FrozenRuleClass;
    use starlark::values::ValueLike;
    // `get_any_visibility` so private (`_my_rule`) bindings resolve.
    // Common rules_js / aspect_bazel_lib pattern: a public macro
    // wraps a private `_copy_to_bin = rule(...)` and the analyzer
    // looks up the private name to read its `toolchains=[...]` list.
    let Ok((owned, _vis)) = bzl.get_any_visibility(rule_symbol) else {
        return Vec::new();
    };
    let Some(rc) = owned.value().downcast_ref::<FrozenRuleClass>() else {
        return Vec::new();
    };
    rc.toolchains.clone()
}

/// First-match toolchain resolver. For every type in `declared`, scan
/// `graph.registered_toolchains` (already pre-canonicalised across
/// the entire module graph by `module_graph::parse_with_lockfile_override`)
/// and pick the first registered `toolchain` target whose
/// `toolchain_type` attr matches. Force-analyze that toolchain's
/// `toolchain` (impl) label and store under the canonical type label.
///
/// STUBs (deferred):
/// - No `target_compatible_with` / `exec_compatible_with` matching —
///   first registered match wins.
/// - No `:all` pattern expansion in `register_toolchains`.
/// - No module-extension-registered toolchains.
/// - The `declared` list (rule's `toolchains=[...]`) is canonicalised
///   against the main module's mapping; rules referencing toolchain
///   types via apparent names not in the main mapping fall back to
///   `<apparent>+` (same caveat as everywhere else in v1).
async fn resolve_declared_toolchains(
    ctx: &mut DiceComputations<'_>,
    declared: &[String],
    rule_pkg: &PackageId,
    config: &Configuration,
    graph: &ModuleGraph,
    target_platform_constraints: Option<&std::collections::BTreeSet<Label>>,
) -> Result<crate::starlark_types::toolchains::Toolchains, String> {
    let _ = config;
    use crate::package::AttrValue;
    use crate::starlark_types::toolchains::Toolchains;

    // Effective mapping for the rule's defining `.bzl` repo. Rule
    // strings like `@aspect_bazel_lib//lib:tar_toolchain_type` are
    // written from rules_oci's perspective, not main's — so they
    // canonicalise via rules_oci's per-repo mapping (which has
    // `aspect_bazel_lib → aspect_bazel_lib~`), not main's (which
    // doesn't declare aspect_bazel_lib at all).
    let rule_mapping = graph.effective_repo_mapping(rule_pkg);
    let rule_mapping_arc = Arc::new(rule_mapping.clone());
    if declared.is_empty() || graph.registered_toolchains.is_empty() {
        return Ok(Toolchains {
            by_type: BTreeMap::new(),
            repo_mapping: rule_mapping_arc,
        });
    }

    // Wanted set: canonicalize each declared toolchain-type string.
    // Skip any that fail to parse — analysis still proceeds with a
    // partial map.
    let mut wanted: BTreeMap<Label, ()> = BTreeMap::new();
    for s in declared {
        if let Ok(l) = Label::resolve(s, rule_pkg, &rule_mapping) {
            wanted.insert(l, ());
        }
    }
    if wanted.is_empty() {
        return Ok(Toolchains {
            by_type: BTreeMap::new(),
            repo_mapping: rule_mapping_arc,
        });
    }

    // Walk registrations in declaration order across the module graph;
    // first compatible match per toolchain-type wins. Each registration
    // may be a single label (`@foo//pkg:tc_name`) or a `:all` pattern
    // that expands to every `toolchain(...)` target in its package.
    let mut by_type: BTreeMap<Label, Arc<AnalyzedTarget>> = BTreeMap::new();
    for reg_label in &graph.registered_toolchains {
        if by_type.len() == wanted.len() {
            break; // every declared type already resolved
        }
        let pkg = match Box::pin(ctx.compute(&PackageKey(reg_label.package.clone()))).await {
            Ok(SharedResult::Ok(p)) => p,
            _ => continue,
        };
        // Effective mapping for the registration's repo (e.g.
        // `aspect_bazel_lib~~toolchains~bsd_tar_toolchains`). The
        // toolchain target's `toolchain_type` attr was written from
        // that repo's view; canonicalise accordingly so it matches
        // the wanted set computed above.
        let reg_mapping = graph.effective_repo_mapping(&reg_label.package);
        // `:all` expansion. Bazel's `register_toolchains("@foo//pkg:all")`
        // means "every toolchain target in this package." Resolve here
        // by enumerating `kind == "toolchain"` targets; each becomes a
        // candidate that flows through the same match logic below.
        let candidate_names: Vec<String> = if reg_label.name() == "all" {
            pkg.targets
                .iter()
                .filter(|(_, t)| t.kind == "toolchain")
                .map(|(n, _)| n.clone())
                .collect()
        } else {
            vec![reg_label.name().to_string()]
        };
        for cand_name in candidate_names {
            let Some(target) = pkg.targets.get(&cand_name) else {
                continue;
            };
            if target.kind != "toolchain" {
                continue;
            }
            let tt_str = match target.attrs.get("toolchain_type") {
                Some(AttrValue::String(s)) => s,
                _ => continue,
            };
            let tt_label = match Label::resolve(tt_str, &reg_label.package, &reg_mapping) {
                Ok(l) => l,
                Err(_) => continue,
            };
            if !wanted.contains_key(&tt_label) || by_type.contains_key(&tt_label) {
                continue;
            }
            // Constraint compatibility check — only when a target
            // platform is configured. Toolchains list `constraint_values`
            // their *target* (and exec) must satisfy; the platform's
            // `constraint_values` must be a superset.
            //
            // STUBs:
            // - `exec_compatible_with` is checked against the same
            //   target-platform set: v1 collapses exec platform onto
            //   target since we don't model transitions. Right for
            //   host builds, wrong for cross-builds.
            // - `constraint_setting.default_constraint_value` isn't
            //   honoured — a constraint missing from the platform
            //   never matches.
            if let Some(plat_constraints) = target_platform_constraints {
                let target_ok = candidate_satisfies_constraints(
                    target.attrs.get("target_compatible_with"),
                    &reg_label.package,
                    &reg_mapping,
                    plat_constraints,
                );
                let exec_ok = candidate_satisfies_constraints(
                    target.attrs.get("exec_compatible_with"),
                    &reg_label.package,
                    &reg_mapping,
                    plat_constraints,
                );
                if !target_ok || !exec_ok {
                    continue;
                }
            }
            let impl_str = match target.attrs.get("toolchain") {
                Some(AttrValue::String(s)) => s,
                _ => continue,
            };
            let impl_label = match Label::resolve(impl_str, &reg_label.package, &reg_mapping) {
                Ok(l) => l,
                Err(_) => continue,
            };
            // Toolchain candidates whose impl repo isn't on-disk
            // (e.g. a darwin variant when running on linux) silently
            // skip. Real Bazel filters these out via
            // `exec_compatible_with` before analysis; without a
            // configured platform we can't replicate that, so we
            // tolerate load failures and let the next compatible
            // candidate match.
            let analyzed = match force_analysis(ctx, impl_label, config).await {
                Ok(t) => t,
                Err(_) => continue,
            };
            by_type.insert(tt_label, analyzed);
        }
        // `pkg` is released at the end of the loop body — `force_analysis`
        // above runs on each candidate before the borrow is dropped, but
        // it doesn't need `pkg` itself, so this is fine.
    }

    Ok(Toolchains {
        by_type,
        repo_mapping: rule_mapping_arc,
    })
}

/// Check a toolchain candidate's `target_compatible_with` /
/// `exec_compatible_with` against the configured platform's
/// constraint set. `attr` is the captured `AttrValue::List` (or
/// missing — both treated as universally compatible). Each
/// constraint in the list must canonicalise to a Label that's in
/// `plat_constraints`.
fn candidate_satisfies_constraints(
    attr: Option<&crate::package::AttrValue>,
    reg_pkg: &PackageId,
    repo_mapping: &BTreeMap<String, crate::label::CanonicalRepo>,
    plat_constraints: &std::collections::BTreeSet<Label>,
) -> bool {
    use crate::package::AttrValue;
    let xs = match attr {
        Some(AttrValue::List(xs)) => xs,
        // Missing / non-list attr = no constraints declared = universally compatible.
        _ => return true,
    };
    xs.iter().all(|v| {
        let s = match v {
            AttrValue::String(s) => s,
            _ => return false,
        };
        match Label::resolve(s, reg_pkg, repo_mapping) {
            Ok(l) => plat_constraints.contains(&l),
            Err(_) => false,
        }
    })
}

/// Walk every `select()` branch key in `target_attrs`, classify
/// each against the configured platform's constraint set, and
/// return the canonical Labels of keys that match. The
/// synchronous `analysis::resolve_selector` reads from this set
/// when picking a branch.
///
/// Match rules:
/// - `//conditions:default` is never in the returned set (handled
///   as fallback by `resolve_selector`).
/// - `kind == "constraint_value"` matches iff the canonical Label
///   is in `target_platform_constraints`.
/// - `kind == "constraint_value"` matches iff the canonical Label is
///   in `target_platform_constraints`.
/// - `kind == "_config_setting_rule"` (the underlying rule of the
///   user-space `config_setting` macro) matches iff its
///   `ConfigSettingInfo` provider's `constraints` list is a subset of
///   `target_platform_constraints` AND every entry in `flag_values`
///   holds (each flag target's `BuildSettingInfo.value` equals the
///   requested value).
/// - Other kinds (or analysis failures) silently fail to match.
async fn compute_select_matches(
    ctx: &mut DiceComputations<'_>,
    target_attrs: &BTreeMap<String, crate::package::AttrValue>,
    target_pkg: &PackageId,
    repo_mapping: &BTreeMap<String, crate::label::CanonicalRepo>,
    config: &Configuration,
    target_platform_constraints: Option<&std::collections::BTreeSet<Label>>,
) -> Result<analysis::SelectMatches, String> {
    let mut out: analysis::SelectMatches = std::collections::BTreeMap::new();
    let mut keys: Vec<String> = Vec::new();
    for v in target_attrs.values() {
        walk_attr_select_keys(v, &mut keys);
    }
    keys.sort();
    keys.dedup();

    for key in keys {
        if key == "//conditions:default" {
            continue;
        }
        let key_label = match Label::resolve(&key, target_pkg, repo_mapping) {
            Ok(l) => l,
            Err(_) => continue,
        };
        if out.contains_key(&key_label) {
            continue;
        }
        let pkg = match Box::pin(ctx.compute(&PackageKey(key_label.package.clone()))).await {
            Ok(SharedResult::Ok(p)) => p,
            _ => continue,
        };
        let Some(target) = pkg.targets.get(key_label.name()) else {
            continue;
        };
        let kind = target.kind.clone();
        drop(pkg);
        match kind.as_str() {
            "constraint_value" => {
                if let Some(plat) = target_platform_constraints
                    && plat.contains(&key_label)
                {
                    // A constraint_value branch's specificity is 1 —
                    // exactly one constraint had to hold for it to
                    // match.
                    out.insert(key_label, 1);
                }
            }
            // `config_setting` is the user-space rule from
            // `builtins_core/exports.bzl`; `_config_setting_rule` was
            // the legacy macro-wrapping name. Both record a target
            // whose `ConfigSettingInfo` carries the matcher.
            "_config_setting_rule" | "config_setting" => {
                let analyzed = match force_analysis(ctx, key_label.clone(), config).await {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let Some(cs_info) = analyzed.provider_by_symbol("ConfigSettingInfo") else {
                    continue;
                };
                let (constraints, flag_values_dict) = match read_config_setting_info(cs_info) {
                    Some(p) => p,
                    None => continue,
                };
                if constraints.is_empty() && flag_values_dict.is_empty() {
                    continue;
                }
                // Constraints must be a subset of the platform set.
                let cv_ok = constraints.iter().all(|s| {
                    match Label::resolve(s, &key_label.package, repo_mapping) {
                        Ok(l) => target_platform_constraints
                            .map(|p| p.contains(&l))
                            .unwrap_or(false),
                        Err(_) => false,
                    }
                });
                if !cv_ok {
                    continue;
                }
                // For each flag, force-analyze the target and read
                // `BuildSettingInfo.value`.
                let mut flag_ok = true;
                for (flag_str, want) in &flag_values_dict {
                    let flag_label =
                        match Label::resolve(flag_str, &key_label.package, repo_mapping) {
                            Ok(l) => l,
                            Err(_) => {
                                flag_ok = false;
                                break;
                            }
                        };
                    let flag_target = match force_analysis(ctx, flag_label, config).await {
                        Ok(t) => t,
                        Err(_) => {
                            flag_ok = false;
                            break;
                        }
                    };
                    let bsi = match flag_target.provider_by_symbol("BuildSettingInfo") {
                        Some(p) => p,
                        None => {
                            flag_ok = false;
                            break;
                        }
                    };
                    let actual = match bsi.fields.get("value") {
                        Some(v) => v.to_value().to_string().trim_matches('"').to_string(),
                        None => String::new(),
                    };
                    if !flag_value_matches(&actual, want) {
                        flag_ok = false;
                        break;
                    }
                }
                if flag_ok {
                    let specificity = constraints.len() + flag_values_dict.len();
                    out.insert(key_label, specificity as u32);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Read `(constraints, flag_values)` off a `ConfigSettingInfo`
/// provider. Returns `None` if either field is malformed (e.g. a
/// rule_kind mismatch left the fields empty).
fn read_config_setting_info(
    cs_info: &analysis::CapturedProvider,
) -> Option<(Vec<String>, BTreeMap<String, String>)> {
    let mut constraints: Vec<String> = Vec::new();
    if let Some(v) = cs_info.fields.get("constraints")
        && let Some(list) = starlark::values::list::ListRef::from_value(v.to_value())
    {
        for item in list.iter() {
            if let Some(s) = item.unpack_str() {
                constraints.push(s.to_string());
            }
        }
    }
    let mut flag_values: BTreeMap<String, String> = BTreeMap::new();
    if let Some(v) = cs_info.fields.get("flag_values")
        && let Some(dict) = starlark::values::dict::DictRef::from_value(v.to_value())
    {
        for (k, val) in dict.iter() {
            // Keys may be Label values (label_keyed_string_dict), bare
            // strings, or already-canonical Label instances. Stringify
            // to the canonical "@@…//pkg:name" form via Display so
            // downstream label resolution sees the same shape.
            let key = k
                .unpack_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| k.to_string());
            if let Some(s) = val.unpack_str() {
                flag_values.insert(key, s.to_string());
            }
        }
    }
    Some((constraints, flag_values))
}

/// Compare an actual build-setting value (string) to a requested
/// `flag_values` entry. Numbers are compared verbatim; bools accept
/// Python (`True`/`False`), Rust (`true`/`false`), and `1`/`0`
/// spellings to mirror Bazel's loose `flag_values` matching.
fn flag_value_matches(actual: &str, want: &str) -> bool {
    if actual == want {
        return true;
    }
    matches!(
        (actual, want),
        ("True" | "true" | "1", "True" | "true" | "1")
            | ("False" | "false" | "0", "False" | "false" | "0")
    )
}

fn walk_attr_select_keys(v: &crate::package::AttrValue, out: &mut Vec<String>) {
    use crate::package::AttrValue;
    use crate::starlark_types::selector::SelectorPiece;
    match v {
        AttrValue::Selector(sl) => {
            for piece in &sl.pieces {
                if let SelectorPiece::Selector(sel) = piece {
                    for key in sel.branches.keys() {
                        out.push(key.clone());
                    }
                }
            }
        }
        AttrValue::List(xs) => xs.iter().for_each(|x| walk_attr_select_keys(x, out)),
        AttrValue::Dict(m) => m.values().for_each(|x| walk_attr_select_keys(x, out)),
        _ => {}
    }
}

/// Resolve every `Selector` in `raw` against `select_matches`,
/// returning a parallel attrs map of concrete `AttrValue`s. List /
/// Dict containers are walked recursively. Non-selector entries
/// pass through cloned.
fn materialise_attrs(
    raw: &BTreeMap<String, crate::package::AttrValue>,
    current_pkg: &PackageId,
    repo_mapping: &BTreeMap<String, crate::label::CanonicalRepo>,
    select_matches: &analysis::SelectMatches,
) -> anyhow::Result<BTreeMap<String, crate::package::AttrValue>> {
    let mut out = BTreeMap::new();
    for (k, v) in raw {
        out.insert(
            k.clone(),
            materialise_one(v, current_pkg, repo_mapping, select_matches)?,
        );
    }
    Ok(out)
}

fn materialise_one(
    v: &crate::package::AttrValue,
    current_pkg: &PackageId,
    repo_mapping: &BTreeMap<String, crate::label::CanonicalRepo>,
    select_matches: &analysis::SelectMatches,
) -> anyhow::Result<crate::package::AttrValue> {
    use crate::package::AttrValue;
    match v {
        AttrValue::Selector(sl) => {
            analysis::resolve_selector(sl, current_pkg, repo_mapping, select_matches)
        }
        AttrValue::List(xs) => Ok(AttrValue::List(
            xs.iter()
                .map(|x| materialise_one(x, current_pkg, repo_mapping, select_matches))
                .collect::<anyhow::Result<Vec<_>>>()?,
        )),
        AttrValue::Dict(m) => Ok(AttrValue::Dict(
            m.iter()
                .map(|(k, x)| {
                    materialise_one(x, current_pkg, repo_mapping, select_matches)
                        .map(|y| (k.clone(), y))
                })
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?,
        )),
        other => Ok(other.clone()),
    }
}

/// Read `<platform_label>`'s `constraint_values` attr and
/// canonicalise each entry. Used by the toolchain resolver to filter
/// `target_compatible_with` candidates. Returns `None` if the
/// platform target can't be loaded; the caller treats that the same
/// as no-platform-set (skip the constraint check).
/// Read `<platform_label>`'s effective `constraint_values` set,
/// including those inherited from `parents=[...]`. Walks the parent
/// chain depth-first; each parent's direct constraint_values get
/// added (deepest-first, so later overrides win in the union view —
/// good enough for v1 since we treat the result as a set anyway and
/// don't model per-setting overriding).
///
/// STUBs:
/// - `constraint_setting.default_constraint_value` isn't honoured —
///   when a toolchain demands constraint_value `V` whose setting the
///   platform doesn't mention, the match fails even if `V` is the
///   setting's default. Real Bazel matches via the default.
/// - Cycles in `parents=[...]` are detected and ignored (real
///   Bazel rejects cyclic platform inheritance; v1 silently
///   short-circuits).
async fn load_target_platform_constraints(
    ctx: &mut DiceComputations<'_>,
    platform_label: &Label,
    repo_mapping: &BTreeMap<String, crate::label::CanonicalRepo>,
) -> Option<std::collections::BTreeSet<Label>> {
    let mut out = std::collections::BTreeSet::new();
    let mut visited: std::collections::BTreeSet<Label> = std::collections::BTreeSet::new();
    let mut stack: Vec<Label> = vec![platform_label.clone()];
    let mut found_any = false;
    while let Some(cur) = stack.pop() {
        if !visited.insert(cur.clone()) {
            continue;
        }
        let Some((cvs, parents)) = load_one_platform(ctx, &cur, repo_mapping).await else {
            continue;
        };
        found_any = true;
        out.extend(cvs);
        // Push parents onto the stack — deepest first per
        // declaration order, so the union ends up declaration-order
        // friendly. Parent loads happen lazily on next iteration.
        stack.extend(parents);
    }
    if !found_any {
        // Couldn't even load the requested platform — signal "no
        // platform configured" semantics so the caller skips the
        // constraint check entirely.
        return None;
    }
    Some(out)
}

/// Load one `platform(...)` target, returning its direct
/// `constraint_values` (canonicalised) and parent labels for the
/// caller's traversal.
async fn load_one_platform(
    ctx: &mut DiceComputations<'_>,
    platform_label: &Label,
    repo_mapping: &BTreeMap<String, crate::label::CanonicalRepo>,
) -> Option<(std::collections::BTreeSet<Label>, Vec<Label>)> {
    use crate::package::AttrValue;
    let pkg = match Box::pin(ctx.compute(&PackageKey(platform_label.package.clone()))).await {
        Ok(SharedResult::Ok(p)) => p,
        _ => return None,
    };
    let target = pkg.targets.get(platform_label.name())?;
    if target.kind != "platform" {
        return None;
    }
    let mut cvs = std::collections::BTreeSet::new();
    if let Some(AttrValue::List(xs)) = target.attrs.get("constraint_values") {
        for v in xs {
            if let AttrValue::String(s) = v
                && let Ok(l) = Label::resolve(s, &platform_label.package, repo_mapping)
            {
                cvs.insert(l);
            }
        }
    }
    let mut parents = Vec::new();
    if let Some(AttrValue::List(xs)) = target.attrs.get("parents") {
        for v in xs {
            if let AttrValue::String(s) = v
                && let Ok(l) = Label::resolve(s, &platform_label.package, repo_mapping)
            {
                parents.push(l);
            }
        }
    }
    Some((cvs, parents))
}

/// Per-attr label dependency shape. Distinct from `ResolvedDep` —
/// this is the unresolved input; `ResolvedDep` is what
/// `AnalysisKey` produces for each.
enum DepLabels {
    Single(Label),
    List(Vec<Label>),
}

/// `alias(name, actual=...)` — forwards every provider from `actual`.
/// The aliased target's `AnalyzedTarget` is the source of truth;
/// we just relabel it to the alias's own `Label` and keep its
/// `analysis_heap` so the FrozenValues in `providers[].fields` stay
/// anchored. Mirrors Bazel's behavior: `bazel cquery //:alias --output=…`
/// reports the same providers as the aliased target.
async fn synthesise_alias_analysis(
    ctx: &mut DiceComputations<'_>,
    target: &crate::package::UnconfiguredTarget,
    config: &Configuration,
) -> Result<AnalyzedTarget, String> {
    use crate::package::AttrValue;
    let actual = match target.attrs.get("actual") {
        Some(AttrValue::Label(l)) => l.clone(),
        _ => {
            // Malformed alias (no `actual=` or non-label string) —
            // fall back to the synthetic-source-file path so the
            // graph still walks.
            return Ok(synthesise_source_file_analysis(target, config));
        }
    };
    let analyzed = force_analysis(ctx, actual, config).await?;
    Ok(AnalyzedTarget {
        label: target.label.clone(),
        configuration: config.clone(),
        rule_kind: target.kind.clone(),
        providers: analyzed.providers.clone(),
        actions: analyzed.actions.clone(),
        analysis_heap: Arc::clone(&analyzed.analysis_heap),
    })
}

fn synthesise_source_file_analysis(
    target: &crate::package::UnconfiguredTarget,
    config: &Configuration,
) -> AnalyzedTarget {
    use starlark::environment::Module;
    use std::sync::Arc;

    let synthetic_file = crate::starlark_types::file::File::source(
        target.label.package.clone(),
        target.label.name.clone(),
    );

    // Allocate the File on a fresh heap so DefaultInfo.files can hold
    // it as a FrozenValue. Module::set + freeze gives us a stable
    // FrozenValue handle we can poke into the synthetic provider.
    let mut fields: std::collections::BTreeMap<String, starlark::values::FrozenValue> =
        std::collections::BTreeMap::new();
    let frozen_module: Arc<starlark::environment::FrozenModule> =
        Module::with_temp_heap(|module| {
            // Match the vendored `DefaultInfo` shape exactly so
            // `target[DefaultInfo].<field>` reads from downstream
            // rules don't error on a missing attribute. Upstream's
            // `_default_info_init` produces `files` + `files_to_run`,
            // and its `computed_fields` add `data_runfiles` /
            // `default_runfiles`. Source-file targets never run that
            // init/computed pass (we synthesize the providers
            // directly), so populate the same field set here with
            // appropriate empty defaults.
            use crate::starlark_types::depset::{Depset, DepsetOrder};
            use crate::starlark_types::runfiles::Runfiles;
            use starlark::values::structs::AllocStruct;

            let file_value = module.heap().alloc(synthetic_file.clone());
            let files_depset = module.heap().alloc_complex(Depset {
                direct: vec![file_value],
                transitive: Vec::new(),
                order: DepsetOrder::Default,
            });
            let empty_depset = module.heap().alloc_complex(Depset {
                direct: Vec::new(),
                transitive: Vec::new(),
                order: DepsetOrder::Default,
            });
            // `files_to_run` — bonanza's `FilesToRunProvider` shape.
            // No `executable` on a plain source file; `_runfiles_*`
            // mirror the empty runfiles state.
            let none_v = starlark::values::Value::new_none();
            let files_to_run_pairs: [(&str, starlark::values::Value<'_>); 6] = [
                ("executable", none_v),
                ("runfiles_manifest", none_v),
                ("repo_mapping_manifest", none_v),
                ("_runfiles_files", empty_depset),
                ("_runfiles_symlinks", empty_depset),
                ("_runfiles_root_symlinks", empty_depset),
            ];
            let files_to_run = module
                .heap()
                .alloc(AllocStruct(files_to_run_pairs.into_iter()));
            // `data_runfiles` / `default_runfiles` — empty Runfiles
            // value. Downstream `default_runfiles.files` (rules_oci's
            // `tar`) sees an empty depset.
            let empty_runfiles = module.heap().alloc(Runfiles::empty());

            module.set("__files__", files_depset);
            module.set("__files_to_run__", files_to_run);
            module.set("__runfiles__", empty_runfiles);
            let frozen = module
                .freeze()
                .expect("freezing a single-File module never fails");
            // SAFETY: the Arc<FrozenModule> we return owns the
            // FrozenHeap these values came from, and we keep the Arc
            // alive on the AnalyzedTarget. Callers that read these
            // fields must do so through the Arc<FrozenModule> kept on
            // `analysis_heap`.
            for (out_name, mod_name) in [
                ("files", "__files__"),
                ("files_to_run", "__files_to_run__"),
                ("default_runfiles", "__runfiles__"),
                ("data_runfiles", "__runfiles__"),
            ] {
                if let Ok(Some(owned)) = frozen.get_option(mod_name) {
                    let frozen_value = unsafe { owned.unchecked_frozen_value() };
                    fields.insert(out_name.to_string(), frozen_value);
                }
            }
            Arc::new(frozen)
        });

    AnalyzedTarget {
        label: target.label.clone(),
        configuration: config.clone(),
        rule_kind: target.kind.clone(),
        providers: vec![analysis::CapturedProvider {
            id: crate::starlark_types::provider::ProviderId {
                defining_bzl: None,
                symbol: "DefaultInfo".to_string(),
            },
            fields,
            type_ptr: None,
        }],
        actions: Vec::new(),
        analysis_heap: frozen_module,
    }
}

/// `filegroup(name, srcs=[...])` — unions every `srcs` entry as a
/// File in DefaultInfo.files. Loupe v1 doesn't run the vendored
/// user-space impl (`computed_fields` on the `runfiles` provider
/// aren't implemented), so the analysis driver synthesises the
/// `srcs`-unioned DefaultInfo directly. Bare file names resolve as
/// source files in the same package; entries with `:` or `//` are
/// skipped (proper label resolution would need a deps-walk like
/// `synthesise_alias_analysis`).
fn synthesise_filegroup_analysis(
    target: &crate::package::UnconfiguredTarget,
    config: &Configuration,
) -> AnalyzedTarget {
    use starlark::environment::Module;
    use std::sync::Arc;

    let srcs: Vec<String> = match target.attrs.get("srcs") {
        Some(crate::package::AttrValue::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                crate::package::AttrValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };

    let mut fields: std::collections::BTreeMap<String, starlark::values::FrozenValue> =
        std::collections::BTreeMap::new();
    let frozen_module: Arc<starlark::environment::FrozenModule> =
        Module::with_temp_heap(|module| {
            use crate::starlark_types::depset::{Depset, DepsetOrder};
            let direct: Vec<_> = srcs
                .iter()
                .filter(|s| !s.contains(':') && !s.starts_with("//") && !s.starts_with('@'))
                .map(|s| {
                    module
                        .heap()
                        .alloc(crate::starlark_types::file::File::source(
                            target.label.package.clone(),
                            s.clone(),
                        ))
                })
                .collect();
            let list = module.heap().alloc_complex(Depset {
                direct,
                transitive: Vec::new(),
                order: DepsetOrder::Default,
            });
            module.set("__filegroup_files__", list);
            let frozen = module
                .freeze()
                .expect("freezing a filegroup module never fails");
            if let Ok(Some(owned)) = frozen.get_option("__filegroup_files__") {
                let frozen_value = unsafe { owned.unchecked_frozen_value() };
                fields.insert("files".to_string(), frozen_value);
            }
            Arc::new(frozen)
        });

    AnalyzedTarget {
        label: target.label.clone(),
        configuration: config.clone(),
        rule_kind: target.kind.clone(),
        providers: vec![analysis::CapturedProvider {
            id: crate::starlark_types::provider::ProviderId {
                defining_bzl: None,
                symbol: "DefaultInfo".to_string(),
            },
            fields,
            type_ptr: None,
        }],
        actions: Vec::new(),
        analysis_heap: frozen_module,
    }
}

fn collect_dep_labels(
    attrs: &BTreeMap<String, crate::package::AttrValue>,
) -> Vec<(String, DepLabels)> {
    let mut out = Vec::new();
    for (attr_name, val) in attrs {
        match val {
            crate::package::AttrValue::Label(l) => {
                out.push((attr_name.clone(), DepLabels::Single(l.clone())));
            }
            crate::package::AttrValue::LabelList(ls) => {
                out.push((attr_name.clone(), DepLabels::List(ls.clone())));
            }
            _ => {}
        }
    }
    out
}

/// What `analysis::run_analysis` consumes per label-typed attr.
pub use crate::analysis::ResolvedDep;

async fn force_analysis(
    ctx: &mut DiceComputations<'_>,
    label: Label,
    config: &Configuration,
) -> Result<Arc<AnalyzedTarget>, String> {
    let key = AnalysisKey(label.clone(), config.clone());
    match Box::pin(ctx.compute(&key)).await {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("AnalysisKey({label}): {e}")),
    }
}

async fn force_package(
    ctx: &mut DiceComputations<'_>,
    pkg_id: &PackageId,
) -> Result<Arc<Package>, String> {
    match ctx.compute(&PackageKey(pkg_id.clone())).await {
        Ok(SharedResult::Ok(p)) => Ok(p),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("PackageKey({pkg_id}): {e}")),
    }
}

async fn force_target_platform_constraints(
    ctx: &mut DiceComputations<'_>,
    config: &Configuration,
) -> Result<Arc<TargetPlatformConstraints>, String> {
    match ctx
        .compute(&TargetPlatformConstraintsKey(config.clone()))
        .await
    {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("TargetPlatformConstraintsKey: {e}")),
    }
}

async fn force_materialised_attrs(
    ctx: &mut DiceComputations<'_>,
    label: &Label,
    config: &Configuration,
) -> Result<Arc<MaterialisedAttrs>, String> {
    let key = MaterialisedAttrsKey(label.clone(), config.clone());
    match Box::pin(ctx.compute(&key)).await {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("MaterialisedAttrsKey({label}): {e}")),
    }
}

async fn force_resolved_toolchains(
    ctx: &mut DiceComputations<'_>,
    label: &Label,
    config: &Configuration,
) -> Result<Arc<crate::starlark_types::toolchains::Toolchains>, String> {
    let key = ResolvedToolchainsKey(label.clone(), config.clone());
    match Box::pin(ctx.compute(&key)).await {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("ResolvedToolchainsKey({label}): {e}")),
    }
}

async fn force_module_graph(ctx: &mut DiceComputations<'_>) -> Result<Arc<ModuleGraph>, String> {
    match ctx.compute(&ModuleGraphKey).await {
        Ok(SharedResult::Ok(g)) => Ok(g),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("ModuleGraphKey: {e}")),
    }
}

async fn force_bzl_load(
    ctx: &mut DiceComputations<'_>,
    label: Label,
) -> Result<Arc<LoadedBzl>, String> {
    match ctx.compute(&BzlLoadKey(label)).await {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("BzlLoadKey: {e}")),
    }
}

async fn compute_bzl_load(
    ctx: &mut DiceComputations<'_>,
    label: &Label,
    graph: &ModuleGraph,
) -> Result<LoadedBzl, String> {
    // `@_builtins//<path>` short-circuits to the embedded tree —
    // user-side `load("@_builtins//common/coverage.bzl", ...)` etc.
    // resolves to whatever was eval'd in
    // `crate::builtins::evaluate_embedded_tree`.
    if label.package.repo.as_str() == "_builtins" {
        // Bazel `load(...)` accepts both colon and no-colon forms:
        //   load("@_builtins//skylib:paths.bzl", "paths")  # pkg=skylib name=paths.bzl
        //   load("@_builtins//skylib/paths.bzl", "paths")  # pkg=skylib/paths.bzl name=paths.bzl
        // Try both — prefer the colon-form composition first since
        // it's the canonical Bazel idiom.
        let primary = if label.package.path.is_root() {
            label.name.to_string()
        } else {
            format!("{}/{}", label.package.path.as_str(), label.name)
        };
        let fallback = label.package.path.as_str().to_string();
        for candidate in [primary.as_str(), fallback.as_str()] {
            if candidate.is_empty() {
                continue;
            }
            if let Ok(Some(frozen)) = crate::builtins::embedded_module_by_path(candidate) {
                return Ok(LoadedBzl {
                    module: Arc::new(frozen),
                });
            }
        }
        return Err(format!(
            "no embedded builtin at `@_builtins//{}` (tried `{}` and `{}`) — registered \
             files are listed in `crate::builtins::EMBEDDED_FILES`",
            primary, primary, fallback
        ));
    }
    let root = ctx.global_data().workspace_root().to_path_buf();
    let external = ctx
        .global_data()
        .external_repo_root()
        .map(|p| p.to_path_buf());
    let bzl_path = bzl_disk_path(&root, external.as_deref(), label)?;
    let ast = bzl_eval::parse(&bzl_path).map_err(|e| format!("{e:#}"))?;
    let load_stmts = build_eval::extract_loads(&ast);
    let mut loaded: std::collections::HashMap<String, FrozenModule> =
        std::collections::HashMap::new();
    for stmt in &load_stmts {
        let target =
            resolve_load(&stmt.module_id, &label.package, graph).map_err(|e| format!("{e:#}"))?;
        let lb = Box::pin(force_bzl_load(ctx, target)).await?;
        loaded.insert(stmt.module_id.clone(), (*lb.module).clone());
    }
    // Stamp the .bzl's effective repo mapping onto every RuleClass it
    // exports so external `attr.label` defaults resolve through the
    // .bzl's perspective at rule-invoke time, not the consumer BUILD's.
    let mut defining_mapping = graph.repo_mapping.clone();
    if let Some(per) = graph.per_repo_mapping.get(&label.package.repo) {
        for (k, v) in per {
            defining_mapping.insert(k.clone(), v.clone());
        }
    }
    let frozen =
        bzl_eval::evaluate(label, &defining_mapping, ast, &loaded).map_err(|e| format!("{e:#}"))?;
    Ok(LoadedBzl {
        module: Arc::new(frozen),
    })
}

async fn compute_package(
    ctx: &mut DiceComputations<'_>,
    pkg: &PackageId,
    graph: &ModuleGraph,
) -> Result<Package, String> {
    // `@bazel_tools//command_line_option:<flag>` — Bazel's virtual
    // flag namespace, mirroring bonanza's
    // [`starlark/bazel_tools/command_line_option`](https://github.com/buildbarn/bonanza/tree/main/starlark/bazel_tools/command_line_option)
    // package. config_setting's `_init` rewrites `values={k: v}` into
    // `flag_values={"@bazel_tools//command_line_option:k": v}`, and
    // transitions reference these labels in `inputs=` / `outputs=`.
    // No BUILD file on disk; we synthesize one target per default
    // build-setting key. Each target carries `BuildSettingInfo(value=…)`
    // when analyzed (see `synthesise_command_line_option_analysis`).
    if pkg.repo.as_str() == "bazel_tools" && pkg.path.as_str() == "command_line_option" {
        return Ok(synthesize_command_line_option_package(pkg));
    }
    let root = ctx.global_data().workspace_root().to_path_buf();
    let external = ctx
        .global_data()
        .external_repo_root()
        .map(|p| p.to_path_buf());
    let (_, ast) =
        build_eval::parse_build(&root, external.as_deref(), pkg).map_err(|e| format!("{e:#}"))?;
    let load_stmts = build_eval::extract_loads(&ast);
    let mut loaded: std::collections::HashMap<String, (Label, FrozenModule)> =
        std::collections::HashMap::new();
    for stmt in &load_stmts {
        let target = resolve_load(&stmt.module_id, pkg, graph).map_err(|e| format!("{e:#}"))?;
        let lb = force_bzl_load(ctx, target.clone()).await?;
        loaded.insert(stmt.module_id.clone(), (target, (*lb.module).clone()));
    }
    // Per-repo merged mapping so loads from inside an external BUILD
    // (rare today, but the path is symmetric with .bzl resolution).
    let mut effective_mapping = graph.repo_mapping.clone();
    if let Some(per) = graph.per_repo_mapping.get(&pkg.repo) {
        for (k, v) in per {
            effective_mapping.insert(k.clone(), v.clone());
        }
    }
    build_eval::evaluate(
        &root,
        external.as_deref(),
        pkg.clone(),
        effective_mapping,
        ast,
        &load_stmts,
        &loaded,
    )
    .map_err(|e| format!("{e:#}"))
}

fn bzl_disk_path(
    workspace_root: &Path,
    external_repo_root: Option<&Path>,
    label: &Label,
) -> Result<PathBuf, String> {
    if !label.name().ends_with(".bzl") {
        return Err(format!(
            "BzlLoadKey label must end in .bzl (got `{}`)",
            label
        ));
    }
    // Choose the repo's on-disk root: main is the workspace root;
    // external repos live under `<external_root>/<canonical>`.
    let repo_root: PathBuf = if label.repo().is_main() {
        workspace_root.to_path_buf()
    } else {
        let ext = external_repo_root.ok_or_else(|| {
            format!(
                "loading `{}`: external repo `{}` referenced but no external_repo_root \
                 is configured (vendor with `bazel mod vendor` or set via \
                 `EngineBuilder::external_repo_root`)",
                label,
                label.repo()
            )
        })?;
        ext.join(label.repo().as_str())
    };
    let mut path = repo_root;
    if !label.package_path().is_root() {
        path.push(label.package_path().as_str());
    }
    path.push(label.name());
    Ok(path)
}

fn resolve_load(
    load_str: &str,
    current_pkg: &PackageId,
    graph: &ModuleGraph,
) -> anyhow::Result<Label> {
    // Per-repo (lockfile-derived) entries win over the main module's
    // mapping for loads from inside an external repo. v1 builds the
    // per-repo map from `MODULE.bazel.lock`'s `moduleExtensions`
    // block — see `crate::lockfile`.
    let mut effective = graph.repo_mapping.clone();
    if let Some(per) = graph.per_repo_mapping.get(&current_pkg.repo) {
        for (k, v) in per {
            effective.insert(k.clone(), v.clone());
        }
    }
    Label::resolve(load_str, current_pkg, &effective)
        .map_err(|e| anyhow::anyhow!("resolving load `{load_str}` from {current_pkg}: {e}"))
}

/// Analyze a synthesized `@bazel_tools//command_line_option:<flag>`
/// target. Returns `[DefaultInfo, BuildSettingInfo(value=…)]` where
/// `value` is the configured value from `Configuration::build_settings`,
/// or the empty string if the flag isn't tracked. Mirrors what a
/// real `bool_flag` / `string_flag` impl would return — the select()
/// resolver reads `BuildSettingInfo.value` to compare against the
/// `flag_values` dict on a `ConfigSettingInfo`.
fn synthesise_command_line_option_analysis(
    target: &crate::package::UnconfiguredTarget,
    config: &Configuration,
) -> AnalyzedTarget {
    use crate::starlark_types::depset::{Depset, DepsetOrder};
    use starlark::environment::Module;
    use std::sync::Arc;

    let flag_name = target.label.name();
    let value = config
        .build_settings()
        .get(flag_name)
        .cloned()
        .unwrap_or_default();

    let mut default_fields: BTreeMap<String, starlark::values::FrozenValue> = BTreeMap::new();
    let mut bsi_fields: BTreeMap<String, starlark::values::FrozenValue> = BTreeMap::new();
    let frozen_module: Arc<starlark::environment::FrozenModule> =
        Module::with_temp_heap(|module| {
            // Empty depset for DefaultInfo.files — synthesized flags
            // don't ship file outputs.
            let empty_depset = module.heap().alloc_complex(Depset {
                direct: Vec::<starlark::values::Value<'_>>::new(),
                transitive: Vec::new(),
                order: DepsetOrder::Default,
            });
            module.set("__empty_depset__", empty_depset);
            // BuildSettingInfo.value — string. The configured-flag
            // value from Configuration. Bool flags would normally
            // surface as Python-style "True"/"False" strings; v1
            // stores everything as strings already.
            let value_str = module.heap().alloc(value.as_str());
            module.set("__value__", value_str);
            let frozen = module
                .freeze()
                .expect("freezing a single-string module never fails");
            if let Ok(Some(owned)) = frozen.get_option("__empty_depset__") {
                let fv = unsafe { owned.unchecked_frozen_value() };
                default_fields.insert("files".to_string(), fv);
            }
            if let Ok(Some(owned)) = frozen.get_option("__value__") {
                let fv = unsafe { owned.unchecked_frozen_value() };
                bsi_fields.insert("value".to_string(), fv);
            }
            Arc::new(frozen)
        });

    let exports_label = crate::label::Label::new(
        crate::label::CanonicalRepo::new("_builtins"),
        crate::label::PackagePath::new("builtins_core").unwrap(),
        "exports.bzl",
    );

    AnalyzedTarget {
        label: target.label.clone(),
        configuration: config.clone(),
        rule_kind: "_loupe_command_line_option".to_string(),
        providers: vec![
            analysis::CapturedProvider {
                id: crate::starlark_types::provider::ProviderId {
                    defining_bzl: None,
                    symbol: "DefaultInfo".to_string(),
                },
                fields: default_fields,
                type_ptr: None,
            },
            analysis::CapturedProvider {
                id: crate::starlark_types::provider::ProviderId {
                    defining_bzl: Some(exports_label),
                    symbol: "BuildSettingInfo".to_string(),
                },
                fields: bsi_fields,
                type_ptr: None,
            },
        ],
        actions: Vec::new(),
        analysis_heap: frozen_module,
    }
}

/// Build the synthetic `//command_line_option:*` package. One target
/// per default build-setting key; each target's `name` attr carries
/// the flag name. Targets land on the `synthesise_source_file_analysis`
/// path (kind isn't `"alias"` and there's no `bzl_label`), which gives
/// them a single-File `DefaultInfo` — enough shape for label resolution
/// and `force_analysis` to succeed when transitions reference these
/// labels in `inputs=` / `outputs=`.
fn synthesize_command_line_option_package(pkg: &PackageId) -> Package {
    let mut p = Package::new(pkg.clone());
    for key in crate::analysis::Configuration::default_v1()
        .build_settings()
        .keys()
    {
        let label = Label::new(pkg.repo.clone(), pkg.path.clone(), key);
        let mut attrs = BTreeMap::new();
        attrs.insert(
            "name".to_string(),
            crate::package::AttrValue::String(key.clone()),
        );
        p.targets.insert(
            key.clone(),
            crate::package::UnconfiguredTarget {
                label,
                kind: "_loupe_command_line_option".to_string(),
                bzl_source: "<command_line_option>".to_string(),
                bzl_label: None,
                source_symbol: None,
                attrs,
            },
        );
    }
    p
}
