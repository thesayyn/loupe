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
use crate::module_graph::{self, ModuleGraph};
use crate::package::Package;
use loupe_labels::{Label, PackageId};

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

/// Ordered list of external-repo roots. Each root is a
/// [`RepoRoot`](crate::repo_root::RepoRoot) — either an embedded
/// `include_dir`-baked tree compiled into the binary, or a filesystem
/// directory like a vendor dir or `<output_base>/external/`. Label
/// loads targeting non-main repos (`@<canonical>//pkg:foo`) resolve
/// to the first root that reports `has(canonical) == true`. Search
/// order matches the order
/// [`SetExternalRepoRoots::set_external_repo_roots`] was called with.
///
/// Convention: the engine prepends a single embedded root first, then
/// appends every user-supplied `--external-repo-root <PATH>` as
/// filesystem. Embedded wins over disk so vendored Bazel-side fixups
/// (e.g. `@bazel_tools//command_line_option`) stay reproducible
/// regardless of what the user has on disk.
#[derive(Default)]
pub struct ExternalRepoRoots(Vec<crate::repo_root::RepoRoot>);

impl std::fmt::Debug for ExternalRepoRoots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ExternalRepoRoots").field(&self.0).finish()
    }
}

// Manual Allocative: the contained `include_dir::Dir` reference and
// the `PathBuf`s are accounted for by the surrounding `Vec` layout
// but neither carries a real `Allocative` impl. Memory-trivial; opt
// out so DICE storage compiles.
impl allocative::Allocative for ExternalRepoRoots {
    fn visit<'a, 'b: 'a>(&self, _visitor: &'a mut allocative::Visitor<'b>) {}
}

impl ExternalRepoRoots {
    pub fn as_slice(&self) -> &[crate::repo_root::RepoRoot] {
        &self.0
    }
}

pub trait HasExternalRepoRoots {
    /// First root that reports `has(canonical) == true`, or `None`.
    /// Search order matches construction order (embedded first when
    /// the engine prepends it).
    fn external_repo_root_for(&self, canonical: &str) -> Option<&crate::repo_root::RepoRoot>;
    /// All configured roots, in search order. Callers that need to
    /// walk every entry (e.g. `module_graph` harvesting transitive
    /// `register_toolchains` calls) iterate this directly.
    fn external_repo_roots(&self) -> &[crate::repo_root::RepoRoot];
}

impl HasExternalRepoRoots for DiceData {
    fn external_repo_root_for(&self, canonical: &str) -> Option<&crate::repo_root::RepoRoot> {
        self.external_repo_roots().iter().find(|r| r.has(canonical))
    }

    fn external_repo_roots(&self) -> &[crate::repo_root::RepoRoot] {
        self.get::<ExternalRepoRoots>()
            .map(|r| r.as_slice())
            .unwrap_or(&[])
    }
}

pub trait SetExternalRepoRoots {
    fn set_external_repo_roots(&mut self, roots: Vec<crate::repo_root::RepoRoot>);
}

impl SetExternalRepoRoots for DiceDataBuilder {
    fn set_external_repo_roots(&mut self, roots: Vec<crate::repo_root::RepoRoot>) {
        self.set(ExternalRepoRoots(roots));
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

/// In-memory BUILD content the engine serves for the synthetic
/// `@@_loupe_platforms//:` package. Lets CLI callers declare a
/// `platform(...)` target inline (`--platforms_expr=...`) without
/// editing the workspace. Distinct from the Bazel-aligned default
/// path (`@@platforms//host:host` + auto-synthesised
/// `@host_platform`) — this is an escape hatch for cross-build or
/// other custom platform configurations the user wants to pin
/// without rebuilding the @host_platform repo.
#[derive(Debug, Clone, Allocative)]
pub struct PlatformsExpr(Option<String>);

impl PlatformsExpr {
    pub fn as_str(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

pub trait HasPlatformsExpr {
    fn platforms_expr(&self) -> Option<&str>;
}

impl HasPlatformsExpr for DiceData {
    fn platforms_expr(&self) -> Option<&str> {
        self.get::<PlatformsExpr>().ok().and_then(|p| p.as_str())
    }
}

pub trait SetPlatformsExpr {
    fn set_platforms_expr(&mut self, content: Option<String>);
}

impl SetPlatformsExpr for DiceDataBuilder {
    fn set_platforms_expr(&mut self, content: Option<String>) {
        self.set(PlatformsExpr(content));
    }
}

/// Reserved canonical repo name for `--platforms_expr` synthesis.
pub const PLATFORMS_EXPR_REPO: &str = "_loupe_platforms";
/// Reserved target name inside the synthesised package — the
/// `platform(name = "current")` the user is required to declare.
pub const PLATFORMS_EXPR_TARGET: &str = "current";

/// Replay-correctness foundation: every off-DICE `std::fs::read_*` site
/// in a compute fn is a stale-data hole when the DICE version isn't
/// bumped per invocation (the v1 fallback). Routing reads through a
/// `FileContentKey` records the dependency on DICE's edge graph so a
/// future file watcher can invalidate just the touched keys.
///
/// Value is `Arc<Vec<u8>>` so binary files (archives, marker files,
/// proto blobs) flow through unchanged. Consumers that want a `String`
/// or `&str` convert at the use site — same shape as the prior
/// `std::fs::read_to_string` callers, with one allocation moved off
/// the hot path into the keyed result.
///
/// Today (v1, version bumped per invocation), every `FileContentKey`
/// recomputes on every engine instantiation — same effective behaviour
/// as the unrouted reads it replaces, plus the in-memory cache for
/// reuse within a single session. The win lands when incrementality
/// turns on.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("FileContentKey({})", _0.display())]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct FileContentKey(pub PathBuf);

#[async_trait]
impl Key for FileContentKey {
    type Value = SharedResult<Vec<u8>>;

    async fn compute(&self, _ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        match std::fs::read(&self.0) {
            Ok(bytes) => SharedResult::ok(bytes),
            Err(e) => SharedResult::err(format!("reading {}: {e}", self.0.display())),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        // Byte-equality (not pointer-equality) so re-reading an
        // unchanged file produces an equal value, which lets DICE skip
        // downstream invalidation. This is the entire point of routing
        // file reads through DICE — pointer-eq here would defeat it.
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

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ModuleGraphKey")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ModuleGraphKey;

#[async_trait]
impl Key for ModuleGraphKey {
    type Value = SharedResult<ModuleGraph>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        use loupe_bzlmod::bzlmod_graph::HasResolvedBzlmod;

        let root = ctx.global_data().workspace_root().to_path_buf();
        let lockfile_override = ctx
            .global_data()
            .bzlmod_lockfile_path()
            .map(|p| p.to_path_buf());

        // Preferred path: in-memory `BzlmodGraph` snapshot set at
        // engine-build time. Everything `ModuleGraphKey` needs to
        // know lives in there — no `read_dir`, no race against lazy
        // materialisation, no walk over `.loupe/bzlmod/`.
        if let Some(bzlmod_graph) = ctx.global_data().resolved_bzlmod() {
            return match module_graph::synthesise_module_graph(
                &root,
                &bzlmod_graph,
                lockfile_override,
            ) {
                Ok(g) => SharedResult::ok(g),
                Err(e) => SharedResult::err(format!("{e:#}")),
            };
        }

        // Fallback for callers that build `Dice` directly without
        // going through `EngineBuilder` (tests, mainly). Falls back
        // to the on-disk walker; loses the in-memory enrichment
        // `EngineBuilder::build` provides, but tests don't need it.
        let external_roots: Vec<crate::repo_root::RepoRoot> =
            ctx.global_data().external_repo_roots().to_vec();
        // Route MODULE.bazel + MODULE.bazel.lock reads through
        // `FileContentKey` so file-watcher invalidation can land
        // without re-parsing the whole module graph on each tick.
        let module_bazel_path = root.join("MODULE.bazel");
        let module_bazel_bytes = match force_file_content(ctx, module_bazel_path.clone()).await {
            Ok(b) => b,
            Err(e) => return SharedResult::err(e),
        };
        let module_bazel_src = match std::str::from_utf8(&module_bazel_bytes) {
            Ok(s) => s.to_string(),
            Err(e) => {
                return SharedResult::err(format!(
                    "{} is not UTF-8: {e}",
                    module_bazel_path.display()
                ));
            }
        };
        let lockfile_path_concrete = lockfile_override
            .clone()
            .unwrap_or_else(|| root.join("MODULE.bazel.lock"));
        let lockfile = if lockfile_path_concrete.exists() {
            let lockfile_bytes = match force_file_content(ctx, lockfile_path_concrete.clone()).await
            {
                Ok(b) => b,
                Err(e) => return SharedResult::err(e),
            };
            let lockfile_src = match std::str::from_utf8(&lockfile_bytes) {
                Ok(s) => s,
                Err(e) => {
                    return SharedResult::err(format!(
                        "{} is not UTF-8: {e}",
                        lockfile_path_concrete.display()
                    ));
                }
            };
            match crate::lockfile::parse_from_str(lockfile_src) {
                Ok(l) => l,
                Err(e) => {
                    return SharedResult::err(format!(
                        "parsing {}: {e:#}",
                        lockfile_path_concrete.display()
                    ));
                }
            }
        } else {
            crate::lockfile::Lockfile::default()
        };
        match module_graph::parse_with_pre_read_inputs(
            &root,
            &module_bazel_src,
            lockfile,
            &external_roots,
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
///
/// `source_digest` is the hex SHA-256 of the `.bzl` source bytes —
/// surfaced so callers building a `RepoSpec::StarlarkRule` can stamp it
/// onto the spec for inclusion in the marker file's
/// `predeclaredInputHash`. Identity-pointer-equality on `module` keeps
/// equality cheap; the digest is just data alongside.
#[derive(Debug, Clone, Allocative)]
pub struct LoadedBzl {
    pub module: Arc<FrozenModule>,
    pub source_digest: Arc<String>,
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
        // Don't walk the engine's external-repo roots when they live
        // inside the workspace (`<workspace>/vendor` from
        // `bazel mod vendor`). Otherwise every vendored BUILD gets
        // attributed to the main module as
        // `@@//vendor/<canonical>/...`, producing ghost-target
        // failures during analysis.
        // Only filesystem-backed roots can shadow workspace dirs
        // (`<workspace>/vendor`). Embedded roots have no disk presence
        // and are irrelevant to the package-walk skip list.
        let mut skip_paths: Vec<PathBuf> = ctx
            .global_data()
            .external_repo_roots()
            .iter()
            .filter_map(|r| r.filesystem_root().map(Path::to_path_buf))
            .collect();
        // The `--bzlmod` auto-cache lives at `<workspace>/.loupe/bzlmod`
        // (or wherever the user pointed `--cache`). Extension-evaluated
        // repos under that cache have their own BUILD files; without
        // skipping them, every cached repo's `BUILD.bazel` would get
        // re-attributed to the main module.
        if let Some(cache) = loupe_bzlmod::HasBzlmodCache::bzlmod_cache(ctx.global_data()) {
            skip_paths.push(cache.to_path_buf());
        }
        let bazelignore = crate::workspace::load_bazelignore(&root);
        match crate::workspace::discover_packages(&root, &skip_paths, &bazelignore) {
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
///
/// `cvs`: the union of constraint_values the platform (and its
/// parents) explicitly list.
///
/// `assigned_settings`: the set of `constraint_setting`s that the
/// platform's CVs cover. Used by the
/// `constraint_setting.default_constraint_value` fallback: a candidate
/// CV whose setting is NOT in this set is considered satisfied if it
/// equals the setting's default. Bazel's "platform doesn't speak to
/// this setting, so the setting's default value implicitly holds."
///
/// `None` when no platform was configured or the platform target
/// couldn't be loaded (callers treat that as "no constraint check").
/// Per-`Configuration` memoization means every target+toolchain in one
/// cquery shares one resolved set — Bazel re-derives it per-target.
#[derive(Debug, Clone, PartialEq, Eq, Allocative, Default)]
pub struct PlatformConstraintSet {
    pub cvs: std::collections::BTreeSet<Label>,
    pub assigned_settings: std::collections::BTreeSet<Label>,
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct TargetPlatformConstraints(pub Option<PlatformConstraintSet>);

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

/// Exec-platform constraint set, mirroring [`TargetPlatformConstraints`]
/// but keyed off the configuration's exec platform. Reads
/// `Configuration::exec_platform()` which falls back to the target
/// platform when no exec platform is set explicitly — so host builds
/// transparently see the same set on both sides.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct ExecPlatformConstraints(pub Option<PlatformConstraintSet>);

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ExecPlatformConstraintsKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ExecPlatformConstraintsKey(pub Configuration);

#[async_trait]
impl Key for ExecPlatformConstraintsKey {
    type Value = SharedResult<ExecPlatformConstraints>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let graph = match force_module_graph(ctx).await {
            Ok(g) => g,
            Err(e) => return SharedResult::err(e),
        };
        let constraints = match self.0.exec_platform() {
            Some(p) => load_target_platform_constraints(ctx, p, &graph.repo_mapping).await,
            None => None,
        };
        SharedResult::ok(ExecPlatformConstraints(constraints))
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

    let target = match pkg.targets.get(target_label.name()) {
        Some(t) => t,
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
    };

    // `alias`, `label_flag`, and `label_setting` all bypass the
    // rule-impl path — they forward providers from the `actual` /
    // `build_setting_default` target instead of running an
    // implementation function. Every other kind
    // (`filegroup`/`genrule`/`platform`/`toolchain`/the
    // `bool_flag`/`string_flag`/… from `command_line_option/settings.bzl`)
    // runs its real user-space `rule(...)` impl.
    if matches!(
        target.kind.as_str(),
        "alias" | "label_flag" | "label_setting"
    ) {
        return synthesise_alias_analysis(ctx, target, config).await;
    }

    let (bzl_label, source_symbol) = match (&target.bzl_label, &target.source_symbol) {
        (Some(l), Some(s)) => (l.clone(), s.clone()),
        // Native loading-only kinds (`constraint_setting`,
        // `platform`, `toolchain`, `toolchain_type`) — record-only,
        // never invoke an impl. Surface a synthetic single-File
        // DefaultInfo so `bazel cquery` shape is preserved.
        _ => return Ok(synthesise_source_file_analysis(target, config)),
    };

    // Force the rule's per-target sub-keys in parallel. The chain that
    // produces `resolved_deps` (m_attrs → bzl → attrs_schema →
    // resolve_label_deps) overlaps with the four orthogonal forces
    // (toolchains, target+exec platform constraints, module graph)
    // via `try_compute2` at the top level and `try_compute_join` over
    // a `Force` enum for the four parallel siblings. The closure passed
    // to `try_compute_join` captures only `&Label` and `&Configuration`,
    // both of which are `Copy`, so the `Copy + Send + Sync` mapper
    // bound is satisfied without per-item clones.
    #[derive(Clone, Copy)]
    enum Force {
        Toolchains,
        Plat,
        Exec,
        Graph,
    }
    enum Forced {
        Toolchains(Arc<crate::starlark_types::toolchains::Toolchains>),
        Plat(Arc<TargetPlatformConstraints>),
        Exec(Arc<ExecPlatformConstraints>),
        Graph(Arc<crate::module_graph::ModuleGraph>),
    }
    let bzl_label_for_chain = bzl_label.clone();
    let source_symbol_for_chain = source_symbol.clone();
    let (chain_result, forced_vec) = ctx
        .try_compute2(
            move |c| {
                use futures::FutureExt;
                async move {
                    let m_attrs = force_materialised_attrs(c, target_label, config).await?;
                    let bzl = force_bzl_load(c, bzl_label_for_chain).await?;
                    let attrs_schema = read_rule_attr_specs(&bzl.module, &source_symbol_for_chain);
                    let resolved_deps =
                        resolve_label_deps(c, &m_attrs.0, &attrs_schema, config).await?;
                    Ok::<_, String>((m_attrs, bzl, resolved_deps))
                }
                .boxed()
            },
            |c| {
                use futures::FutureExt;
                async move {
                    c.try_compute_join(
                        [Force::Toolchains, Force::Plat, Force::Exec, Force::Graph],
                        |c, f| {
                            use futures::FutureExt;
                            async move {
                                match f {
                                    Force::Toolchains => {
                                        force_resolved_toolchains(c, target_label, config)
                                            .await
                                            .map(Forced::Toolchains)
                                    }
                                    Force::Plat => force_target_platform_constraints(c, config)
                                        .await
                                        .map(Forced::Plat),
                                    Force::Exec => force_exec_platform_constraints(c, config)
                                        .await
                                        .map(Forced::Exec),
                                    Force::Graph => force_module_graph(c).await.map(Forced::Graph),
                                }
                            }
                            .boxed()
                        },
                    )
                    .await
                }
                .boxed()
            },
        )
        .await?;
    let (m_attrs, bzl, resolved_deps) = chain_result;
    let mut toolchains_opt = None;
    let mut plat_constraints_opt = None;
    let mut exec_constraints_opt = None;
    let mut graph_opt = None;
    for f in forced_vec {
        match f {
            Forced::Toolchains(v) => toolchains_opt = Some(v),
            Forced::Plat(v) => plat_constraints_opt = Some(v),
            Forced::Exec(v) => exec_constraints_opt = Some(v),
            Forced::Graph(v) => graph_opt = Some(v),
        }
    }
    let toolchains = toolchains_opt.expect("Force::Toolchains result missing");
    let plat_constraints = plat_constraints_opt.expect("Force::Plat result missing");
    let exec_constraints = exec_constraints_opt.expect("Force::Exec result missing");
    let graph = graph_opt.expect("Force::Graph result missing");

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
        plat_constraints.0.as_ref().map(|s| Arc::new(s.cvs.clone())),
        exec_constraints.0.as_ref().map(|s| Arc::new(s.cvs.clone())),
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
    let exec_constraints = force_exec_platform_constraints(ctx, config).await?;
    resolve_declared_toolchains(
        ctx,
        &declared,
        &bzl_label.package,
        config,
        &graph,
        plat_constraints.0.as_ref(),
        exec_constraints.0.as_ref(),
    )
    .await
}

/// Walk materialised attrs, force `AnalysisKey` for each label-typed
/// dep under the per-attr-derived configuration, and stitch the
/// results into a `BTreeMap` keyed by attr name.
///
/// `attrs_schema` comes from the rule's frozen `AttrSpec` map and
/// supplies the [`AttrCfg`] classifier per attr.
/// [`derive_dep_config`] turns it into the dep's `Configuration`.
async fn resolve_label_deps(
    ctx: &mut DiceComputations<'_>,
    materialised_attrs: &BTreeMap<String, crate::package::AttrValue>,
    attrs_schema: &BTreeMap<String, crate::starlark_types::attr::FrozenAttrSpec>,
    config: &Configuration,
) -> Result<BTreeMap<String, ResolvedDep>, String> {
    use crate::starlark_types::attr::{AttrCfgGen, AttrCfgKind};
    let mut resolved_deps: BTreeMap<String, ResolvedDep> = BTreeMap::new();
    for (attr_name, kind) in collect_dep_labels(materialised_attrs) {
        let schema = attrs_schema.get(&attr_name);
        let attr_cfg_kind = schema
            .map(|s| s.cfg.kind())
            .unwrap_or(AttrCfgKind::Inherited);
        let dep_config = match (attr_cfg_kind, schema) {
            (AttrCfgKind::Transition, Some(s)) => {
                if let AttrCfgGen::Transition(fv) = &s.cfg {
                    apply_user_transition(config, *fv, materialised_attrs)
                        .unwrap_or_else(|_| config.clone())
                } else {
                    config.clone()
                }
            }
            _ => derive_dep_config(config, attr_cfg_kind),
        };
        match kind {
            DepLabels::Single(label) => {
                let dep = force_analysis(ctx, label, &dep_config).await?;
                resolved_deps.insert(attr_name, ResolvedDep::Single(dep));
            }
            DepLabels::List(labels) => {
                // Fan out all label-list elements in parallel. The
                // closure passed to `try_compute_join` must be `Copy`,
                // so the per-item `Configuration` rides along inside
                // the item tuple instead of being captured.
                let items: Vec<(Label, Configuration)> = labels
                    .into_iter()
                    .map(|l| (l, dep_config.clone()))
                    .collect();
                let out: Vec<Arc<AnalyzedTarget>> = ctx
                    .try_compute_join(items, |ctx, (label, cfg)| {
                        use futures::FutureExt;
                        async move { force_analysis(ctx, label, &cfg).await }.boxed()
                    })
                    .await?;
                resolved_deps.insert(attr_name, ResolvedDep::List(out));
            }
            DepLabels::KeyedDict(entries) => {
                // Same fan-out, with the per-entry string preserved
                // separately so the closure stays `Copy`.
                let labels_only: Vec<Label> = entries.iter().map(|(l, _)| l.clone()).collect();
                let strs: Vec<String> = entries.into_iter().map(|(_, s)| s).collect();
                let items: Vec<(Label, Configuration)> = labels_only
                    .iter()
                    .map(|l| (l.clone(), dep_config.clone()))
                    .collect();
                let deps: Vec<Arc<AnalyzedTarget>> = ctx
                    .try_compute_join(items, |ctx, (label, cfg)| {
                        use futures::FutureExt;
                        async move { force_analysis(ctx, label, &cfg).await }.boxed()
                    })
                    .await?;
                let out: Vec<(Label, Arc<AnalyzedTarget>, String)> = labels_only
                    .into_iter()
                    .zip(deps)
                    .zip(strs)
                    .map(|((l, d), s)| (l, d, s))
                    .collect();
                resolved_deps.insert(attr_name, ResolvedDep::Dict(out));
            }
        }
    }
    Ok(resolved_deps)
}

/// Per-attr config derivation. Returns the `Configuration` each dep
/// should be analyzed under given the parent's configuration and the
/// attr's [`AttrCfg`] classifier.
///
/// `AttrCfg::Exec` transitions the dep onto the exec platform: the
/// returned config keeps the parent's build settings + exec platform
/// but replaces `target_platform` with the parent's effective exec
/// platform (`Configuration::exec_platform()`). When no exec platform
/// is set explicitly, `exec_platform()` falls back to the target —
/// preserving the single-platform host-build semantics Loupe shipped
/// with before the split. `AttrCfg::Target` and `Inherited` pass the
/// parent through unchanged.
fn derive_dep_config(
    parent: &Configuration,
    attr_cfg: crate::starlark_types::attr::AttrCfgKind,
) -> Configuration {
    use crate::starlark_types::attr::AttrCfgKind;
    match attr_cfg {
        AttrCfgKind::Inherited | AttrCfgKind::Target => parent.clone(),
        AttrCfgKind::Exec => match parent.exec_platform() {
            Some(exec) => parent.clone().with_target_platform(exec.clone()).as_exec(),
            // Even with no exec platform configured, the dep's
            // configuration *is* the exec config (host-build fallback).
            // Mark it so `bazel-out/<cpu>-opt-exec-<hash>/...` shows up
            // for exec-side actions instead of the parent's
            // `<cpu>-fastbuild/...` — matches what Bazel produces.
            None => parent.clone().as_exec(),
        },
        AttrCfgKind::NoneCfg => Configuration::unconfigured(),
        AttrCfgKind::Unconfigured => parent.without_build_settings(),
        // Step 6 wires the transition function through here. Today
        // the dep resolves under the parent's config and only the
        // surface shape (list-of-one) differs — matches the previous
        // `cfg_transition: bool` behaviour. The actual firing of the
        // transition fn lives in a separate code path that has access
        // to the schema's `AttrCfgGen::Transition(V)` payload.
        AttrCfgKind::Transition => parent.clone(),
    }
}

/// Fire a user transition function captured on
/// `AttrCfgGen::Transition(fv)`. `fv` is expected to be a
/// `_TransitionInfo` provider instance with fields `implementation`
/// (function), `inputs` (list of flag labels), `outputs` (list of
/// flag labels).
///
/// Signature: `def impl(settings, attr) -> dict`. Loupe only accepts
/// the two-argument form per the cfg plan.
///
/// Returns a new `Configuration` with the result dict overlaid on the
/// parent's build settings. Falls back to `parent.clone()` on any
/// error so a broken transition doesn't poison analysis.
fn apply_user_transition(
    parent: &Configuration,
    transition_value: starlark::values::FrozenValue,
    materialised_attrs: &BTreeMap<String, crate::package::AttrValue>,
) -> Result<Configuration, String> {
    use starlark::environment::{Globals, Module};
    use starlark::eval::Evaluator;
    use starlark::values::dict::{AllocDict, DictRef};
    use starlark::values::structs::AllocStruct;

    let result_settings: BTreeMap<String, String> = Module::with_temp_heap(|module| {
        // Re-extract on the live heap so subsequent allocations are
        // anchored on `module`.
        let tv = transition_value.to_value();
        let impl_fn = tv
            .get_attr("implementation", module.heap())
            .map_err(|e| format!("transition .implementation: {e}"))?
            .ok_or_else(|| "transition has no `implementation` field".to_string())?;
        let inputs_v = tv
            .get_attr("inputs", module.heap())
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                module
                    .heap()
                    .alloc(Vec::<starlark::values::Value<'_>>::new())
            });
        let inputs_list =
            starlark::values::list::ListRef::from_value(inputs_v).ok_or_else(|| {
                format!(
                    "transition.inputs is not a list (got {})",
                    inputs_v.get_type()
                )
            })?;
        let inputs: Vec<String> = inputs_list
            .iter()
            .filter_map(|v| v.unpack_str().map(|s| s.to_string()))
            .collect();
        let outputs_v = tv
            .get_attr("outputs", module.heap())
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                module
                    .heap()
                    .alloc(Vec::<starlark::values::Value<'_>>::new())
            });
        let outputs_list =
            starlark::values::list::ListRef::from_value(outputs_v).ok_or_else(|| {
                format!(
                    "transition.outputs is not a list (got {})",
                    outputs_v.get_type()
                )
            })?;
        let outputs: std::collections::BTreeSet<String> = outputs_list
            .iter()
            .filter_map(|v| v.unpack_str().map(|s| s.to_string()))
            .collect();

        // Build settings: keys in `inputs` looked up against the
        // parent config's build_settings (defaults to empty string if
        // absent — matches "flag has no override").
        let settings_pairs: Vec<(starlark::values::Value<'_>, starlark::values::Value<'_>)> =
            inputs
                .iter()
                .map(|k| {
                    let v = parent.build_settings().get(k).cloned().unwrap_or_default();
                    (
                        module.heap().alloc(k.as_str()),
                        module.heap().alloc(v.as_str()),
                    )
                })
                .collect();
        let settings_val = module.heap().alloc(AllocDict(settings_pairs));

        // Build attr struct from materialised_attrs. Strings/labels
        // surface as their stringified value; deeper kinds (depset,
        // selector remnants) drop to empty string — transitions
        // rarely read them.
        let attr_pairs: Vec<(&str, starlark::values::Value<'_>)> = materialised_attrs
            .iter()
            .map(|(k, v)| {
                let stringified = match v {
                    crate::package::AttrValue::String(s) => s.clone(),
                    crate::package::AttrValue::Label(l) => l.to_string(),
                    other => format!("{:?}", other),
                };
                (k.as_str(), module.heap().alloc(stringified))
            })
            .collect();
        let attr_val = module.heap().alloc(AllocStruct(attr_pairs));

        let _ = Globals::standard();
        let mut eval = Evaluator::new(&module);
        let result = eval
            .eval_function(impl_fn, &[settings_val, attr_val], &[])
            .map_err(|e| format!("transition impl raised: {e}"))?;

        let dict = DictRef::from_value(result)
            .ok_or_else(|| format!("transition impl returned non-dict ({})", result.get_type()))?;
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in dict.iter() {
            let key = k
                .unpack_str()
                .map(|s| s.to_string())
                .ok_or_else(|| "transition result key not a string".to_string())?;
            if !outputs.is_empty() && !outputs.contains(&key) {
                return Err(format!(
                    "transition returned key {key:?} not declared in outputs"
                ));
            }
            let val = v
                .unpack_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string());
            out.insert(key, val);
        }
        Ok(out)
    })?;

    // Apply overlay.
    let mut new_cfg = parent.clone();
    for (k, v) in result_settings {
        new_cfg = new_cfg.with_build_setting(k, v);
    }
    Ok(new_cfg)
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

/// Look up the rule symbol on the frozen `.bzl` and return its
/// `attrs={...}` schema. Empty map if anything goes wrong — the
/// analyzer treats an absent schema as "every attr is inherited",
/// which matches v1's previous behaviour.
fn read_rule_attr_specs(
    bzl: &FrozenModule,
    rule_symbol: &str,
) -> BTreeMap<String, crate::starlark_types::attr::FrozenAttrSpec> {
    use crate::starlark_types::rule::FrozenRuleClass;
    use starlark::values::ValueLike;
    let Ok((owned, _vis)) = bzl.get_any_visibility(rule_symbol) else {
        return BTreeMap::new();
    };
    let Some(rc) = owned.value().downcast_ref::<FrozenRuleClass>() else {
        return BTreeMap::new();
    };
    rc.attrs.clone()
}

/// First-match toolchain resolver. For every type in `declared`, scan
/// `graph.registered_toolchains` (already pre-canonicalised across
/// the entire module graph by `module_graph::parse_with_lockfile_override`)
/// and pick the first registered `toolchain` target whose
/// `toolchain_type` attr matches. Force-analyze that toolchain's
/// `toolchain` (impl) label and store under the canonical type label.
///
/// STUBs (deferred):
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
    target_platform_constraints: Option<&PlatformConstraintSet>,
    exec_platform_constraints: Option<&PlatformConstraintSet>,
) -> Result<crate::starlark_types::toolchains::Toolchains, String> {
    let _ = config;
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

    // Wave 1: fan out every registration's `PackageKey` in parallel.
    // DICE memoizes so duplicate package fetches dedupe across the
    // wave. The first-match-wins walk below runs sequentially over the
    // pre-loaded packages — the ordering constraint forbids parallel
    // candidate selection across registrations, but loading the
    // packages doesn't depend on ordering.
    let reg_with_pkg: Vec<(Label, Option<Arc<Package>>)> = ctx
        .compute_join(
            graph.registered_toolchains.clone(),
            |ctx, reg_label: Label| {
                use futures::FutureExt;
                async move {
                    let pkg = match ctx
                        .compute(&PackageKey(reg_label.package.clone()))
                        .await
                    {
                        Ok(SharedResult::Ok(p)) => Some(p),
                        Ok(SharedResult::Err(e)) => {
                            tracing::debug!(reg=%reg_label, error=%e, "toolchain package load failed");
                            None
                        }
                        _ => None,
                    };
                    (reg_label, pkg)
                }
                .boxed()
            },
        )
        .await;

    // Wave 2: walk registrations in declaration order across the module
    // graph; first compatible match per toolchain-type wins. Each
    // registration may be a single label (`@foo//pkg:tc_name`) or a
    // `:all` pattern that expands to every `toolchain(...)` target in
    // its package. Constraint checks stay serial inside the candidate
    // loop because the first-match-wins semantics requires the scan to
    // observe an earlier candidate's win before considering the next.
    let mut by_type: BTreeMap<Label, Arc<AnalyzedTarget>> = BTreeMap::new();
    for (reg_label, pkg_opt) in &reg_with_pkg {
        if by_type.len() == wanted.len() {
            break; // every declared type already resolved
        }
        let Some(pkg) = pkg_opt else {
            continue;
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
            let Some(tt_str) = attr_as_label_string(target.attrs.get("toolchain_type")) else {
                continue;
            };
            let tt_label = match Label::resolve(&tt_str, &reg_label.package, &reg_mapping) {
                Ok(l) => l,
                Err(_) => continue,
            };
            tracing::debug!(reg=%reg_label, cand=cand_name, tt=%tt_label, wanted_count=wanted.len(), is_wanted=wanted.contains_key(&tt_label), "tc cand check");
            if !wanted.contains_key(&tt_label) || by_type.contains_key(&tt_label) {
                continue;
            }
            // Constraint compatibility check. Toolchains list
            // `constraint_values` their *target* and *exec* sides must
            // satisfy; each side checks against its own platform set.
            // When no target platform is configured, the corresponding
            // check is skipped (Bazel's "no platform = universally
            // compatible" semantics). `exec_platform_constraints`
            // falls back to `target_platform_constraints` upstream
            // when no exec platform was set explicitly, so host-build
            // callers see identical sets on both sides.
            //
            // `constraint_setting.default_constraint_value` IS honoured
            // via `candidate_satisfies_constraints_async`: when the
            // platform doesn't speak to a constraint's setting, the
            // setting's default value fills in.
            if let Some(plat_constraints) = target_platform_constraints {
                let target_ok = candidate_satisfies_constraints_async(
                    ctx,
                    target.attrs.get("target_compatible_with"),
                    &reg_label.package,
                    &reg_mapping,
                    plat_constraints,
                )
                .await;
                if !target_ok {
                    continue;
                }
            }
            if let Some(exec_plat) = exec_platform_constraints {
                let exec_ok = candidate_satisfies_constraints_async(
                    ctx,
                    target.attrs.get("exec_compatible_with"),
                    &reg_label.package,
                    &reg_mapping,
                    exec_plat,
                )
                .await;
                if !exec_ok {
                    continue;
                }
            }
            let Some(impl_str) = attr_as_label_string(target.attrs.get("toolchain")) else {
                continue;
            };
            let impl_label = match Label::resolve(&impl_str, &reg_label.package, &reg_mapping) {
                Ok(l) => l,
                Err(_) => continue,
            };
            // Analyze the impl under the exec config: a toolchain is a
            // tool that runs on the exec platform, so its dep graph
            // selects against the exec platform's constraints (not the
            // consumer's target). `derive_dep_config` with
            // `AttrCfgKind::Exec` produces exactly that transition,
            // falling back to the parent config when no exec platform
            // is set explicitly (host-build no-op).
            //
            // Failures surface: by the time a candidate clears the
            // exec/target constraint checks above, it's the
            // first-match-wins winner for this `toolchain_type` and
            // Bazel would have analyzed exactly it. Swallowing a real
            // analysis error here would hide the gap and resurface
            // downstream as a confusing `no resolved toolchain` raise
            // (the candidate matched but secretly fell through).
            let impl_config =
                derive_dep_config(config, crate::starlark_types::attr::AttrCfgKind::Exec);
            let analyzed = force_analysis(ctx, impl_label.clone(), &impl_config)
                .await
                .map_err(|e| format!("analyzing toolchain impl {impl_label}: {e}"))?;
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
/// Async variant that also dereferences `alias(actual=…)` on each
/// constraint before matching, so a toolchain spelling
/// `@platforms//cpu:arm64` matches a platform spelling
/// `:aarch64` (and vice versa). Both forms get checked against the
/// platform set: the as-written label, and its alias target.
async fn candidate_satisfies_constraints_async(
    ctx: &mut DiceComputations<'_>,
    attr: Option<&crate::package::AttrValue>,
    reg_pkg: &PackageId,
    repo_mapping: &BTreeMap<String, loupe_labels::CanonicalRepo>,
    plat: &PlatformConstraintSet,
) -> bool {
    let labels = attr_as_label_strings(attr);
    if labels.is_empty() {
        // Missing / empty attr = no constraints declared = universally
        // compatible. The reader returns `[]` for both genuinely-empty
        // lists and unrecognised shapes; either way the toolchain has
        // nothing to enforce against.
        return true;
    }
    for s in labels {
        let l = match Label::resolve(&s, reg_pkg, repo_mapping) {
            Ok(l) => l,
            Err(_) => return false,
        };
        if !Box::pin(constraint_is_satisfied(ctx, &l, plat)).await {
            return false;
        }
    }
    true
}

/// Read a `label`-shaped attr's string form, normalising across the
/// two storage shapes Loupe ends up with:
///
/// - `AttrValue::String(label_str)` — produced by Rust-side native
///   recorders (today only `alias`) and by `attr.string(...)` attrs
///   whose value happens to be a label.
/// - `AttrValue::Label(canonical)` — produced by `validate_and_resolve`
///   for `attr.label(...)` attrs on user-space `rule(...)` definitions.
///
/// Both surface as a single string that downstream `Label::resolve`
/// can re-parse.
fn attr_as_label_string(v: Option<&crate::package::AttrValue>) -> Option<String> {
    use crate::package::AttrValue;
    match v {
        Some(AttrValue::String(s)) => Some(s.clone()),
        Some(AttrValue::Label(l)) => Some(l.to_string()),
        _ => None,
    }
}

/// Read a `label_list`-shaped attr as a list of label strings,
/// normalising across:
///
/// - `AttrValue::List(AttrValue::String)` — native-recorder form.
/// - `AttrValue::LabelList(Label)` — user-space `attr.label_list(...)`
///   after `validate_and_resolve` canonicalises each entry.
fn attr_as_label_strings(v: Option<&crate::package::AttrValue>) -> Vec<String> {
    use crate::package::AttrValue;
    match v {
        Some(AttrValue::List(items)) => items
            .iter()
            .filter_map(|x| match x {
                AttrValue::String(s) => Some(s.clone()),
                AttrValue::Label(l) => Some(l.to_string()),
                _ => None,
            })
            .collect(),
        Some(AttrValue::LabelList(labels)) => labels.iter().map(|l| l.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Follow `alias(actual = …)` from a constraint_value's label.
/// Returns `Some(canonical)` if the target is an alias chain ending
/// at a different `constraint_value`; `None` if the label is already
/// canonical or unresolvable. Used by `load_one_platform` to expand
/// the platform's constraint set to cover both naming forms (e.g.
/// `@platforms//cpu:arm64` ↔ `:aarch64`).
async fn dereference_constraint_alias(
    ctx: &mut DiceComputations<'_>,
    label: Label,
) -> Option<Label> {
    use crate::package::AttrValue;
    let pkg = match Box::pin(ctx.compute(&PackageKey(label.package.clone()))).await {
        Ok(SharedResult::Ok(p)) => p,
        _ => return None,
    };
    let target = pkg.targets.get(label.name())?;
    if target.kind != "alias" {
        return None;
    }
    let actual = match target.attrs.get("actual") {
        Some(AttrValue::String(s)) => s.clone(),
        Some(AttrValue::Label(l)) => return Some(l.clone()),
        _ => return None,
    };
    Label::resolve(&actual, &label.package, &Default::default()).ok()
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
    repo_mapping: &BTreeMap<String, loupe_labels::CanonicalRepo>,
    config: &Configuration,
    target_platform_constraints: Option<&PlatformConstraintSet>,
) -> Result<analysis::SelectMatches, String> {
    let mut keys: Vec<String> = Vec::new();
    for v in target_attrs.values() {
        walk_attr_select_keys(v, &mut keys);
    }
    keys.sort();
    keys.dedup();

    // Resolve every branch key to its canonical Label in one synchronous
    // pass (no DICE call). Drop `//conditions:default` and any that
    // fail canonical resolution; pass the rest into the parallel
    // evaluator.
    let resolved: Vec<Label> = keys
        .into_iter()
        .filter(|k| k != "//conditions:default")
        .filter_map(|k| Label::resolve(&k, target_pkg, repo_mapping).ok())
        .collect();

    // Fan out per-branch evaluation. Each branch independently forces
    // its `PackageKey`, the `AnalysisKey` for `config_setting` kinds,
    // and the flag-value `AnalysisKey`s; `constraint_is_satisfied`
    // forces alias-dereferences via its own `PackageKey` lookups.
    // DICE dedupes overlapping forces across the wavefront.
    let per_branch: Vec<(Label, Option<std::collections::BTreeSet<Label>>)> = ctx
        .try_compute_join(resolved, |ctx, key_label: Label| {
            use futures::FutureExt;
            async move {
                let m = evaluate_select_branch(
                    ctx,
                    &key_label,
                    repo_mapping,
                    config,
                    target_platform_constraints,
                )
                .await?;
                Ok::<_, String>((key_label, m))
            }
            .boxed()
        })
        .await?;

    // Stitch the per-branch matches into the output map. `try_compute_join`
    // returns results in input order; `BTreeMap::insert` then sorts by
    // Label, which is the same order the previous sequential walk
    // produced (`keys` was sorted+deduped above, label-resolution is a
    // pure function of the string and pkg+mapping).
    let mut out: analysis::SelectMatches = std::collections::BTreeMap::new();
    for (key_label, m) in per_branch {
        if let Some(set) = m {
            out.insert(key_label, set);
        }
    }
    Ok(out)
}

/// Per-branch matcher for `compute_select_matches`. Returns `Some(set)`
/// when the branch's key (a `constraint_value` or `config_setting`
/// target) matches the configured platform; `None` otherwise (mismatch,
/// load failure, or other unmodelled kind — silently drop, same as the
/// previous sequential `continue` arms).
async fn evaluate_select_branch(
    ctx: &mut DiceComputations<'_>,
    key_label: &Label,
    repo_mapping: &BTreeMap<String, loupe_labels::CanonicalRepo>,
    config: &Configuration,
    target_platform_constraints: Option<&PlatformConstraintSet>,
) -> Result<Option<std::collections::BTreeSet<Label>>, String> {
    let pkg = match ctx.compute(&PackageKey(key_label.package.clone())).await {
        Ok(SharedResult::Ok(p)) => p,
        _ => return Ok(None),
    };
    let Some(target) = pkg.targets.get(key_label.name()) else {
        return Ok(None);
    };
    let kind = target.kind.clone();
    drop(pkg);
    match kind.as_str() {
        "constraint_value" => {
            if let Some(plat) = target_platform_constraints
                && constraint_is_satisfied(ctx, key_label, plat).await
            {
                // A constraint_value branch's "constraint set" is just
                // itself — a singleton.
                let mut set = std::collections::BTreeSet::new();
                set.insert(key_label.clone());
                return Ok(Some(set));
            }
            Ok(None)
        }
        // `config_setting` is the user-space rule from
        // `builtins_core/exports.bzl`; `_config_setting_rule` was the
        // legacy macro-wrapping name. Both record a target whose
        // `ConfigSettingInfo` carries the matcher.
        "_config_setting_rule" | "config_setting" => {
            let analyzed = force_analysis(ctx, key_label.clone(), config)
                .await
                .map_err(|e| format!("analyzing select() branch {key_label}: {e}"))?;
            let Some(cs_info) = analyzed.provider_by_symbol("ConfigSettingInfo") else {
                return Ok(None);
            };
            let (constraints, flag_values_dict) = match read_config_setting_info(cs_info) {
                Some(p) => p,
                None => return Ok(None),
            };
            if constraints.is_empty() && flag_values_dict.is_empty() {
                return Ok(None);
            }
            let mut cv_labels: Vec<Label> = Vec::with_capacity(constraints.len());
            for s in &constraints {
                let l = match Label::resolve(s, &key_label.package, repo_mapping) {
                    Ok(l) => l,
                    Err(_) => return Ok(None),
                };
                let satisfied = match target_platform_constraints {
                    Some(plat) => constraint_is_satisfied(ctx, &l, plat).await,
                    None => false,
                };
                if !satisfied {
                    return Ok(None);
                }
                cv_labels.push(l);
            }
            let mut flag_labels: Vec<Label> = Vec::with_capacity(flag_values_dict.len());
            for (flag_str, want) in &flag_values_dict {
                let flag_label = match Label::resolve(flag_str, &key_label.package, repo_mapping) {
                    Ok(l) => l,
                    Err(_) => return Ok(None),
                };
                let flag_target = force_analysis(ctx, flag_label.clone(), config)
                    .await
                    .map_err(|e| {
                        format!("analyzing select() flag {flag_label} on {key_label}: {e}")
                    })?;
                let bsi = match flag_target.provider_by_symbol("BuildSettingInfo") {
                    Some(p) => p,
                    None => return Ok(None),
                };
                let actual = match bsi.fields.get("value") {
                    Some(v) => v.to_value().to_string().trim_matches('"').to_string(),
                    None => String::new(),
                };
                if !flag_value_matches(&actual, want) {
                    return Ok(None);
                }
                flag_labels.push(flag_label);
            }
            let mut set = std::collections::BTreeSet::new();
            set.extend(cv_labels);
            set.extend(flag_labels);
            Ok(Some(set))
        }
        _ => Ok(None),
    }
}

/// Read `(constraints, flag_values)` off a `ConfigSettingInfo`
/// provider. Returns `None` if either field is malformed (e.g. a
/// rule_kind mismatch left the fields empty).
fn read_config_setting_info(
    cs_info: &analysis::CapturedProvider,
) -> Option<(Vec<String>, BTreeMap<String, String>)> {
    let mut constraints: Vec<String> = Vec::new();
    if let Some(v) = cs_info.fields.get("constraints") {
        let v = v.to_value();
        if let Some(list) = starlark::values::list::ListRef::from_value(v) {
            // Legacy shape: list-of-strings (CV labels).
            for item in list.iter() {
                if let Some(s) = item.unpack_str() {
                    constraints.push(s.to_string());
                }
            }
        } else if let Some(dict) = starlark::values::dict::DictRef::from_value(v) {
            // Vendored config_setting shape:
            // `{constraint_setting.label: constraint_value.label}`.
            // The values are the CV labels — exactly what
            // `compute_select_matches` needs to compare against
            // `target_platform_constraints`. A `None` value means
            // "this setting must be absent from the platform",
            // which Loupe doesn't model yet — those entries are
            // skipped (matches behaviour before the dict shape).
            //
            // Values may be bare strings or `Label` values (the
            // vendored impl emits `ctx.label` directly). Stringify
            // both to the canonical "@@…//pkg:name" form so
            // `Label::resolve` downstream sees the same shape.
            for (_setting, cv) in dict.iter() {
                if cv.is_none() {
                    continue;
                }
                let s = cv
                    .unpack_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| cv.to_string());
                constraints.push(s);
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
    repo_mapping: &BTreeMap<String, loupe_labels::CanonicalRepo>,
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
    repo_mapping: &BTreeMap<String, loupe_labels::CanonicalRepo>,
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
/// Read `<platform_label>`'s effective constraint set, including those
/// inherited from `parents=[...]`. Walks the parent chain depth-first;
/// each parent's direct constraint_values get added (deepest-first, so
/// later overrides win in the union view — good enough for v1 since we
/// treat the result as a set anyway and don't model per-setting
/// overriding).
///
/// Each CV is also inspected for its `constraint_setting` — those
/// settings land in `assigned_settings`, which the
/// [`constraint_is_satisfied`] fallback uses to decide whether the
/// platform "speaks to" a given setting. A candidate CV's setting that
/// the platform doesn't assign falls back to
/// `constraint_setting.default_constraint_value`.
///
/// STUB: Cycles in `parents=[...]` are detected and ignored (real
/// Bazel rejects cyclic platform inheritance; v1 silently
/// short-circuits).
async fn load_target_platform_constraints(
    ctx: &mut DiceComputations<'_>,
    platform_label: &Label,
    repo_mapping: &BTreeMap<String, loupe_labels::CanonicalRepo>,
) -> Option<PlatformConstraintSet> {
    let mut out = PlatformConstraintSet::default();
    let mut visited: std::collections::BTreeSet<Label> = std::collections::BTreeSet::new();
    let mut stack: Vec<Label> = vec![platform_label.clone()];
    let mut found_any = false;
    while let Some(cur) = stack.pop() {
        if !visited.insert(cur.clone()) {
            continue;
        }
        let Some((cvs, settings, parents)) = load_one_platform(ctx, &cur, repo_mapping).await
        else {
            continue;
        };
        found_any = true;
        out.cvs.extend(cvs);
        out.assigned_settings.extend(settings);
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

/// Load one `platform(...)` target, returning:
/// - its direct `constraint_values` (canonicalised),
/// - the `constraint_setting`s those CVs belong to (read from each
///   CV target's `constraint_setting` attr),
/// - and parent labels for the caller's traversal.
async fn load_one_platform(
    ctx: &mut DiceComputations<'_>,
    platform_label: &Label,
    repo_mapping: &BTreeMap<String, loupe_labels::CanonicalRepo>,
) -> Option<(
    std::collections::BTreeSet<Label>,
    std::collections::BTreeSet<Label>,
    Vec<Label>,
)> {
    let pkg = match Box::pin(ctx.compute(&PackageKey(platform_label.package.clone()))).await {
        Ok(SharedResult::Ok(p)) => p,
        _ => return None,
    };
    let target = pkg.targets.get(platform_label.name())?;
    if target.kind != "platform" {
        return None;
    }
    let mut cvs = std::collections::BTreeSet::new();
    let mut assigned_settings = std::collections::BTreeSet::new();
    let cv_strs = attr_as_label_strings(target.attrs.get("constraint_values"));
    // The Package borrow has to be dropped before we recurse via
    // `dereference_constraint_alias` / `read_constraint_value_setting`
    // (both compute on the same Dice context).
    drop(pkg);
    for s in cv_strs {
        if let Ok(l) = Label::resolve(&s, &platform_label.package, repo_mapping) {
            // Keep the label as written, then *also* try to
            // resolve `alias(actual=…)` and insert that. Covers
            // `@platforms//cpu:arm64` aliasing `:aarch64` so
            // toolchains using either spelling satisfy the check
            // without forcing candidate-side dereferencing.
            let aliased = Box::pin(dereference_constraint_alias(ctx, l.clone())).await;
            // Read the CV's setting so the default-value fallback can
            // tell whether the platform speaks to that setting at all.
            // Failures here are non-fatal — we just don't record the
            // setting, and the fallback never fires for it.
            let setting_label = Box::pin(read_constraint_value_setting(
                ctx,
                aliased.clone().unwrap_or_else(|| l.clone()),
            ))
            .await;
            cvs.insert(l);
            if let Some(canon) = aliased {
                cvs.insert(canon);
            }
            if let Some(setting) = setting_label {
                assigned_settings.insert(setting);
            }
        }
    }
    let pkg = match Box::pin(ctx.compute(&PackageKey(platform_label.package.clone()))).await {
        Ok(SharedResult::Ok(p)) => p,
        _ => return None,
    };
    let target = pkg.targets.get(platform_label.name())?;
    let mut parents = Vec::new();
    for s in attr_as_label_strings(target.attrs.get("parents")) {
        if let Ok(l) = Label::resolve(&s, &platform_label.package, repo_mapping) {
            parents.push(l);
        }
    }
    Some((cvs, assigned_settings, parents))
}

/// Read a `constraint_value(...)` target's `constraint_setting` attr
/// and return the canonical label of the setting. `None` when the CV
/// target can't be loaded or doesn't carry the attr (corrupt/synthetic
/// targets, or a label that pointed at something that wasn't a
/// `constraint_value` after all).
async fn read_constraint_value_setting(
    ctx: &mut DiceComputations<'_>,
    cv_label: Label,
) -> Option<Label> {
    let pkg = match Box::pin(ctx.compute(&PackageKey(cv_label.package.clone()))).await {
        Ok(SharedResult::Ok(p)) => p,
        _ => return None,
    };
    let target = pkg.targets.get(cv_label.name())?;
    if target.kind != "constraint_value" {
        return None;
    }
    let setting_str = attr_as_label_string(target.attrs.get("constraint_setting"))?;
    Label::resolve(&setting_str, &cv_label.package, &Default::default()).ok()
}

/// Read a `constraint_setting(...)` target's `default_constraint_value`
/// attr. `None` when the setting can't be loaded, isn't actually a
/// `constraint_setting`, or has no default declared.
async fn read_constraint_setting_default(
    ctx: &mut DiceComputations<'_>,
    setting_label: &Label,
) -> Option<Label> {
    let pkg = match Box::pin(ctx.compute(&PackageKey(setting_label.package.clone()))).await {
        Ok(SharedResult::Ok(p)) => p,
        _ => return None,
    };
    let target = pkg.targets.get(setting_label.name())?;
    if target.kind != "constraint_setting" {
        return None;
    }
    let default_str = attr_as_label_string(target.attrs.get("default_constraint_value"))?;
    if default_str.is_empty() {
        return None;
    }
    Label::resolve(&default_str, &setting_label.package, &Default::default()).ok()
}

/// Match-time check for one constraint label against the platform's
/// effective constraint set. Implements Bazel's three-step rule:
///
/// 1. Direct membership in the platform's explicit `cvs`.
/// 2. Alias-deref the candidate (resolves `@platforms//cpu:arm64` →
///    `@platforms//cpu:aarch64` etc.) and retry.
/// 3. If the candidate's `constraint_setting` is NOT in the platform's
///    `assigned_settings` (the platform doesn't speak to it), consult
///    the setting's `default_constraint_value`. The match succeeds iff
///    the default equals the candidate.
///
/// Step 3 is the `constraint_setting.default_constraint_value`
/// fallback: a platform that lists no CV for setting `S` implicitly
/// has `S.default_constraint_value`.
async fn constraint_is_satisfied(
    ctx: &mut DiceComputations<'_>,
    cv_label: &Label,
    plat: &PlatformConstraintSet,
) -> bool {
    if plat.cvs.contains(cv_label) {
        return true;
    }
    // Speculatively kick off the alias-deref AND the constraint_value
    // setting read in parallel via `compute2`. Both forces normally
    // hit the same PackageKey (`cv_label.package`) — DICE dedupes —
    // and when `cv_label` is a real `constraint_value` (the common
    // case), `setting_via_cv_label` is `Some` and we skip the later
    // retry. When `cv_label` IS an alias, the setting read returns
    // `None` (alias kind != `constraint_value`), and we fall back to
    // reading the setting from the dereffed `probe` below.
    let cv_a = cv_label.clone();
    let cv_b = cv_label.clone();
    let (deref, setting_via_cv_label) = ctx
        .compute2(
            move |c| {
                use futures::FutureExt;
                async move { dereference_constraint_alias(c, cv_a).await }.boxed()
            },
            move |c| {
                use futures::FutureExt;
                async move { read_constraint_value_setting(c, cv_b).await }.boxed()
            },
        )
        .await;
    tracing::debug!(cv=%cv_label, deref=?deref.as_ref().map(Label::to_string), plat_cvs=?plat.cvs.iter().map(Label::to_string).collect::<Vec<_>>(), "constraint_is_satisfied check");
    if let Some(ref canon) = deref
        && plat.cvs.contains(canon)
    {
        return true;
    }
    // Default-value fallback. Look up the CV's setting (or its
    // alias-deref'd canonical form), and only consult the default if
    // the platform doesn't speak to that setting at all.
    let setting = match setting_via_cv_label {
        Some(s) => s,
        None => {
            // `cv_label` was an alias; retry with the dereffed probe.
            let probe = match deref.clone() {
                Some(p) => p,
                None => return false,
            };
            match read_constraint_value_setting(ctx, probe).await {
                Some(s) => s,
                None => return false,
            }
        }
    };
    if plat.assigned_settings.contains(&setting) {
        return false;
    }
    match read_constraint_setting_default(ctx, &setting).await {
        Some(default_cv) => &default_cv == cv_label,
        None => false,
    }
}

/// Per-attr label dependency shape. Distinct from `ResolvedDep` —
/// this is the unresolved input; `ResolvedDep` is what
/// `AnalysisKey` produces for each.
enum DepLabels {
    Single(Label),
    List(Vec<Label>),
    /// `attr.label_keyed_string_dict` — keeps the per-key string
    /// value alongside the resolved Label. Preserves declaration
    /// order via the Vec (the underlying `BTreeMap` already sorts on
    /// `Label`, so this is a stable order regardless).
    KeyedDict(Vec<(Label, String)>),
}

/// Provider-forwarding alias analysis. Covers three kinds:
/// - `alias(name, actual=...)` reads its dep from `actual`.
/// - `label_flag` / `label_setting` (mirror Bazel
///   docs `extending/config#label-typed-build-settings`) read from
///   `build_setting_default`. They aren't `rule(build_setting=…)`
///   instances and don't return `BuildSettingInfo`; they're a kind of
///   alias that forwards providers from the target the flag points at.
///
/// The forwarded `AnalyzedTarget` is the source of truth — we relabel
/// it to the forwarding target's own `Label` and keep its
/// `analysis_heap` so the FrozenValues in `providers[].fields` stay
/// anchored. Mirrors Bazel: `bazel cquery //:alias --output=…` reports
/// the same providers as the aliased target.
async fn synthesise_alias_analysis(
    ctx: &mut DiceComputations<'_>,
    target: &crate::package::UnconfiguredTarget,
    config: &Configuration,
) -> Result<AnalyzedTarget, String> {
    use crate::package::AttrValue;
    let forward_attr = match target.kind.as_str() {
        "label_flag" | "label_setting" => "build_setting_default",
        _ => "actual",
    };
    let forwarded = match target.attrs.get(forward_attr) {
        Some(AttrValue::Label(l)) => l.clone(),
        Some(AttrValue::String(s)) => {
            match Label::resolve(s, &target.label.package, &Default::default()) {
                Ok(l) => l,
                Err(_) => return Ok(synthesise_source_file_analysis(target, config)),
            }
        }
        _ => {
            // Malformed (no forwarding label or non-label value) —
            // fall back to the synthetic-source-file path so the
            // graph still walks.
            return Ok(synthesise_source_file_analysis(target, config));
        }
    };
    let analyzed = force_analysis(ctx, forwarded, config).await?;
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
    )
    .with_layout(config.repository_layout());

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
            // `files_to_run` — `FilesToRunProvider`-shaped.
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
            crate::package::AttrValue::LabelKeyedStringDict(m) => {
                out.push((
                    attr_name.clone(),
                    DepLabels::KeyedDict(m.iter().map(|(l, s)| (l.clone(), s.clone())).collect()),
                ));
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

async fn force_exec_platform_constraints(
    ctx: &mut DiceComputations<'_>,
    config: &Configuration,
) -> Result<Arc<ExecPlatformConstraints>, String> {
    match ctx
        .compute(&ExecPlatformConstraintsKey(config.clone()))
        .await
    {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("ExecPlatformConstraintsKey: {e}")),
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

async fn force_file_content(
    ctx: &mut DiceComputations<'_>,
    path: PathBuf,
) -> Result<Arc<Vec<u8>>, String> {
    let display = path.display().to_string();
    match ctx.compute(&FileContentKey(path)).await {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("FileContentKey({display}): {e}")),
    }
}

/// Consult `loupe-bzlmod`'s lazy-fetch path for `canonical`.
///
/// Returns:
/// - `Ok(Some(root))` when a `RepoSpec` is registered and materialisation
///   succeeded — caller uses this `Materialised` root for file reads.
/// - `Ok(None)` when no `RepoSpec` is registered for this canonical;
///   caller falls through to the user-supplied `external_repo_root_for`.
/// - `Err` when a spec is registered but materialisation failed; this
///   propagates so users see the real cause instead of a misleading
///   "canonical not in any root" downstream.
async fn maybe_materialise(
    ctx: &mut DiceComputations<'_>,
    canonical: &str,
) -> Result<Option<crate::repo_root::RepoRoot>, String> {
    use loupe_bzlmod::{BzlmodResult, HasRepoSpecs, MaterializedRepoKey};
    if ctx.global_data().repo_spec(canonical).is_none() {
        return Ok(None);
    }
    match ctx
        .compute(&MaterializedRepoKey(canonical.to_string()))
        .await
    {
        Ok(BzlmodResult::Ok(path)) => Ok(Some(crate::repo_root::RepoRoot::Materialised {
            canonical: canonical.to_string(),
            dir: (*path).clone(),
        })),
        Ok(BzlmodResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("MaterializedRepoKey({canonical}): {e}")),
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
            if let Ok(Some((frozen, src))) =
                crate::builtins::embedded_module_with_source_by_path(candidate)
            {
                return Ok(LoadedBzl {
                    module: Arc::new(frozen),
                    source_digest: Arc::new(sha256_hex(src.as_bytes())),
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
    // Per-repo lookup: first the bzlmod lazy-fetch path (if a RepoSpec
    // is registered for this canonical), then user-supplied
    // `external_repo_root`s. Embedded roots come first via the
    // user-supplied list (the engine builder prepends them) so vendored
    // Bazel-side fixups stay reproducible.
    let repo_root = match maybe_materialise(ctx, label.package.repo.as_str()).await? {
        Some(r) => Some(r),
        None => ctx
            .global_data()
            .external_repo_root_for(label.package.repo.as_str())
            .cloned(),
    };
    let (ast, source) = load_bzl_ast(ctx, &root, repo_root.as_ref(), label).await?;
    let load_stmts = build_eval::extract_loads(&ast);
    // Resolve every load target synchronously first (no DICE call),
    // then fan out the transitive `BzlLoadKey` forces in parallel.
    // Independent siblings — DICE dedup's overlap across the wavefront.
    let resolved: Vec<(String, Label)> = load_stmts
        .iter()
        .map(|stmt| {
            let target = resolve_load(&stmt.module_id, &label.package, graph)
                .map_err(|e| format!("{e:#}"))?;
            Ok::<_, String>((stmt.module_id.clone(), target))
        })
        .collect::<Result<_, _>>()?;
    let loaded_pairs: Vec<(String, FrozenModule)> = ctx
        .try_compute_join(resolved, |ctx, (module_id, target)| {
            use futures::FutureExt;
            async move {
                let lb = force_bzl_load(ctx, target).await?;
                Ok::<_, String>((module_id, (*lb.module).clone()))
            }
            .boxed()
        })
        .await?;
    let loaded: std::collections::HashMap<String, FrozenModule> =
        loaded_pairs.into_iter().collect();
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
        source_digest: Arc::new(sha256_hex(source.as_bytes())),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

async fn compute_package(
    ctx: &mut DiceComputations<'_>,
    pkg: &PackageId,
    graph: &ModuleGraph,
) -> Result<Package, String> {
    // `@@_loupe_platforms//:` — CLI's `--platforms_expr=…` synthesis.
    // Treat the in-memory BUILD content as if it were a regular
    // `BUILD.bazel` at this package path: same load/eval pipeline,
    // same `Rule::invoke` recording into UnconfiguredTargets. The
    // engine guarantees only the root path under this repo is
    // populated; any other path under the reserved repo errors.
    if pkg.repo.as_str() == PLATFORMS_EXPR_REPO {
        return compute_platforms_expr_package(ctx, pkg, graph).await;
    }
    let root = ctx.global_data().workspace_root().to_path_buf();
    let repo_root = match maybe_materialise(ctx, pkg.repo.as_str()).await? {
        Some(r) => Some(r),
        None => ctx
            .global_data()
            .external_repo_root_for(pkg.repo.as_str())
            .cloned(),
    };
    let (_, ast) =
        build_eval::parse_build(&root, repo_root.as_ref(), pkg).map_err(|e| format!("{e:#}"))?;
    let load_stmts = build_eval::extract_loads(&ast);
    let resolved: Vec<(String, Label)> = load_stmts
        .iter()
        .map(|stmt| {
            let target = resolve_load(&stmt.module_id, pkg, graph).map_err(|e| format!("{e:#}"))?;
            Ok::<_, String>((stmt.module_id.clone(), target))
        })
        .collect::<Result<_, _>>()?;
    let loaded_entries: Vec<(String, Label, FrozenModule)> = ctx
        .try_compute_join(resolved, |ctx, (module_id, target)| {
            use futures::FutureExt;
            async move {
                let lb = force_bzl_load(ctx, target.clone()).await?;
                Ok::<_, String>((module_id, target, (*lb.module).clone()))
            }
            .boxed()
        })
        .await?;
    let loaded: std::collections::HashMap<String, (Label, FrozenModule)> = loaded_entries
        .into_iter()
        .map(|(id, target, frozen)| (id, (target, frozen)))
        .collect();
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
        repo_root,
        pkg.clone(),
        effective_mapping,
        ast,
        &load_stmts,
        &loaded,
    )
    .map_err(|e| format!("{e:#}"))
}

/// Evaluate the CLI-supplied `--platforms_expr` BUILD content as the
/// body of `@@_loupe_platforms//:BUILD.bazel`. Mirrors
/// [`compute_package`]'s pipeline (parse → resolve loads → evaluate)
/// but sources the BUILD text from global data instead of disk.
///
/// The package path must be empty (the root): only one synthesised
/// package per repo. The user's expression is required to declare
/// `platform(name = "current")`; downstream `target_platform =
/// @@_loupe_platforms//:current` then resolves through the regular
/// `load_target_platform_constraints` path.
async fn compute_platforms_expr_package(
    ctx: &mut DiceComputations<'_>,
    pkg: &PackageId,
    graph: &ModuleGraph,
) -> Result<Package, String> {
    use crate::build_eval;
    if !pkg.path.is_root() {
        return Err(format!(
            "@@{}//{}: synthesised platforms package only ships the \
             root path; got `{}`",
            PLATFORMS_EXPR_REPO,
            pkg.path.as_str(),
            pkg.path.as_str()
        ));
    }
    let expr = ctx
        .global_data()
        .platforms_expr()
        .ok_or_else(|| {
            format!(
                "@@{}//: package referenced but --platforms_expr was not set",
                PLATFORMS_EXPR_REPO
            )
        })?
        .to_string();
    let ast = starlark::syntax::AstModule::parse(
        "BUILD.bazel",
        expr,
        &starlark::syntax::Dialect::Standard,
    )
    .map_err(|e| format!("parsing --platforms_expr: {e}"))?;
    let load_stmts = build_eval::extract_loads(&ast);
    let resolved: Vec<(String, Label)> = load_stmts
        .iter()
        .map(|stmt| {
            let target = resolve_load(&stmt.module_id, pkg, graph).map_err(|e| format!("{e:#}"))?;
            Ok::<_, String>((stmt.module_id.clone(), target))
        })
        .collect::<Result<_, _>>()?;
    let loaded_entries: Vec<(String, Label, FrozenModule)> = ctx
        .try_compute_join(resolved, |ctx, (module_id, target)| {
            use futures::FutureExt;
            async move {
                let lb = force_bzl_load(ctx, target.clone()).await?;
                Ok::<_, String>((module_id, target, (*lb.module).clone()))
            }
            .boxed()
        })
        .await?;
    let loaded: std::collections::HashMap<String, (Label, FrozenModule)> = loaded_entries
        .into_iter()
        .map(|(id, target, frozen)| (id, (target, frozen)))
        .collect();
    // Synthesised packages don't have apparent-name imports of their
    // own — they live outside the bzlmod graph — so the effective
    // mapping is just main's. That suffices for typical
    // `platform(constraint_values=["@platforms//os:macos", ...])`
    // declarations (the labels canonicalise via main's repo_mapping).
    let mapping = graph.repo_mapping.clone();
    // BUILD evaluator wants a workspace root to anchor file paths
    // against. The synthesised package isn't backed by a real
    // directory; pass workspace_root() — the BUILD body only declares
    // targets and references labels, so the root never gets touched
    // by anything that'd dereference a file.
    let root = ctx.global_data().workspace_root().to_path_buf();
    // The synthesised `@@_loupe_platforms` repo doesn't live on disk
    // — `repo_root = None` is fine; the BUILD body only declares
    // targets and references labels (no glob, no file reads).
    build_eval::evaluate(&root, None, pkg.clone(), mapping, ast, &load_stmts, &loaded)
        .map_err(|e| format!("--platforms_expr: {e}"))
}

/// Resolve a `.bzl` label to an `AstModule`. Reads from the workspace
/// directly for the main repo (routed through `FileContentKey` so
/// edits invalidate downstream) and through the [`RepoRoot`] enum for
/// external repos (handles both embedded and filesystem variants).
async fn load_bzl_ast(
    ctx: &mut DiceComputations<'_>,
    workspace_root: &Path,
    repo_root: Option<&crate::repo_root::RepoRoot>,
    label: &Label,
) -> Result<(starlark::syntax::AstModule, String), String> {
    if !label.name().ends_with(".bzl") {
        return Err(format!(
            "BzlLoadKey label must end in .bzl (got `{}`)",
            label
        ));
    }
    if label.repo().is_main() {
        let mut path = workspace_root.to_path_buf();
        if !label.package_path().is_root() {
            path.push(label.package_path().as_str());
        }
        path.push(label.name());
        let bytes = force_file_content(ctx, path.clone()).await?;
        let src = String::from_utf8((*bytes).clone())
            .map_err(|e| format!("loading `{label}`: source is not UTF-8: {e}"))?;
        let filename = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let display_origin = path.display().to_string();
        let ast = bzl_eval::parse_source(src.clone(), &filename, &display_origin)
            .map_err(|e| format!("{e:#}"))?;
        return Ok((ast, src));
    }
    let root = repo_root.ok_or_else(|| {
        format!(
            "loading `{}`: external repo `{}` not found under any \
             `--external-repo-root` (vendor with `bazel mod vendor` \
             and/or add `<output_base>/external` to the search path)",
            label,
            label.repo()
        )
    })?;
    let canonical = label.repo().as_str();
    let rel = if label.package_path().is_root() {
        PathBuf::from(label.name())
    } else {
        Path::new(label.package_path().as_str()).join(label.name())
    };
    let bytes = root.read(canonical, &rel).ok_or_else(|| {
        format!(
            "loading `{}`: `{}` not present under repo root for `{}`",
            label,
            rel.display(),
            canonical
        )
    })?;
    let src = String::from_utf8(bytes)
        .map_err(|e| format!("loading `{label}`: source is not UTF-8: {e}"))?;
    let origin = format!(
        "{canonical}//{}:{}",
        label.package_path().as_str(),
        label.name()
    );
    let ast =
        bzl_eval::parse_source(src.clone(), label.name(), &origin).map_err(|e| format!("{e:#}"))?;
    Ok((ast, src))
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
