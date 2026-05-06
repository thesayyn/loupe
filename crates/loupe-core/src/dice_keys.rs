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

/// Analyze a configured target. Forces the target's `PackageKey`
/// (yielding the `UnconfiguredTarget`), then `BzlLoadKey` for the
/// rule's defining `.bzl` (yielding the `FrozenRuleClass`), and
/// finally runs the synchronous analysis driver in `crate::analysis`.
///
/// Phase 5 stop gate: fixture #1 has no label-typed attrs, so the
/// "recursive analysis of label deps" deliverable in `PLAN.md` is
/// plumbed via this key (just call `ctx.compute(&AnalysisKey(...))`
/// from inside `compute_analysis`) but not yet exercised. The next
/// ruleset will surface it.
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
    // 1. Force PackageKey to find the UnconfiguredTarget. `pkg` is an
    //    `Arc<Package>`, so re-borrowing `target` from it across the
    //    later awaits is free — we just have to drop the borrow at
    //    each await point.
    let pkg = match ctx.compute(&PackageKey(target_label.package.clone())).await {
        Ok(SharedResult::Ok(p)) => p,
        Ok(SharedResult::Err(e)) => return Err(Arc::unwrap_or_clone(e)),
        Err(e) => return Err(format!("PackageKey({}): {e}", target_label.package)),
    };

    // 2. Pull out just the small fields we need to carry across the
    //    awaits below — `bzl_label`/`source_symbol` are cheap to
    //    clone, and the early-return path only needs label/kind which
    //    are cheap too. Crucially: no clone of `attrs`.
    let (bzl_label, source_symbol) = {
        let target = match pkg.targets.get(target_label.name()) {
            Some(t) => t,
            None => {
                // STUB: Bazel auto-discovers source files referenced
                // by label even without an `exports_files(...)`. v1
                // synthesises an UnconfiguredTarget on the fly so the
                // analysis chain can return a source-file
                // DefaultInfo. Doesn't check the file actually exists
                // on disk.
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
        };
        match (&target.bzl_label, &target.source_symbol) {
            (Some(l), Some(s)) => (l.clone(), s.clone()),
            // Source files (synthetic kind from `exports_files`) and
            // native rules (filegroup, alias, …) don't have a Starlark
            // impl to invoke. They round-trip through analysis as
            // synthetic-DefaultInfo targets — DefaultInfo.files
            // contains the source File so consumers calling
            // `ctx.file.<src>` / `ctx.files.<src>` actually receive
            // it (Bazel's behaviour for source-file deps).
            _ => {
                return Ok(synthesise_source_file_analysis(target, config));
            }
        }
    };

    // 3. Recursively force AnalysisKey for every label-typed attr.
    //    Collect the dep labels first (cheap clones from the package
    //    Arc), then drop the borrow before awaiting.
    let dep_labels = {
        let target = pkg
            .targets
            .get(target_label.name())
            .expect("present above; PackageKey is purely functional");
        collect_dep_labels(target)
    };
    let mut resolved_deps: BTreeMap<String, ResolvedDep> = BTreeMap::new();
    for (attr_name, kind) in dep_labels {
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

    // 4. Force BzlLoadKey to recover the FrozenRuleClass.
    let bzl = force_bzl_load(ctx, bzl_label).await?;

    // 5. Workspace name from MODULE.bazel.
    let graph = force_module_graph(ctx).await?;
    let workspace_name = graph.main.name.clone();

    // 6. Resolve declared toolchains.
    //    Read the rule's `toolchains=[...]` list off the FrozenRuleClass,
    //    walk `graph.registered_toolchains` for each, force-analyze
    //    matching impls, and build the `Toolchains` map for ctx.
    let declared_toolchain_types = read_rule_toolchain_types(&bzl.module, &source_symbol);
    let resolved_toolchains =
        resolve_declared_toolchains(ctx, &declared_toolchain_types, config, &graph).await?;

    // 7. Run analysis (synchronous Starlark driver). Re-borrow
    //    `target` from the same `Arc<Package>` — no deep clone.
    let target = pkg
        .targets
        .get(target_label.name())
        .expect("present above; PackageKey is purely functional");
    analysis::run_analysis(
        target,
        Arc::clone(&bzl.module),
        &source_symbol,
        &workspace_name,
        config,
        &resolved_deps,
        resolved_toolchains,
    )
    .map_err(|e| format!("{e:#}"))
}

/// Look up the rule symbol on the frozen `.bzl` and return the
/// `toolchains=[...]` list it declared. Returns `[]` if anything
/// goes wrong — analysis can still succeed without resolved
/// toolchains (the rule impl just hits `WildcardStruct` on every
/// `ctx.toolchains[T]` access).
fn read_rule_toolchain_types(bzl: &FrozenModule, rule_symbol: &str) -> Vec<String> {
    use crate::starlark_types::rule::FrozenRuleClass;
    use starlark::values::ValueLike;
    let Ok(Some(owned)) = bzl.get_option(rule_symbol) else {
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
    config: &Configuration,
    graph: &ModuleGraph,
) -> Result<crate::starlark_types::toolchains::Toolchains, String> {
    use crate::label::{CanonicalRepo, PackagePath};
    use crate::package::AttrValue;
    use crate::starlark_types::toolchains::Toolchains;

    let repo_mapping = Arc::new(graph.repo_mapping.clone());
    if declared.is_empty() || graph.registered_toolchains.is_empty() {
        return Ok(Toolchains {
            by_type: BTreeMap::new(),
            repo_mapping,
        });
    }

    let main_pkg = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());

    // Wanted set: canonicalize each declared toolchain-type string.
    // Skip any that fail to parse — analysis still proceeds with a
    // partial map.
    let mut wanted: BTreeMap<Label, ()> = BTreeMap::new();
    for s in declared {
        if let Ok(l) = Label::resolve(s, &main_pkg, &repo_mapping) {
            wanted.insert(l, ());
        }
    }
    if wanted.is_empty() {
        return Ok(Toolchains {
            by_type: BTreeMap::new(),
            repo_mapping,
        });
    }

    // Walk registrations in declaration order across the module graph;
    // first match per toolchain-type wins. Each registration may be
    // a single label (`@foo//pkg:tc_name`) or a `:all` pattern that
    // expands to every `toolchain(...)` target in its package — both
    // are handled below.
    let mut by_type: BTreeMap<Label, Arc<AnalyzedTarget>> = BTreeMap::new();
    for reg_label in &graph.registered_toolchains {
        if by_type.len() == wanted.len() {
            break; // every declared type already resolved
        }
        let pkg = match Box::pin(ctx.compute(&PackageKey(reg_label.package.clone()))).await {
            Ok(SharedResult::Ok(p)) => p,
            _ => continue,
        };
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
            // The toolchain target's `toolchain_type` attr canonicalises
            // against ITS package's repo mapping. We use main's mapping
            // here as a STUB — see resolver doc-comment caveat. Most
            // toolchain types are written as fully-qualified labels
            // which canonicalise the same way regardless.
            let tt_label = match Label::resolve(tt_str, &reg_label.package, &repo_mapping) {
                Ok(l) => l,
                Err(_) => continue,
            };
            if !wanted.contains_key(&tt_label) || by_type.contains_key(&tt_label) {
                continue;
            }
            let impl_str = match target.attrs.get("toolchain") {
                Some(AttrValue::String(s)) => s,
                _ => continue,
            };
            let impl_label = match Label::resolve(impl_str, &reg_label.package, &repo_mapping) {
                Ok(l) => l,
                Err(_) => continue,
            };
            let analyzed = force_analysis(ctx, impl_label, config).await?;
            by_type.insert(tt_label, analyzed);
        }
        // `pkg` is released at the end of the loop body — `force_analysis`
        // above runs on each candidate before the borrow is dropped, but
        // it doesn't need `pkg` itself, so this is fine.
    }

    Ok(Toolchains {
        by_type,
        repo_mapping,
    })
}

/// Per-attr label dependency shape. Distinct from `ResolvedDep` —
/// this is the unresolved input; `ResolvedDep` is what
/// `AnalysisKey` produces for each.
enum DepLabels {
    Single(Label),
    List(Vec<Label>),
}

/// Walk the target's attrs, pulling out everything label-typed so
/// `compute_analysis` can issue the AnalysisKey forces in one place.
/// Build a synthetic `AnalyzedTarget` for a source file or unrun
/// native target. `DefaultInfo.files` contains a single
/// [`File::source`] for the target, so consumers reading
/// `ctx.file.<dep>` / `ctx.files.<dep>` see the actual source path
/// (matching Bazel's source-file behaviour).
///
/// STUB for native rules (`filegroup`, `alias`, …): we don't yet
/// follow the rule's `srcs` / `actual` to populate the file list
/// honestly — every native target reports a single File named after
/// itself.
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
            let list = module.heap().alloc(vec![synthetic_file.clone()]);
            module.set("__source_files__", list);
            let frozen = module
                .freeze()
                .expect("freezing a single-File module never fails");
            if let Ok(Some(owned)) = frozen.get_option("__source_files__") {
                // SAFETY: the Arc<FrozenModule> we return owns the
                // FrozenHeap this value came from, and we keep the Arc
                // alive on the AnalyzedTarget. Callers that read
                // `fields["files"]` must do so through the
                // Arc<FrozenModule> kept on `analysis_heap` (the
                // standard add_reference discipline — see DESIGN.md).
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
            id: crate::starlark_types::default_info::default_info_id(),
            fields,
        }],
        actions: Vec::new(),
        analysis_heap: frozen_module,
    }
}

fn collect_dep_labels(target: &crate::package::UnconfiguredTarget) -> Vec<(String, DepLabels)> {
    let mut out = Vec::new();
    for (attr_name, val) in &target.attrs {
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
    let frozen = bzl_eval::evaluate(label, ast, &loaded).map_err(|e| format!("{e:#}"))?;
    Ok(LoadedBzl {
        module: Arc::new(frozen),
    })
}

async fn compute_package(
    ctx: &mut DiceComputations<'_>,
    pkg: &PackageId,
    graph: &ModuleGraph,
) -> Result<Package, String> {
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
