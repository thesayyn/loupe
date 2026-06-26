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
/// directory (today: the bzlmod cache directory the in-process
/// resolver materialises repos into). Label loads targeting non-main
/// repos (`@<canonical>//pkg:foo`) resolve to the first root that
/// reports `has(canonical) == true`. Search order matches the order
/// [`SetExternalRepoRoots::set_external_repo_roots`] was called with.
///
/// Convention: the engine prepends embedded roots first, then appends
/// the bzlmod cache as a filesystem root. Embedded wins over disk so
/// embedded Bazel-side fixups (e.g. `@bazel_tools//command_line_option`)
/// stay reproducible regardless of what the resolver has cached.
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

    #[tracing::instrument(name = "FileContent", skip(self, _ctx), fields(path = %self.0.display()))]
    async fn compute(&self, _ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        // tokio::fs delegates to the blocking pool but releases the
        // DICE worker thread so other keys can progress. Sync
        // `std::fs::read` here serialises every concurrent file read
        // through the few-thread DICE executor.
        match tokio::fs::read(&self.0).await {
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

/// POSIX file metadata, narrowed to the bits Loupe actually consumes.
/// Today that's just `is_executable`: REAPI's `FileNode.is_executable`
/// reads `mode & 0o111 != 0` on Unix; Bazel inherits that semantic when
/// building input merkle trees. `exists` is surfaced so callers can
/// distinguish a missing source file from a present non-executable one
/// without re-stat'ing.
///
/// Cross-platform: Windows has no Unix `+x` bit. `is_executable` is
/// `false` on non-Unix until either a `.exe`/`.bat`-suffix heuristic
/// or a WSL-aware mode plumbing lands — same gap Bazel has on those
/// hosts. Documented STUB.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct FileMetadata {
    pub exists: bool,
    pub is_executable: bool,
}

impl FileMetadata {
    pub const MISSING: FileMetadata = FileMetadata {
        exists: false,
        is_executable: false,
    };
}

/// Source-file metadata routed through DICE. Mirrors `FileContentKey`'s
/// shape so a future file-watcher invalidates the same way for content
/// and mode changes alike. Read via [`force_file_metadata`].
///
/// Source files only. Generated outputs carry the bit explicitly on the
/// `File` they were declared with (`ctx.actions.write(is_executable=True)`
/// flows through `File::with_is_executable`).
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("FileMetadataKey({})", _0.display())]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct FileMetadataKey(pub PathBuf);

#[async_trait]
impl Key for FileMetadataKey {
    type Value = SharedResult<FileMetadata>;

    async fn compute(&self, _ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        match tokio::fs::metadata(&self.0).await {
            Ok(m) => {
                #[cfg(unix)]
                let is_executable = {
                    use std::os::unix::fs::PermissionsExt;
                    m.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                // STUB: no POSIX +x on Windows. Bazel itself falls back
                // to `.exe`/`.bat`-suffix detection on those hosts; we
                // leave the bit false for now and treat as a follow-up.
                let is_executable = false;
                SharedResult::ok(FileMetadata {
                    exists: true,
                    is_executable,
                })
            }
            // `NotFound` is a normal case — auto-discovered source-file
            // labels reference paths that may legitimately not exist
            // (deleted file, never-staged label). Surface as
            // `MISSING` rather than an error so the synthetic-target
            // path keeps working.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                SharedResult::ok(FileMetadata::MISSING)
            }
            Err(e) => SharedResult::err(format!("stat {}: {e}", self.0.display())),
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

/// Session-global singleton: the empty `DefaultInfo()` instance, minted
/// once through the vendored provider and shared by every target whose
/// impl returned no `DefaultInfo`. The key has no parameters, so DICE
/// computes it exactly once and never invalidates (the mint reads only
/// embedded builtins). See [`analysis::mint_default_info`].
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("EmptyDefaultInfoKey")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct EmptyDefaultInfoKey;

#[async_trait]
impl Key for EmptyDefaultInfoKey {
    type Value = SharedResult<analysis::EmptyDefaultInfo>;

    async fn compute(&self, _ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        match analysis::mint_default_info(Vec::new()) {
            Ok((heap, provider)) => SharedResult::ok(analysis::EmptyDefaultInfo { heap, provider }),
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

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ModuleGraphKey")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ModuleGraphKey;

#[async_trait]
impl Key for ModuleGraphKey {
    type Value = SharedResult<ModuleGraph>;

    #[tracing::instrument(name = "ModuleGraph", skip_all)]
    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let root = ctx.global_data().workspace_root().to_path_buf();
        let lockfile_override = ctx
            .global_data()
            .bzlmod_lockfile_path()
            .map(|p| p.to_path_buf());

        // Preferred path: in-memory `BzlmodGraph` snapshot injected via
        // `BzlmodGraphKey`. Everything `ModuleGraphKey` needs to know
        // lives in there — no `read_dir`, no race against lazy
        // materialisation, no walk over `.loupe/bzlmod/`. Recording
        // the dep edge on `BzlmodGraphKey` means a later
        // `inject_bzlmod_graph` (e.g. when extension eval produces a
        // new ext-generated repo) invalidates this key — and via this
        // key, every downstream `RepoMappingKey` — structurally.
        match loupe_bzlmod::force_bzlmod_graph(ctx).await {
            Ok(Some(bzlmod_graph)) => {
                return match module_graph::synthesise_module_graph(
                    &root,
                    &bzlmod_graph,
                    lockfile_override,
                ) {
                    Ok(g) => SharedResult::ok(g),
                    Err(e) => SharedResult::err(format!("{e:#}")),
                };
            }
            Ok(None) => {
                // Fall through to the test-fallback path below.
            }
            Err(e) => return SharedResult::err(e),
        }

        // Fallback for callers that build `Dice` directly without
        // going through `EngineBuilder` (tests, mainly). Parses
        // `MODULE.bazel` against the lockfile only; transitive module
        // info, ext-generated repos, etc. all live in the
        // `BzlmodGraph` snapshot the engine builder injects.
        //
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
        match module_graph::parse_with_pre_read_inputs(&root, &module_bazel_src, lockfile) {
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

    #[tracing::instrument(name = "BzlLoad", skip(self, ctx), fields(label = %self.0))]
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

    #[tracing::instrument(name = "Package", skip(self, ctx), fields(package = %self.0))]
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

    #[tracing::instrument(name = "DiscoverPackages", skip_all)]
    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let root = ctx.global_data().workspace_root().to_path_buf();
        // Don't walk filesystem-backed external roots when they live
        // inside the workspace (the bzlmod cache at
        // `<workspace>/.loupe/bzlmod`). Otherwise every cached BUILD
        // gets attributed to the main module as
        // `@@//.loupe/bzlmod/<canonical>/...`, producing ghost-target
        // failures during analysis. Embedded roots have no disk
        // presence and are irrelevant to the skip list.
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
    /// REAPI `Platform.properties` from the platform target's
    /// `exec_properties = {...}` attr. Loupe surfaces it on the
    /// per-target `Configuration` clone in `compute_analysis`; consumers
    /// (AC-key emitters) read it back via
    /// [`crate::Configuration::exec_properties`]. Inherited from
    /// `parents=[...]` via union — parent entries land first, child
    /// entries override on key collisions (BTreeMap insert semantics).
    pub exec_properties: BTreeMap<String, String>,
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

    #[tracing::instrument(name = "TargetPlatformConstraints", skip_all)]
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

    #[tracing::instrument(name = "ExecPlatformConstraints", skip_all)]
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

    #[tracing::instrument(name = "MaterialisedAttrs", skip(self, ctx), fields(label = %self.0))]
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

    #[tracing::instrument(name = "ResolvedToolchains", skip(self, ctx), fields(label = %self.0))]
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

/// Identity of an aspect across a DICE-key boundary — the `.bzl` that
/// ran `aspect(...)` plus the binding symbol it was assigned to. Mirrors
/// how a rule is recovered (`defining_bzl` + `source_symbol`). Both are
/// stamped on `FrozenAspect` at `.bzl` freeze (see `bzl_eval`).
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("AspectRef({symbol} @ {defining_bzl})")]
pub struct AspectRef {
    pub defining_bzl: Label,
    pub symbol: String,
}

/// Recover an `AspectRef` from a frozen `aspect()` value — used to turn a
/// `FrozenAspect.requires` entry (a `FrozenValue`) into a forceable key.
fn aspect_ref_from_value(v: starlark::values::FrozenValue) -> Result<AspectRef, String> {
    use crate::starlark_types::aspect::FrozenAspect;
    use starlark::values::ValueLike;
    let a = v
        .to_value()
        .downcast_ref::<FrozenAspect>()
        .ok_or_else(|| format!("expected an aspect value, got {}", v.to_value().get_type()))?;
    let defining_bzl = a
        .defining_bzl
        .get()
        .cloned()
        .ok_or_else(|| "aspect missing stamped defining_bzl".to_string())?;
    let symbol = a
        .symbol
        .get()
        .cloned()
        .ok_or_else(|| "aspect missing stamped symbol".to_string())?;
    Ok(AspectRef {
        defining_bzl,
        symbol,
    })
}

/// Load the defining `.bzl` and downcast the stamped symbol to a
/// `FrozenAspect`, returning it plus the heap that owns it (so callers
/// can anchor it). Mirrors `read_rule_attr_specs`'s bzl-load path.
async fn recover_aspect(
    ctx: &mut DiceComputations<'_>,
    aspect: &AspectRef,
) -> Result<(starlark::values::FrozenValue, Arc<FrozenModule>), String> {
    use starlark::values::ValueLike;
    let bzl = force_bzl_load(ctx, aspect.defining_bzl.clone()).await?;
    let (owned, _vis) = bzl
        .module
        .get_any_visibility(&aspect.symbol)
        .map_err(|e| format!("looking up aspect `{}`: {e}", aspect.symbol))?;
    // SAFETY: `fv` is returned alongside `Arc::clone(&bzl.module)` (below),
    // so the caller holds the frozen module that owns `fv`'s heap alive for
    // as long as it keeps the value.
    let fv = unsafe { owned.unchecked_frozen_value() };
    if fv
        .to_value()
        .downcast_ref::<crate::starlark_types::aspect::FrozenAspect>()
        .is_none()
    {
        return Err(format!(
            "`{}` is not an aspect (got {})",
            aspect.symbol,
            fv.to_value().get_type()
        ));
    }
    Ok((fv, Arc::clone(&bzl.module)))
}

/// Apply one aspect to one target+config. Forces the base target's
/// analysis, recursively applies the aspect's `requires=` aspects (which
/// run first on the same target), propagates the aspect along its
/// `attr_aspects` edges, resolves the aspect's toolchains, then runs the
/// aspect impl via `analysis::run_aspect`.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("AspectKey({}, {_1}, {_2})", _0.symbol)]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct AspectKey(pub AspectRef, pub Label, pub Configuration);

#[async_trait]
impl Key for AspectKey {
    type Value = SharedResult<analysis::AspectApplication>;

    #[tracing::instrument(name = "Aspect", skip(self, ctx), fields(aspect = %self.0.symbol, label = %self.1))]
    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        match compute_aspect(ctx, &self.0, &self.1, &self.2).await {
            Ok(v) => SharedResult::ok(v),
            Err(e) => SharedResult::err(e),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
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

async fn force_aspect(
    ctx: &mut DiceComputations<'_>,
    aspect: AspectRef,
    base: Label,
    config: &Configuration,
) -> Result<Arc<analysis::AspectApplication>, String> {
    match Box::pin(ctx.compute(&AspectKey(aspect.clone(), base.clone(), config.clone()))).await {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("AspectKey({}, {base}): {e}", aspect.symbol)),
    }
}

async fn compute_aspect(
    ctx: &mut DiceComputations<'_>,
    aspect: &AspectRef,
    base_label: &Label,
    config: &Configuration,
) -> Result<analysis::AspectApplication, String> {
    use crate::starlark_types::aspect::FrozenAspect;
    use starlark::values::ValueLike;

    // a. Recover the FrozenAspect + its heap.
    let (aspect_fv, aspect_heap) = recover_aspect(ctx, aspect).await?;
    // Read the aspect's metadata out of the frozen value into owned
    // Rust data so we don't hold a `Value<'v>` across the awaits below.
    let (
        aspect_impl,
        attr_aspects,
        requires_refs,
        aspect_toolchains,
        aspect_fragments,
        aspect_attrs,
        required_providers,
    ) = {
        let a = aspect_fv
            .to_value()
            .downcast_ref::<FrozenAspect>()
            .ok_or_else(|| "recovered aspect is not a FrozenAspect".to_string())?;
        let mut requires_refs = Vec::with_capacity(a.requires.len());
        for r in &a.requires {
            requires_refs.push(aspect_ref_from_value(*r)?);
        }
        (
            a.implementation,
            a.attr_aspects.clone(),
            requires_refs,
            a.toolchains.clone(),
            a.fragments.clone(),
            a.attrs.0.clone(),
            a.required_providers.clone(),
        )
    };

    // b. Force the base target's analysis.
    let base = force_analysis(ctx, base_label.clone(), config).await?;

    // Node-visit gate (Bazel's OR-of-AND-sets): the aspect runs its impl
    // on a node only if the node carries *every* provider in *at least one*
    // required AND-set. Matched by full provider-id string
    // (`<bzl>%Symbol`), not bare symbol — two `Info`s from different `.bzl`s
    // are distinct keys, so the gate can't misfire on a same-named provider
    // from the wrong file (the proto aspect must see protobuf's real
    // `ProtoInfo`, not any `ProtoInfo`). Without the gate the proto aspect —
    // propagated along every `deps` edge — would run `target[ProtoInfo]` on
    // non-proto nodes and fail. Skipped nodes return an empty application;
    // the aspect still propagates onward.
    if !required_providers.is_empty() {
        let node_ids: std::collections::HashSet<String> =
            base.providers.iter().map(|p| p.id.to_string()).collect();
        let visitable = required_providers
            .iter()
            .any(|and_set| and_set.iter().all(|id| node_ids.contains(id)));
        if !visitable {
            return Ok(analysis::AspectApplication {
                providers: Vec::new(),
                actions: Vec::new(),
                heap: Arc::clone(&base.analysis_heap),
            });
        }
    }

    // c. Required aspects run first on the SAME target.
    let mut required = Vec::with_capacity(requires_refs.len());
    for req in &requires_refs {
        required.push(force_aspect(ctx, req.clone(), base_label.clone(), config).await?);
    }

    // d. Propagation: re-resolve the base's deps, then apply THIS aspect
    // to the deps reached through each `attr_aspects` edge. The resulting
    // aspect-applied deps replace the plain deps in `resolved_deps` so
    // `ctx.rule.attr.<edge>[i][AspectProvider]` resolves.
    let m_attrs = force_materialised_attrs(ctx, base_label, config).await?;
    let (base_bzl_label, base_source_symbol) = {
        let pkg = force_package(ctx, &base_label.package).await?;
        match pkg.targets.get(base_label.name()) {
            Some(t) => match (&t.bzl_label, &t.source_symbol) {
                (Some(l), Some(s)) => (Some(l.clone()), Some(s.clone())),
                _ => (None, None),
            },
            None => (None, None),
        }
    };
    let attrs_schema = match &base_bzl_label {
        Some(l) => {
            let bzl = force_bzl_load(ctx, l.clone()).await?;
            read_rule_attr_specs(&bzl.module, base_source_symbol.as_deref().unwrap_or(""))
        }
        None => BTreeMap::new(),
    };
    let mut resolved_deps =
        resolve_label_deps(ctx, &m_attrs.0, &attrs_schema, config, false).await?;
    if !attr_aspects.is_empty() {
        for edge in &attr_aspects {
            let Some(dep) = resolved_deps.get(edge) else {
                continue;
            };
            // Collect the dep labels + their current AnalyzedTargets.
            let targets: Vec<Arc<AnalyzedTarget>> = dep.iter_targets().cloned().collect();
            // Fan the aspect out over every dep this edge reaches in
            // parallel — each `force_aspect` is an independent DICE key, and
            // `try_compute_join` preserves input order, so the zip-augment
            // below stays aligned with `targets`. (`Label`/`Configuration`
            // clones are refcount bumps, so bundling per-item is cheap.)
            let items: Vec<(AspectRef, Label, Configuration)> = targets
                .iter()
                .map(|t| (aspect.clone(), t.label.clone(), t.configuration.clone()))
                .collect();
            let apps: Vec<Arc<analysis::AspectApplication>> = ctx
                .try_compute_join(items, |ctx, (asp, label, cfg)| {
                    use futures::FutureExt;
                    async move { force_aspect(ctx, asp, label, &cfg).await }.boxed()
                })
                .await?;
            let applied: Vec<Arc<AnalyzedTarget>> = targets
                .iter()
                .zip(apps)
                .map(|(t, app)| {
                    analysis::augment_dep_with_aspects(t, &[app]).map_err(|e| format!("{e:#}"))
                })
                .collect::<Result<_, _>>()?;
            // Rebuild the ResolvedDep preserving its shape.
            let replacement = match dep {
                ResolvedDep::Single(_) => ResolvedDep::Single(applied.into_iter().next().unwrap()),
                ResolvedDep::List(_) => ResolvedDep::List(applied),
                ResolvedDep::Dict(entries) => {
                    let strs: Vec<(Label, String)> = entries
                        .iter()
                        .map(|(l, _, s)| (l.clone(), s.clone()))
                        .collect();
                    ResolvedDep::Dict(
                        strs.into_iter()
                            .zip(applied)
                            .map(|((l, s), at)| (l, at, s))
                            .collect(),
                    )
                }
            };
            resolved_deps.insert(edge.clone(), replacement);
        }
    }

    // e. Resolve the aspect's own declared toolchains under `config`.
    let resolved_toolchains = {
        let graph = force_module_graph(ctx).await?;
        let plat = force_target_platform_constraints(ctx, config).await?;
        let exec = force_exec_platform_constraints(ctx, config).await?;
        resolve_declared_toolchains(
            ctx,
            &aspect_toolchains,
            &[],
            &aspect.defining_bzl.package,
            config,
            &graph,
            plat.0.as_ref(),
            exec.0.as_ref(),
        )
        .await?
    };

    // f. Run the aspect impl.
    let graph = force_module_graph(ctx).await?;
    analysis::run_aspect(
        &base,
        &required,
        aspect_impl,
        aspect_heap,
        &aspect_attrs,
        &aspect_fragments,
        &m_attrs.0,
        &attrs_schema,
        &resolved_deps,
        resolved_toolchains,
        config,
        &graph.main.name,
    )
    .map_err(|e| format!("{e:#}"))
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

    #[tracing::instrument(name = "Analysis", skip(self, ctx), fields(label = %self.0))]
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
            // Bazel auto-discovers source files referenced by label
            // even without an `exports_files(...)`. v1 synthesises an
            // UnconfiguredTarget on the fly so the analysis chain
            // returns a source-file DefaultInfo. The on-disk POSIX
            // `+x` bit flows through `FileMetadataKey` so the
            // synthetic `File` carries `is_executable` matching what
            // Bazel sees when staging the input merkle tree.
            let synthetic = crate::package::UnconfiguredTarget {
                label: target_label.clone(),
                kind: "source_file".to_string(),
                bzl_source: "<auto-discovered>".to_string(),
                bzl_label: None,
                source_symbol: None,
                attrs: std::collections::BTreeMap::new(),
            };
            let is_executable = source_file_is_executable(ctx, target_label).await;
            return synthesise_source_file_analysis(&synthetic, config, is_executable);
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

    // Native loading-only kinds — record-only, never invoke an impl.
    // We short-circuit by kind even when a wrapper `.bzl` happens to
    // assign a `bzl_label` (Loupe's `builtins_core/exports.bzl`
    // re-exports `constraint_setting` etc. through Starlark stubs).
    // Going through the full impl path for these is wrong on two
    // axes: there's no real impl to invoke, and they form natural
    // analysis-time cycles (`constraint_setting.default_constraint_value`
    // ↔ `constraint_value.constraint_setting`) that DICE's cycle
    // predicate misses because the dep edge is forced from inside
    // `resolve_label_deps`, not declared upfront. Surface a synthetic
    // single-File DefaultInfo so `bazel cquery` shape is preserved.
    if matches!(
        target.kind.as_str(),
        "constraint_setting" | "constraint_value" | "platform" | "toolchain" | "toolchain_type",
    ) {
        return synthesise_source_file_analysis(target, config, false);
    }

    // `package_group` is loading-only, but the cc toolchain (and
    // `py_internal.is_available_for`) read `target[PackageSpecificationInfo]`
    // off allowlist attrs that point at one. Surface an empty DefaultInfo
    // plus a permissive PackageSpecificationInfo instead of the bare
    // source-file fallback.
    if target.kind.as_str() == "package_group" {
        return synthesise_package_group_analysis(target, config);
    }

    let (bzl_label, source_symbol) = match (&target.bzl_label, &target.source_symbol) {
        (Some(l), Some(s)) => (l.clone(), s.clone()),
        // Other native loading-only kinds with no `.bzl` wrapper
        // (e.g. native targets we haven't enumerated). Same
        // synthetic-source-file fallback as the kind-list above.
        _ => return synthesise_source_file_analysis(target, config, false),
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
                    let is_analysis_test =
                        read_rule_is_analysis_test(&bzl.module, &source_symbol_for_chain);
                    let resolved_deps =
                        resolve_label_deps(c, &m_attrs.0, &attrs_schema, config, is_analysis_test)
                            .await?;
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
    // Merge the configured target platform's `exec_properties` onto
    // a per-target Configuration clone. Platform-side entries land
    // first; the engine-level Configuration's
    // `with_exec_properties(...)` overlay (if any) wins on key
    // collision — matches Bazel's "platform default, command-line
    // override" precedence.
    let merged_config = merge_platform_exec_properties(config, plat_constraints.0.as_ref());
    let empty_default_info = force_empty_default_info(ctx).await?;
    let result = analysis::run_analysis(
        target,
        &m_attrs.0,
        Arc::clone(&bzl.module),
        &source_symbol,
        &graph.main.name,
        &merged_config,
        &resolved_deps,
        (*toolchains).clone(),
        plat_constraints.0.as_ref().map(|s| Arc::new(s.cvs.clone())),
        exec_constraints.0.as_ref().map(|s| Arc::new(s.cvs.clone())),
        &empty_default_info,
    )
    .map_err(|e| format!("{e:#}"));
    match result {
        Ok(at) => Ok(at),
        // `--allow_analysis_failures`: when the configuration carries the
        // flag (set by analysistest's `analysis_test_transition` on a
        // failing subject), capture the failure into `AnalysisFailureInfo`
        // instead of propagating — Bazel's contract that lets a test read
        // `dep[AnalysisFailureInfo]`. Off by default, so non-test analysis
        // still fails loudly.
        Err(e) if config_allows_analysis_failures(&merged_config) => {
            analysis::synthesise_analysis_failure_target(target_label, &merged_config, &e)
                .map(Arc::unwrap_or_clone)
                .map_err(|e| format!("{e:#}"))
        }
        Err(e) => Err(e),
    }
}

/// Whether `--//command_line_option:allow_analysis_failures` is truthy in
/// this configuration. Keyed by the flag's target name (the Bazel
/// convention for `--<flag>=…` overrides).
fn config_allows_analysis_failures(config: &Configuration) -> bool {
    matches!(
        config
            .build_settings()
            .get("allow_analysis_failures")
            .map(String::as_str),
        Some("True" | "true" | "1")
    )
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
    // Use the target's *defining* repo's effective mapping for select
    // branch keys / label attrs — the strings were written from that
    // BUILD's perspective. Passing main's mapping was a latent bug
    // that happened to work via `Label::resolve`'s `<apparent>+`
    // fallback on v1 fixtures.
    let target_mapping = graph.effective_repo_mapping_cached(&target_label.package.repo);
    let select_matches = compute_select_matches(
        ctx,
        &target_attrs,
        &target_label.package,
        &target_mapping,
        config,
        plat_constraints.0.as_ref(),
    )
    .await?;
    let mut materialised = materialise_attrs(
        &target_attrs,
        &target_label.package,
        &target_mapping,
        &select_matches,
    )
    .map_err(|e| format!("{e:#}"))?;

    // Fill in rule-schema defaults for attrs the BUILD invocation
    // didn't set. Bazel materialises `ctx.attr.<name>` to the attr's
    // declared default when unset — e.g. `attr.string_dict()` →
    // `{}`. Without this, `ctx.attr.execution_requirements` (bazel_lib
    // run_binary) is None and `dicts.add(None)` fails. Only fills
    // *missing* keys; target-supplied values (already in
    // `materialised`) win.
    let (bzl_label, source_symbol) = match (&target.bzl_label, &target.source_symbol) {
        (Some(l), Some(s)) => (l.clone(), s.clone()),
        _ => return Ok(MaterialisedAttrs(materialised)),
    };
    let bzl = force_bzl_load(ctx, bzl_label).await?;
    let attrs_schema = read_rule_attr_specs(&bzl.module, &source_symbol);
    for (name, spec) in &attrs_schema {
        // Missing OR explicit `None` falls back to the declared
        // default. Macros routinely forward optional kwargs as
        // `attr = None` (bazel_lib's `run_binary` does
        // `execution_requirements = None`); Bazel coerces that to the
        // typed default (`attr.string_dict()` → `{}`). Without this,
        // `ctx.attr.execution_requirements` is None and
        // `dicts.add(None)` fails.
        let needs_default = match materialised.get(name) {
            None | Some(crate::package::AttrValue::None) => true,
            Some(_) => false,
        };
        if needs_default {
            let default = spec.default_attr_value();
            // Only override a present `None` when the default is a
            // non-None container/value — a label attr that genuinely
            // defaults to None should stay None.
            if !matches!(default, crate::package::AttrValue::None)
                || !materialised.contains_key(name)
            {
                materialised.insert(name.clone(), default);
            }
        }
    }
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
            optional: Arc::new(std::collections::BTreeSet::new()),
        });
    };
    let (bzl_label, source_symbol) = match (&target.bzl_label, &target.source_symbol) {
        (Some(l), Some(s)) => (l.clone(), s.clone()),
        _ => {
            return Ok(Toolchains {
                by_type: BTreeMap::new(),
                repo_mapping: Arc::new(graph.repo_mapping.clone()),
                optional: Arc::new(std::collections::BTreeSet::new()),
            });
        }
    };
    drop(pkg);

    let bzl = force_bzl_load(ctx, bzl_label.clone()).await?;
    let declared = read_rule_toolchain_types(&bzl.module, &source_symbol);
    let optional = read_rule_optional_toolchain_types(&bzl.module, &source_symbol);
    let graph = force_module_graph(ctx).await?;
    let plat_constraints = force_target_platform_constraints(ctx, config).await?;
    let exec_constraints = force_exec_platform_constraints(ctx, config).await?;
    resolve_declared_toolchains(
        ctx,
        &declared,
        &optional,
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
/// Force every aspect in `refs` on `dep` and fold their providers/actions
/// onto it. No-op (returns `dep` unchanged) when `refs` is empty.
async fn apply_aspects(
    ctx: &mut DiceComputations<'_>,
    dep: Arc<AnalyzedTarget>,
    cfg: &Configuration,
    refs: &[AspectRef],
) -> Result<Arc<AnalyzedTarget>, String> {
    if refs.is_empty() {
        return Ok(dep);
    }
    let mut apps = Vec::with_capacity(refs.len());
    for r in refs {
        apps.push(force_aspect(ctx, r.clone(), dep.label.clone(), cfg).await?);
    }
    analysis::augment_dep_with_aspects(&dep, &apps).map_err(|e| format!("{e:#}"))
}

async fn resolve_label_deps(
    ctx: &mut DiceComputations<'_>,
    materialised_attrs: &BTreeMap<String, crate::package::AttrValue>,
    attrs_schema: &BTreeMap<String, crate::starlark_types::attr::FrozenAttrSpec>,
    config: &Configuration,
    // When the consuming rule is `rule(analysis_test=True)`, a dep that
    // fails analysis is captured into an `AnalysisFailureInfo` stand-in
    // rather than propagated (Bazel's `--allow_analysis_failures`).
    analysis_test: bool,
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
        // `attr.label(aspects=[…])` — aspects applied to every dep of
        // this attr. Recovered to forceable `AspectRef`s here; each dep's
        // resolved target is augmented with the aspect applications below
        // so `ctx.attr.<attr>[Provider]` sees aspect providers.
        let aspect_refs: Vec<AspectRef> = match schema {
            Some(s) if !s.aspects.is_empty() => {
                let mut refs = Vec::with_capacity(s.aspects.len());
                for a in &s.aspects {
                    refs.push(aspect_ref_from_value(*a)?);
                }
                refs
            }
            _ => Vec::new(),
        };
        match kind {
            DepLabels::Single(label) => {
                let dep = match force_analysis(ctx, label.clone(), &dep_config).await {
                    Ok(d) => apply_aspects(ctx, d, &dep_config, &aspect_refs).await?,
                    Err(e) if analysis_test => {
                        // Capture the subject's failure into an
                        // AnalysisFailureInfo stand-in instead of
                        // propagating — aspects are skipped (they'd fail
                        // on the same broken target).
                        analysis::synthesise_analysis_failure_target(&label, &dep_config, &e)
                            .map_err(|e| format!("{e:#}"))?
                    }
                    Err(e) => return Err(e),
                };
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
                let mut out: Vec<Arc<AnalyzedTarget>> = ctx
                    .try_compute_join(items, |ctx, (label, cfg)| {
                        use futures::FutureExt;
                        async move { force_analysis(ctx, label, &cfg).await }.boxed()
                    })
                    .await?;
                // ponytail: aspect application is sequential (the
                // `try_compute_join` closure must be `Copy` and can't
                // carry `aspect_refs`). Parallelise if a label_list with
                // aspects ever dominates a profile.
                if !aspect_refs.is_empty() {
                    for d in out.iter_mut() {
                        *d = apply_aspects(ctx, Arc::clone(d), &dep_config, &aspect_refs).await?;
                    }
                }
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
                let mut deps: Vec<Arc<AnalyzedTarget>> = ctx
                    .try_compute_join(items, |ctx, (label, cfg)| {
                        use futures::FutureExt;
                        async move { force_analysis(ctx, label, &cfg).await }.boxed()
                    })
                    .await?;
                if !aspect_refs.is_empty() {
                    for d in deps.iter_mut() {
                        *d = apply_aspects(ctx, Arc::clone(d), &dep_config, &aspect_refs).await?;
                    }
                }
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
/// toolchains (the rule impl only fails if it actually reads
/// `ctx.toolchains[T]`, which raises on the empty map).
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

/// The `mandatory = False` subset of a rule's declared toolchain types
/// (`rule(toolchains=[config_common.toolchain_type(T, mandatory=False)])`).
/// Used to decide which unresolved declared types yield `None` from
/// `ctx.toolchains[T]` rather than raising.
fn read_rule_optional_toolchain_types(bzl: &FrozenModule, rule_symbol: &str) -> Vec<String> {
    use crate::starlark_types::rule::FrozenRuleClass;
    use starlark::values::ValueLike;
    let Ok((owned, _vis)) = bzl.get_any_visibility(rule_symbol) else {
        return Vec::new();
    };
    let Some(rc) = owned.value().downcast_ref::<FrozenRuleClass>() else {
        return Vec::new();
    };
    rc.optional_toolchains.clone()
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

/// Whether the rule was declared `rule(analysis_test=True)`. Drives the
/// `--allow_analysis_failures` capture in [`resolve_label_deps`].
fn read_rule_is_analysis_test(bzl: &FrozenModule, rule_symbol: &str) -> bool {
    use crate::starlark_types::rule::FrozenRuleClass;
    use starlark::values::ValueLike;
    let Ok((owned, _vis)) = bzl.get_any_visibility(rule_symbol) else {
        return false;
    };
    owned
        .value()
        .downcast_ref::<FrozenRuleClass>()
        .map(|rc| rc.analysis_test)
        .unwrap_or(false)
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
#[allow(clippy::too_many_arguments)] // ctx + declared/optional + pkg + config + graph + 2 constraint sets
async fn resolve_declared_toolchains(
    ctx: &mut DiceComputations<'_>,
    declared: &[String],
    optional_declared: &[String],
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
    if declared.is_empty() {
        // Early-return path. Crucially this fires BEFORE the
        // toolchain-demand walk below: rules with no `toolchains=[...]`
        // declarations don't force any extensions, preserving laziness
        // for unreferenced toolchain-only extensions when analysis only
        // touches non-toolchain rules (e.g. `filegroup`).
        return Ok(Toolchains {
            by_type: BTreeMap::new(),
            repo_mapping: rule_mapping_arc,
            optional: Arc::new(std::collections::BTreeSet::new()),
        });
    }

    // Wanted set: canonicalize each declared toolchain-type string.
    // A string that fails to canonicalise is dropped (analysis proceeds
    // with a partial map) but logged — a dropped type silently becomes a
    // downstream "no resolved toolchain", so surface the raw string and
    // error to make the gap diagnosable.
    let mut wanted: BTreeMap<Label, ()> = BTreeMap::new();
    for s in declared {
        match Label::resolve(s, rule_pkg, &rule_mapping) {
            Ok(l) => {
                wanted.insert(l, ());
            }
            Err(e) => {
                tracing::warn!(
                    declared = %s,
                    rule_pkg = %rule_pkg,
                    error = %e,
                    "declared toolchain type dropped during canonicalisation"
                );
            }
        }
    }
    if wanted.is_empty() {
        return Ok(Toolchains {
            by_type: BTreeMap::new(),
            repo_mapping: rule_mapping_arc,
            optional: Arc::new(std::collections::BTreeSet::new()),
        });
    }

    // Optional (`mandatory = False`) subset of the declared types,
    // canonicalised the same way as `wanted`. An optional type that
    // doesn't resolve yields `ctx.toolchains[T] == None` rather than
    // raising; types outside this set still raise when unresolved.
    let optional_wanted: std::collections::BTreeSet<Label> = optional_declared
        .iter()
        .filter_map(|s| Label::resolve(s, rule_pkg, &rule_mapping).ok())
        .collect();

    // Toolchain demand path. The MODULE.bazel-level
    // `register_toolchains(...)` calls landed in
    // `graph.registered_toolchains` at parse time. Module extensions
    // can also call `module_ctx.register_toolchains(...)` mid-impl;
    // those contributions sit inside each `ModuleExtensionKey::Value`
    // and require the extension to actually run.
    //
    // Force every extension declared via a non-dev `use_extension` by
    // a reached module — i.e. every entry in `ExtensionInputsKey` —
    // and union their captured toolchains in. An extension that is
    // never `use_extension`'d (or whose `use_extension` was
    // dev-dependency in a non-root module) stays cold.
    //
    // Cached: `ModuleExtensionKey` is DICE-keyed, so subsequent
    // `ResolvedToolchainsKey` computes reuse the eval output.
    let mut combined_registrations: Vec<Label> = graph.registered_toolchains.clone();
    if let Ok(demand) = force_extension_demand(ctx).await {
        let ext_ids: Vec<ExtensionId> = demand.inputs.keys().cloned().collect();
        let outputs: Vec<(ExtensionId, Option<Arc<ModuleExtensionEvalOutput>>)> = ctx
            .compute_join(ext_ids, |ctx, ext_id: ExtensionId| {
                use futures::FutureExt;
                async move {
                    let r = match ctx.compute(&ModuleExtensionKey(ext_id.clone())).await {
                        Ok(SharedResult::Ok(v)) => Some(v),
                        Ok(SharedResult::Err(e)) => {
                            tracing::warn!(ext=%ext_id, error=%e, "toolchain demand: extension eval failed");
                            None
                        }
                        Err(e) => {
                            tracing::warn!(ext=%ext_id, error=%e, "toolchain demand: extension compute error");
                            None
                        }
                    };
                    (ext_id, r)
                }
                .boxed()
            })
            .await;
        for (ext_id, out) in outputs {
            let Some(out) = out else { continue };
            // Canonicalise each raw label string through the
            // extension's defining-`.bzl` perspective. Matches what
            // `eval_module_extension`'s impl would see at eval time
            // (per the existing mapping derivation in
            // `ModuleExtensionKey::compute`).
            let pkg = PackageId::new(
                ext_id.defining_bzl.package.repo.clone(),
                ext_id.defining_bzl.package.path.clone(),
            );
            let mapping = graph.effective_repo_mapping(&pkg);
            for raw in &out.registered_toolchains {
                match Label::resolve(raw, &pkg, &mapping) {
                    Ok(l) => combined_registrations.push(l),
                    Err(e) => tracing::debug!(
                        ext = %ext_id, raw = %raw, error = %e,
                        "toolchain demand: failed to canonicalise extension toolchain label"
                    ),
                }
            }
        }
    }

    if combined_registrations.is_empty() {
        return Ok(Toolchains {
            by_type: BTreeMap::new(),
            repo_mapping: rule_mapping_arc,
            optional: Arc::new(optional_wanted.clone()),
        });
    }

    // Wave 1: fan out every registration's `PackageKey` in parallel.
    // DICE memoizes so duplicate package fetches dedupe across the
    // wave. The first-match-wins walk below runs sequentially over the
    // pre-loaded packages — the ordering constraint forbids parallel
    // candidate selection across registrations, but loading the
    // packages doesn't depend on ordering.
    let reg_with_pkg: Vec<(Label, Option<Arc<Package>>)> = ctx
        .compute_join(combined_registrations, |ctx, reg_label: Label| {
            use futures::FutureExt;
            async move {
                let pkg = match ctx.compute(&PackageKey(reg_label.package.clone())).await {
                    Ok(SharedResult::Ok(p)) => Some(p),
                    Ok(SharedResult::Err(e)) => {
                        // A registration package that fails to load (e.g. an
                        // extension-generated toolchain repo whose
                        // repository_rule eval errors) silently drops the
                        // candidate, leaving only a downstream "no resolved
                        // toolchain". Surface it at warn so the real cause is
                        // visible (the probe runs RUST_LOG=warn).
                        tracing::warn!(reg=%reg_label, error=%e, "toolchain package load failed");
                        None
                    }
                    _ => None,
                };
                (reg_label, pkg)
            }
            .boxed()
        })
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
            // A matched candidate whose impl fails to analyse does NOT
            // fail the consumer. Such failures are usually downstream of
            // a tolerated empty-placeholder repo download (hermetic
            // interpreters, node binaries, …): the toolchain can't be
            // modelled, but the type is simply left *unresolved* so the
            // consumer sees `ctx.toolchains[T] == None` and copes when
            // the type is optional (`mandatory = False`). Logged at warn
            // so the gap stays visible rather than silently swallowed.
            let impl_config =
                derive_dep_config(config, crate::starlark_types::attr::AttrCfgKind::Exec);
            match force_analysis(ctx, impl_label.clone(), &impl_config).await {
                Ok(analyzed) => {
                    by_type.insert(tt_label, analyzed);
                }
                Err(e) => {
                    tracing::warn!(
                        tt = %tt_label,
                        toolchain_impl = %impl_label,
                        error = %e,
                        "toolchain impl analysis failed; leaving type unresolved"
                    );
                }
            }
        }
        // `pkg` is released at the end of the loop body — `force_analysis`
        // above runs on each candidate before the borrow is dropped, but
        // it doesn't need `pkg` itself, so this is fine.
    }

    // Diagnostic: a mandatory wanted type left unresolved becomes a
    // downstream "no resolved toolchain" error. Surface whether any
    // registration even targeted the type's own repo — distinguishing a
    // registration that was never captured from one captured but
    // carrying no matching `toolchain(...)`. Optional types are expected
    // to be absent, so skip them to keep the log focused.
    for tt in wanted.keys() {
        if by_type.contains_key(tt) {
            continue;
        }
        // Include optional types: a rule may declare `proto_toolchain_type`
        // optional and then `fail()` on its own when the lookup is None,
        // so the gap is invisible unless optional misses are surfaced too.
        let optional = optional_wanted.contains(tt);
        let tt_s = tt.to_string();
        let repo_prefix = tt_s.split("//").next().unwrap_or("");
        let same_repo: Vec<String> = reg_with_pkg
            .iter()
            .filter(|(reg, _)| !repo_prefix.is_empty() && reg.to_string().starts_with(repo_prefix))
            .map(|(reg, pkg)| {
                if pkg.is_some() {
                    reg.to_string()
                } else {
                    format!("{reg} [pkg-load-failed]")
                }
            })
            .collect();
        tracing::warn!(
            tt = %tt,
            optional = optional,
            registrations_total = reg_with_pkg.len(),
            same_repo_registrations = ?same_repo,
            "toolchain type wanted but unresolved"
        );
    }

    Ok(Toolchains {
        by_type,
        repo_mapping: rule_mapping_arc,
        optional: Arc::new(optional_wanted),
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
    repo_mapping: &loupe_labels::RepoMapping,
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

/// Build a per-target `Configuration` whose `exec_properties` reflects
/// both the configured target platform and any engine-level override.
///
/// Precedence (low → high): platform attr → engine-level override.
/// Matches Bazel's "platform default, command-line override" model:
/// `--platform_property` and `--remote_default_exec_properties` shape
/// flags overlay onto the platform's `exec_properties`. Loupe doesn't
/// model the flag surface yet — the engine-level override
/// (`Configuration::with_exec_properties`) stands in for it.
///
/// When `plat` is `None` (no platform configured or the platform
/// target couldn't be loaded) the config passes through unchanged —
/// callers see only what they set on `Configuration` directly.
fn merge_platform_exec_properties(
    config: &Configuration,
    plat: Option<&PlatformConstraintSet>,
) -> Configuration {
    let Some(plat) = plat else {
        return config.clone();
    };
    if plat.exec_properties.is_empty() {
        return config.clone();
    }
    let mut merged = plat.exec_properties.clone();
    for (k, v) in config.exec_properties() {
        merged.insert(k.clone(), v.clone());
    }
    config.clone().with_exec_properties(merged)
}

/// Read a `string_dict`-shaped attr into a `BTreeMap<String, String>`.
/// Coerces non-string values to their `Display` form — Bazel's
/// `exec_properties` is documented as `Dict[str, str]`, but Starlark
/// will happily evaluate `{"K": 1}` and the BUILD-side capture stores
/// it as a `String(name) → AttrValue::Int(1)` pair. Stringifying keeps
/// the call non-fatal; Bazel itself errors on non-strings but the
/// fidelity loss here isn't worse than dropping the entry silently.
fn attr_as_string_dict(v: Option<&crate::package::AttrValue>) -> BTreeMap<String, String> {
    use crate::package::AttrValue;
    let mut out = BTreeMap::new();
    if let Some(AttrValue::Dict(m)) = v {
        for (k, val) in m {
            let s = match val {
                AttrValue::String(s) => s.clone(),
                AttrValue::Bool(b) => b.to_string(),
                AttrValue::Int(i) => i.to_string(),
                AttrValue::None => String::new(),
                other => format!("{other:?}"),
            };
            out.insert(k.clone(), s);
        }
    }
    out
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
    repo_mapping: &loupe_labels::RepoMapping,
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
    repo_mapping: &loupe_labels::RepoMapping,
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
                // Read the flag's value from engine state — the value
                // the analysis driver computed for this build setting —
                // rather than reaching into `bazel_skylib`'s
                // `BuildSettingInfo` provider by symbol + repr. A target
                // with no `build_setting_value` isn't a build setting, so
                // the flag reference can't match.
                let actual = match &flag_target.build_setting_value {
                    Some(v) => v.clone(),
                    None => return Ok(None),
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
    repo_mapping: &loupe_labels::RepoMapping,
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
    repo_mapping: &loupe_labels::RepoMapping,
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
    repo_mapping: &loupe_labels::RepoMapping,
) -> Option<PlatformConstraintSet> {
    let mut out = PlatformConstraintSet::default();
    let mut visited: std::collections::BTreeSet<Label> = std::collections::BTreeSet::new();
    let mut stack: Vec<Label> = vec![platform_label.clone()];
    let mut found_any = false;
    // Layer `exec_properties` parent-first so child entries override
    // — matches Bazel's `platform(parents=[...])` inheritance, where
    // a child platform's `exec_properties` wins on key collision.
    // We walk depth-first from `platform_label`, but the stack pops
    // in LIFO order; instead of insertion-order tricks, collect each
    // level's map in walk order and merge at the end by iterating
    // *reversed* — that puts the requested platform's overrides last
    // and they survive (BTreeMap::insert replaces on duplicate key).
    let mut props_layers: Vec<BTreeMap<String, String>> = Vec::new();
    while let Some(cur) = stack.pop() {
        if !visited.insert(cur.clone()) {
            continue;
        }
        let Some((cvs, settings, parents, exec_props)) =
            load_one_platform(ctx, &cur, repo_mapping).await
        else {
            continue;
        };
        found_any = true;
        out.cvs.extend(cvs);
        out.assigned_settings.extend(settings);
        // The first iteration always processes `platform_label`
        // itself, so `props_layers[0]` is the requested platform's
        // own `exec_properties`. Subsequent layers are parents
        // (visited via the depth-first stack pop).
        props_layers.push(exec_props);
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
    // Merge parent → child so child overrides win. `props_layers[0]`
    // is the requested platform (the leaf-most child); apply it last.
    for layer in props_layers.iter().rev() {
        for (k, v) in layer {
            out.exec_properties.insert(k.clone(), v.clone());
        }
    }
    Some(out)
}

/// Load one `platform(...)` target, returning:
/// - its direct `constraint_values` (canonicalised),
/// - the `constraint_setting`s those CVs belong to (read from each
///   CV target's `constraint_setting` attr),
/// - parent labels for the caller's traversal,
/// - and the platform's own `exec_properties` map (REAPI
///   `Platform.properties`). Empty when the attr is unset.
async fn load_one_platform(
    ctx: &mut DiceComputations<'_>,
    platform_label: &Label,
    repo_mapping: &loupe_labels::RepoMapping,
) -> Option<(
    std::collections::BTreeSet<Label>,
    std::collections::BTreeSet<Label>,
    Vec<Label>,
    BTreeMap<String, String>,
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
    // Read `exec_properties = {...}` off the platform target. The
    // BUILD-side dict surfaces as `AttrValue::Dict<String, AttrValue>`
    // through `unpack_attr`; non-string values get coerced to their
    // string form (Bazel's `exec_properties` is documented as
    // string→string, so this just hardens against odd inputs).
    let exec_properties = attr_as_string_dict(target.attrs.get("exec_properties"));
    Some((cvs, assigned_settings, parents, exec_properties))
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
                Err(_) => {
                    let is_executable = source_file_is_executable(ctx, &target.label).await;
                    return synthesise_source_file_analysis(target, config, is_executable);
                }
            }
        }
        _ => {
            // Malformed (no forwarding label or non-label value) —
            // fall back to the synthetic-source-file path so the
            // graph still walks.
            let is_executable = source_file_is_executable(ctx, &target.label).await;
            return synthesise_source_file_analysis(target, config, is_executable);
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
        // Forward the flag value so an `alias` / `label_flag` /
        // `label_setting` pointing at a build setting still resolves in
        // `select()`.
        build_setting_value: analyzed.build_setting_value.clone(),
    })
}

/// Analysis for a `package_group` target: an empty `DefaultInfo` plus a
/// `PackageSpecificationInfo`. The cc toolchain rule indexes
/// `allowlist_target[PackageSpecificationInfo]`; without the provider the
/// access raises and the cc toolchain (and everything depending on it,
/// e.g. proto/protoc) fails to analyse.
fn synthesise_package_group_analysis(
    target: &crate::package::UnconfiguredTarget,
    config: &Configuration,
) -> Result<AnalyzedTarget, String> {
    let (analysis_heap, providers) = analysis::mint_package_group_providers()
        .map_err(|e| format!("synthesising package_group providers: {e:#}"))?;
    Ok(AnalyzedTarget {
        label: target.label.clone(),
        configuration: config.clone(),
        rule_kind: target.kind.clone(),
        providers,
        actions: Vec::new(),
        analysis_heap,
        build_setting_value: None,
    })
}

fn synthesise_source_file_analysis(
    target: &crate::package::UnconfiguredTarget,
    config: &Configuration,
    is_executable: bool,
) -> Result<AnalyzedTarget, String> {
    // Mint `DefaultInfo(files = depset([file]))` through the vendored
    // provider so `files_to_run` / `data_runfiles` / `default_runfiles`
    // get the provider's real `init`/`computed_fields` shape — identical
    // to a rule target's `DefaultInfo`, no hand-rolled field set. The
    // on-disk POSIX `+x` bit rides on the `File` itself (Bazel surfaces
    // source-file executability on the artifact, not on
    // `files_to_run.executable`).
    let synthetic_file = crate::starlark_types::file::File::source(
        target.label.package.clone(),
        target.label.name(),
    )
    .with_layout(config.repository_layout())
    .with_is_executable(is_executable);

    let (analysis_heap, provider) = analysis::mint_default_info(vec![synthetic_file])
        .map_err(|e| format!("synthesising source-file DefaultInfo: {e:#}"))?;

    Ok(AnalyzedTarget {
        label: target.label.clone(),
        configuration: config.clone(),
        rule_kind: target.kind.clone(),
        providers: vec![provider],
        actions: Vec::new(),
        analysis_heap,
        // Source files are never build settings.
        build_setting_value: None,
    })
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

async fn force_empty_default_info(
    ctx: &mut DiceComputations<'_>,
) -> Result<Arc<analysis::EmptyDefaultInfo>, String> {
    match ctx.compute(&EmptyDefaultInfoKey).await {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("EmptyDefaultInfoKey: {e}")),
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

/// Force `FileMetadataKey` for `path`. Returns `FileMetadata::MISSING`
/// (via the `SharedResult::Ok` arm) when the file isn't on disk —
/// auto-discovered source-file labels regularly hit this case.
async fn force_file_metadata(
    ctx: &mut DiceComputations<'_>,
    path: PathBuf,
) -> Result<Arc<FileMetadata>, String> {
    let display = path.display().to_string();
    match ctx.compute(&FileMetadataKey(path)).await {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
        Err(e) => Err(format!("FileMetadataKey({display}): {e}")),
    }
}

/// Build the on-disk path of a source `File` rooted under the
/// workspace. Mirrors `<workspace_root> / <File::path()>` for
/// main-repo files; external-repo source files would need the
/// configured `external_repo_root`. v1 only reads metadata for
/// main-repo source files (the auto-discovered synthetic path),
/// where this composition is correct.
fn source_file_disk_path(workspace_root: &Path, target_label: &Label) -> PathBuf {
    let pkg_path = target_label.package.path.as_str();
    let name = target_label.name();
    if pkg_path.is_empty() {
        workspace_root.join(name)
    } else {
        workspace_root.join(pkg_path).join(name)
    }
}

/// Force `FileMetadataKey` for the on-disk source-file at
/// `target_label` and return the POSIX `+x` bit. Returns `false` for:
/// - External-repo labels (we don't have a workspace-relative path).
/// - Missing files (auto-discovered labels whose source isn't staged).
/// - Stat errors (rare; surfaced as `false` rather than failing
///   analysis, since a stat failure on a synthetic-source-file target
///   isn't worth poisoning the whole analyse).
async fn source_file_is_executable(ctx: &mut DiceComputations<'_>, target_label: &Label) -> bool {
    // External repos: TODO/STUB — needs `external_repo_root_for(repo)`
    // to compose the disk path. Skipped for now; toolchain binaries
    // hauled in via `http_archive`-shaped external repos will mis-report
    // until this lands. Same gap shape as the original source-file STUB.
    if !target_label.package.repo.is_main() {
        return false;
    }
    let workspace_root = ctx.global_data().workspace_root().to_path_buf();
    let disk_path = source_file_disk_path(&workspace_root, target_label);
    match force_file_metadata(ctx, disk_path).await {
        Ok(m) => m.is_executable,
        Err(_) => false,
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
    use loupe_bzlmod::{BzlmodResult, MaterializedRepoKey, force_repo_spec};
    // 1. Static spec (lockfile / registry / manually-supplied) — the
    //    existing path. Records a dep edge on `RepoSpecKey(canonical)`
    //    so a later spec injection invalidates `BzlLoadKey` /
    //    `PackageKey` readers structurally.
    if force_repo_spec(ctx, canonical).await?.is_some() {
        return match ctx
            .compute(&MaterializedRepoKey(canonical.to_string()))
            .await
        {
            Ok(BzlmodResult::Ok(path)) => Ok(Some(crate::repo_root::RepoRoot::Materialised {
                canonical: canonical.to_string(),
                dir: (*path).clone(),
            })),
            Ok(BzlmodResult::Err(e)) => Err(Arc::unwrap_or_clone(e)),
            Err(e) => Err(format!("MaterializedRepoKey({canonical}): {e}")),
        };
    }

    // 2. Lazy demand route: this canonical might be produced by a
    //    module_extension. Derive the owner index from
    //    `ModuleGraphKey` + `BzlmodGraphKey` (both DICE-cached); on
    //    hit, force the extension via `ExtMaterializedRepoKey` and
    //    materialise from its captured spec.
    let demand = match force_extension_demand(ctx).await {
        Ok(d) => d,
        Err(_) => return Ok(None), // ModuleGraphKey not available (tests bypassing EngineBuilder)
    };
    let Some(ext_id) = demand.owner_of(canonical) else {
        return Ok(None);
    };

    match ctx
        .compute(&ExtMaterializedRepoKey(canonical.to_string()))
        .await
    {
        Ok(SharedResult::Ok(path)) => Ok(Some(crate::repo_root::RepoRoot::Materialised {
            canonical: canonical.to_string(),
            dir: (*path).clone(),
        })),
        Ok(SharedResult::Err(e)) => Err(format!(
            "ExtMaterializedRepoKey({canonical}) via {ext_id}: {}",
            Arc::unwrap_or_clone(e)
        )),
        Err(e) => Err(format!("ExtMaterializedRepoKey({canonical}): {e}")),
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
    // Resolve every load target — demand-aware so sibling repos of an
    // extension-generated `current_pkg` resolve (forces the owning
    // extension on a static-resolve miss). Then fan out the transitive
    // `BzlLoadKey` forces in parallel.
    let mut resolved: Vec<(String, Label)> = Vec::with_capacity(load_stmts.len());
    for stmt in &load_stmts {
        let target = resolve_load_demand(ctx, &stmt.module_id, &label.package, graph).await?;
        resolved.push((stmt.module_id.clone(), target));
    }
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
    // `effective_repo_mapping` sets the mapping's `owner` to the
    // .bzl's repo so `Label("@//pkg:x")` self-refs work out of the box.
    let defining_mapping = graph.effective_repo_mapping_cached(&label.package.repo);
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
    let mut resolved: Vec<(String, Label)> = Vec::with_capacity(load_stmts.len());
    for stmt in &load_stmts {
        let target = resolve_load_demand(ctx, &stmt.module_id, pkg, graph).await?;
        resolved.push((stmt.module_id.clone(), target));
    }
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
    // Per-repo merged mapping for resolving label attrs inside this
    // BUILD. For an ext-generated repo (e.g. rules_java's
    // `…+remotejdk21_linux_aarch64_toolchain_config_repo`) the BUILD's
    // `toolchain(toolchain = "@remotejdk21_linux_aarch64//:jdk")`
    // references a *sibling* repo from the same extension, which isn't
    // in the static module-graph mapping. `ext_sibling_mapping`
    // augments it with the siblings + owning module's view (same path
    // used for `.bzl` load resolution).
    let effective_mapping = match ext_sibling_mapping(ctx, pkg, graph).await {
        Some(m) => Arc::new(m),
        None => graph.effective_repo_mapping_cached(&pkg.repo),
    };
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
    let mapping = Arc::new(graph.repo_mapping.clone());
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
            "loading `{}`: external repo `{}` is not registered \
             (no RepoSpec, no materialised cache entry). Configure \
             a registry (`with_bcr` / `with_registry`) and a bzlmod \
             cache so the in-process resolver can fetch it.",
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
    let effective = graph.effective_repo_mapping_cached(&current_pkg.repo);
    Label::resolve(load_str, current_pkg, &effective)
        .map_err(|e| anyhow::anyhow!("resolving load `{load_str}` from {current_pkg}: {e}"))
}

/// Demand-aware load resolution. Tries the static `resolve_load`
/// first; on an "unknown apparent repo" failure where `current_pkg`
/// is itself an extension-generated repo, forces the owning extension
/// and retries against its full sibling set.
///
/// Why this exists: an extension like rules_js's `npm_translate_lock`
/// generates a hub repo (`@@+npm+npm`) plus dozens of leaf repos
/// (`@@+npm+npm__typescript__5.8.2`, …). The hub's `.bzl` files
/// `load("@npm__typescript__5.8.2//:package_json.bzl")` — referencing
/// siblings by short name. Those siblings are discovered only by
/// *running* the extension; they're absent from the module graph's
/// `per_repo_mapping` (which is built before any extension runs). So
/// the static resolve fails, and we fall back to forcing the owning
/// extension's `ModuleExtensionKey` to learn the sibling canonicals.
async fn resolve_load_demand(
    ctx: &mut DiceComputations<'_>,
    load_str: &str,
    current_pkg: &PackageId,
    graph: &ModuleGraph,
) -> Result<Label, String> {
    if let Ok(l) = resolve_load(load_str, current_pkg, graph) {
        return Ok(l);
    }
    if let Some(mapping) = ext_sibling_mapping(ctx, current_pkg, graph).await
        && let Ok(l) = Label::resolve(load_str, current_pkg, &mapping)
    {
        return Ok(l);
    }
    // Fall through to the original (graph-only) error for the clearest
    // diagnostic — the sibling path didn't have it either.
    resolve_load(load_str, current_pkg, graph).map_err(|e| format!("{e:#}"))
}

/// Build an apparent→canonical mapping that augments
/// `graph.effective_repo_mapping(current_pkg)` with every sibling repo
/// produced by the extension that owns `current_pkg`'s repo. Returns
/// `None` when `current_pkg` is not an extension-generated repo, or
/// when the owning extension can't be forced.
async fn ext_sibling_mapping(
    ctx: &mut DiceComputations<'_>,
    current_pkg: &PackageId,
    graph: &ModuleGraph,
) -> Option<loupe_labels::RepoMapping> {
    let repo = current_pkg.repo.as_str().to_string();
    let demand = force_extension_demand(ctx).await.ok()?;
    let ext_id = demand.owner_of(&repo)?;
    let inputs = demand.inputs.get(&ext_id)?;
    // Sibling canonicals share the `<owner><sep><ext><sep>` prefix;
    // the suffix is the short name that loads reference.
    let prefix = format!(
        "{}{}{}{}",
        inputs.owner_canonical, inputs.sep, ext_id.ext_name, inputs.sep
    );
    let output = match ctx.compute(&ModuleExtensionKey(ext_id)).await {
        Ok(SharedResult::Ok(o)) => o,
        _ => return None,
    };
    let mut mapping = graph.effective_repo_mapping(current_pkg);
    // Inherit the owning module's apparent→canonical view. An
    // ext-generated repo's `.bzl`s load from the extension's *defining*
    // module's deps — e.g. `cc_compatibility_proxy` (owned by rules_cc)
    // loads `@rules_cc//cc/private:cc_common.bzl`. `rules_cc` resolves
    // through rules_cc's own per-repo mapping, not the consuming
    // module's. The owner canonical is `inputs.owner_canonical`
    // (empty == main).
    let owner_pkg = PackageId::new(
        loupe_labels::CanonicalRepo::new(inputs.owner_canonical.clone()),
        loupe_labels::PackagePath::root(),
    );
    for (k, v) in graph.effective_repo_mapping(&owner_pkg).entries {
        mapping.entries.entry(k).or_insert(v);
    }
    for canonical in output.specs.keys() {
        if let Some(short) = canonical.strip_prefix(&prefix) {
            mapping
                .entries
                .entry(short.to_string())
                .or_insert_with(|| loupe_labels::CanonicalRepo::new(canonical.clone()));
        }
    }
    Some(mapping)
}

// Lazy module-extension evaluation.
//
// `EngineBuilder::build` no longer evaluates extensions eagerly.
// Instead, it injects two index keys:
//
//   `RepoOwnerIndexKey` — every canonical name a `use_repo(ext, ...)`
//                         clause would produce, mapped back to the
//                         extension that would generate it.
//   `ExtensionInputsKey` — for every `use_extension`-declared
//                          extension, the ordered `ModuleView` list
//                          (root tags first, transitives in resolution
//                          order) the extension impl will see.
//
// Demand routes:
//
//   1. `maybe_materialise(ctx, canonical)` — when no static `RepoSpec`
//      is registered, consults `RepoOwnerIndexKey` and forces
//      `ModuleExtensionKey` for the owning extension; on success,
//      promotes the captured spec into `RepoSpecsKey` via
//      `inject_extension_specs(...)` and re-tries.
//
//   2. `ResolvedToolchainsKey` — forces every extension reachable via
//      a non-dev `use_extension` from a reached module, unions their
//      captured `registered_toolchains` into the registration list.
//
// Both routes are pull-only: an extension that no demand reaches never
// has its `ModuleExtensionKey::compute` invoked.

/// Identity of one `use_extension(...)` declaration: the
/// canonical-form `.bzl` label that defines the extension plus the
/// exported symbol name. Two `use_extension` records that resolve to
/// the same `(defining_bzl, ext_name)` merge into one
/// `ModuleExtensionKey`, matching Bazel's "extension runs once with
/// every consumer's tags" semantics.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Allocative, Pagable)]
pub struct ExtensionId {
    pub defining_bzl: Label,
    pub ext_name: String,
}

impl std::fmt::Display for ExtensionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}%{}", self.defining_bzl, self.ext_name)
    }
}

/// Inputs assembled per extension: the ordered module list it will
/// see, plus the canonical-naming context
/// (`<owner_canonical><sep><ext_name><sep><repo_short>`) the engine
/// uses when promoting captured `http_archive(...)` calls into
/// `RepoSpec`s.
#[derive(Clone, Debug)]
pub struct ExtensionInputs {
    pub modules: Vec<loupe_bzlmod::ModuleView>,
    pub owner_canonical: String,
    pub sep: char,
    /// `Some(rule_name)` marks an *innate* extension synthesised from a
    /// `use_repo_rule(...)` proxy. For innate extensions no
    /// `module_extension` impl runs; instead `innate_invocations` are
    /// promoted directly to `RepoSpec::StarlarkRule`s, and `rule_name`
    /// is the middle canonical segment (`<owner><sep><rule_name><sep>
    /// <repo>`). `None` for ordinary `use_extension(...)` extensions.
    pub innate_rule_name: Option<String>,
    /// The captured `proxy(name=…, …)` calls for an innate extension.
    /// Empty for ordinary extensions.
    pub innate_invocations: Vec<loupe_bzlmod::resolution::RepoRuleInvocation>,
}

/// Lazy-demand projection of `ModuleGraph` + `BzlmodGraph`. Carries
/// both halves the lazy demand path needs:
///
///   - `owner_index`: `canonical → ExtensionId` — every name a root
///     or transitive `use_repo(ext, name)` would produce. Used by
///     `maybe_materialise` / `ExtMaterializedRepoKey` to find the
///     owning extension for a demanded canonical.
///   - `inputs`: `ExtensionId → ExtensionInputs` — the ordered
///     module list + canonical-naming context the extension's impl
///     will see. Used by `ModuleExtensionKey::compute` and the
///     toolchain-demand walk in `resolve_declared_toolchains`.
///
/// Pure derivation — produced on-the-fly inside each compute that
/// needs it. Both source keys (`ModuleGraphKey`, `BzlmodGraphKey`)
/// are DICE-cached, so consumers pay only the BTreeMap-walk cost
/// (a few thousand ops on typical workspaces).
pub struct ExtensionDemand {
    pub owner_index: BTreeMap<String, ExtensionId>,
    pub inputs: BTreeMap<ExtensionId, ExtensionInputs>,
}

impl ExtensionDemand {
    /// Find the extension that owns `canonical`. Two routes:
    ///   1. `owner_index` — exact match for a `use_repo(ext, name)`'d
    ///      canonical (the statically-declared set).
    ///   2. Prefix match — `canonical` is a *leaf* repo an extension
    ///      generated internally (never `use_repo`'d), e.g.
    ///      `+npm+npm__typescript__5.8.2`. Its
    ///      `<owner_canonical><sep><ext_name><sep>` prefix uniquely
    ///      identifies the owning extension. rules_js's
    ///      `npm_translate_lock` produces hundreds of these per-package
    ///      leaf repos; only the hub is `use_repo`'d, so route 1 misses
    ///      them all.
    pub fn owner_of(&self, canonical: &str) -> Option<ExtensionId> {
        if let Some(id) = self.owner_index.get(canonical) {
            return Some(id.clone());
        }
        for (ext_id, inputs) in &self.inputs {
            let prefix = format!(
                "{}{}{}{}",
                inputs.owner_canonical, inputs.sep, ext_id.ext_name, inputs.sep
            );
            if canonical.starts_with(&prefix) && canonical.len() > prefix.len() {
                return Some(ext_id.clone());
            }
        }
        None
    }
}

/// Compute the [`ExtensionDemand`] projection from the resolved
/// `ModuleGraph` and `BzlmodGraph`. Mirrors what Bazel does to
/// decide which extension produces which repo, statically — no
/// extension impl runs.
pub fn derive_extension_demand(
    graph: &ModuleGraph,
    bzlmod_graph: &loupe_bzlmod::bzlmod_graph::BzlmodGraph,
) -> Result<ExtensionDemand, String> {
    use loupe_bzlmod::ModuleTagInvocation;
    use loupe_bzlmod::ModuleView;

    let main_pkg = PackageId::new(
        graph.main.canonical.clone(),
        loupe_labels::PackagePath::root(),
    );
    let main_name = graph.main.name.clone();
    let main_version = graph.main.version.clone();
    let main_owner = graph.main.canonical.as_str().to_string();
    let sep = graph.canonical_separator();

    let mut grouped: BTreeMap<ExtensionId, (Vec<ModuleView>, String)> = BTreeMap::new();
    let mut owner_index: BTreeMap<String, ExtensionId> = BTreeMap::new();
    // Innate extensions synthesised from `use_repo_rule(...)`: one entry
    // per (declaring module, defining_bzl, rule_name). Value carries the
    // owner canonical, rule name, and the captured invocations.
    let mut innate: BTreeMap<
        ExtensionId,
        (
            String,
            String,
            Vec<loupe_bzlmod::resolution::RepoRuleInvocation>,
        ),
    > = BTreeMap::new();

    // Root MODULE.bazel use_extension records. We read from the
    // BzlmodGraph snapshot (`main.use_extensions`) instead of
    // `graph.use_extensions` so tag attrs come pre-typed as
    // `RepoAttrValue`s — no conversion pass needed.
    for rec in &bzlmod_graph.main.use_extensions {
        let bzl_label = Label::resolve(&rec.bzl_label, &main_pkg, &graph.repo_mapping)
            .map_err(|e| format!("resolving root use_extension `{}`: {e}", rec.bzl_label))?;
        let ext_id = ExtensionId {
            defining_bzl: bzl_label.clone(),
            ext_name: rec.name.clone(),
        };
        let owner_canonical: String =
            if bzl_label.package.repo.as_str() == graph.main.canonical.as_str() {
                main_owner.clone()
            } else {
                bzl_label.package.repo.as_str().to_string()
            };
        let main_tags: Vec<ModuleTagInvocation> = rec
            .tags
            .iter()
            .map(|t| ModuleTagInvocation {
                class: t.class.clone(),
                attrs: t.attrs.clone(),
            })
            .collect();
        let entry = grouped
            .entry(ext_id.clone())
            .or_insert_with(|| (Vec::new(), owner_canonical.clone()));
        entry.0.push(ModuleView {
            name: main_name.clone(),
            version: main_version.clone(),
            is_root: true,
            canonical: graph.main.canonical.as_str().to_string(),
            tags: main_tags,
        });
        for short in &rec.use_repo_names {
            let canonical = format!("{}{}{}{}{}", owner_canonical, sep, rec.name, sep, short);
            owner_index
                .entry(canonical)
                .or_insert_with(|| ext_id.clone());
        }
    }

    // Root MODULE.bazel `use_repo_rule(...)` invocations → innate
    // extensions. Owner is the main module's canonical.
    for inv in &bzlmod_graph.main.repo_rule_invocations {
        let Ok(bzl_label) = Label::resolve(&inv.bzl_label, &main_pkg, &graph.repo_mapping) else {
            continue;
        };
        let ext_id = ExtensionId {
            defining_bzl: bzl_label,
            ext_name: format!("{} {}", main_owner, inv.rule_name),
        };
        let canonical = format!("{}{}{}{}{}", main_owner, sep, inv.rule_name, sep, inv.name);
        owner_index
            .entry(canonical)
            .or_insert_with(|| ext_id.clone());
        innate
            .entry(ext_id)
            .or_insert_with(|| (main_owner.clone(), inv.rule_name.clone(), Vec::new()))
            .2
            .push(inv.clone());
    }

    // Transitive modules' use_extension records (non-dev only).
    for m in &bzlmod_graph.transitive {
        let canon = loupe_labels::CanonicalRepo::new(m.canonical.clone());
        let mut mapping = loupe_labels::RepoMapping::owned_by(canon.clone());
        mapping.entries.insert(m.name.clone(), canon.clone());
        for b in &m.bazel_deps {
            mapping.entries.insert(
                b.apparent.clone(),
                loupe_labels::CanonicalRepo::new(b.canonical.clone()),
            );
        }
        let pkg = PackageId::new(canon, loupe_labels::PackagePath::root());
        for ue in &m.use_extensions {
            if ue.dev_dependency {
                continue;
            }
            let Ok(bzl_label) = Label::resolve(&ue.bzl_label, &pkg, &mapping) else {
                continue;
            };
            let ext_id = ExtensionId {
                defining_bzl: bzl_label.clone(),
                ext_name: ue.name.clone(),
            };
            let owner_canonical: String =
                if bzl_label.package.repo.as_str() == graph.main.canonical.as_str() {
                    main_owner.clone()
                } else {
                    bzl_label.package.repo.as_str().to_string()
                };
            let t_tags: Vec<ModuleTagInvocation> = ue
                .tags
                .iter()
                .map(|t| ModuleTagInvocation {
                    class: t.class.clone(),
                    attrs: t.attrs.clone(),
                })
                .collect();
            let entry = grouped
                .entry(ext_id.clone())
                .or_insert_with(|| (Vec::new(), owner_canonical.clone()));
            entry.0.push(ModuleView {
                name: m.name.clone(),
                version: m.version.clone(),
                is_root: false,
                canonical: m.canonical.clone(),
                tags: t_tags,
            });
            for short in &ue.use_repo_names {
                let canonical = format!("{}{}{}{}{}", owner_canonical, sep, ue.name, sep, short);
                owner_index
                    .entry(canonical)
                    .or_insert_with(|| ext_id.clone());
            }
        }
        // Transitive `use_repo_rule(...)` invocations → innate
        // extensions owned by this module.
        for inv in &m.repo_rule_invocations {
            let Ok(bzl_label) = Label::resolve(&inv.bzl_label, &pkg, &mapping) else {
                continue;
            };
            let ext_id = ExtensionId {
                defining_bzl: bzl_label,
                ext_name: format!("{} {}", m.canonical, inv.rule_name),
            };
            let canonical = format!("{}{}{}{}{}", m.canonical, sep, inv.rule_name, sep, inv.name);
            owner_index
                .entry(canonical)
                .or_insert_with(|| ext_id.clone());
            innate
                .entry(ext_id)
                .or_insert_with(|| (m.canonical.clone(), inv.rule_name.clone(), Vec::new()))
                .2
                .push(inv.clone());
        }
    }

    let mut inputs: BTreeMap<ExtensionId, ExtensionInputs> = grouped
        .into_iter()
        .map(|(k, (modules, owner_canonical))| {
            (
                k,
                ExtensionInputs {
                    modules,
                    owner_canonical,
                    sep,
                    innate_rule_name: None,
                    innate_invocations: Vec::new(),
                },
            )
        })
        .collect();
    for (ext_id, (owner_canonical, rule_name, invocations)) in innate {
        inputs.insert(
            ext_id,
            ExtensionInputs {
                modules: Vec::new(),
                owner_canonical,
                sep,
                innate_rule_name: Some(rule_name),
                innate_invocations: invocations,
            },
        );
    }

    Ok(ExtensionDemand {
        owner_index,
        inputs,
    })
}

/// Load + derive [`ExtensionDemand`] from inside a DICE compute. Both
/// source keys are DICE-cached; the derivation itself is pure.
pub(crate) async fn force_extension_demand(
    ctx: &mut DiceComputations<'_>,
) -> Result<ExtensionDemand, String> {
    let graph = match ctx.compute(&ModuleGraphKey).await {
        Ok(SharedResult::Ok(g)) => g,
        Ok(SharedResult::Err(e)) => {
            return Err(format!("ModuleGraphKey: {}", Arc::unwrap_or_clone(e)));
        }
        Err(e) => return Err(format!("ModuleGraphKey: {e}")),
    };
    let bzlmod = loupe_bzlmod::force_bzlmod_graph(ctx).await?;
    let Some(bzlmod) = bzlmod else {
        return Ok(ExtensionDemand {
            owner_index: BTreeMap::new(),
            inputs: BTreeMap::new(),
        });
    };
    derive_extension_demand(&graph, &bzlmod)
}

/// Output of one `ModuleExtensionKey::compute`: the per-canonical
/// `RepoSpec` map the extension's impl declared via
/// `http_archive(...)`-shape calls, plus the `ExtGenRecord` breakdown
/// of each canonical and the raw `register_toolchains(...)` labels.
#[derive(Debug, Clone)]
pub struct ModuleExtensionEvalOutput {
    pub specs: BTreeMap<String, loupe_bzlmod::RepoSpec>,
    pub ext_generated: Vec<loupe_bzlmod::bzlmod_graph::ExtGenRecord>,
    /// Raw label strings from `module_ctx.register_toolchains(*labels)`.
    /// `ResolvedToolchainsKey` canonicalises through the extension's
    /// effective repo mapping before matching.
    pub registered_toolchains: Vec<String>,
}

impl Allocative for ModuleExtensionEvalOutput {
    fn visit<'a, 'b: 'a>(&self, _v: &'a mut allocative::Visitor<'b>) {}
}

impl PartialEq for ModuleExtensionEvalOutput {
    fn eq(&self, other: &Self) -> bool {
        // RepoSpec / ExtGenRecord carry FrozenValues (not Eq); compare
        // by canonical names + ext_generated keys + toolchain labels.
        // A recompute that yields the same canonical-name set
        // structurally invalidates downstream readers via DICE
        // equality, but the per-canonical RepoSpec pointers may
        // differ (fresh frozen heaps). Treat as different to be safe.
        std::ptr::eq(self, other)
    }
}
impl Eq for ModuleExtensionEvalOutput {}

/// Evaluate one module_extension. Compute forces
/// `BzlLoadKey(defining_bzl)`, downcasts the exported symbol to
/// `FrozenModuleExtension`, looks up the cached `ExtensionInputs`,
/// runs `eval_module_extension(...)` on a blocking thread, and
/// promotes the captured `http_archive(...)` calls into per-canonical
/// `RepoSpec`s.
///
/// **Lazy demand.** This key has no callers until something else
/// (`maybe_materialise`, `ResolvedToolchainsKey`) reaches it. An
/// extension declared by an MODULE.bazel but never `use_repo`'d and
/// never producing a matched toolchain stays cold for the engine's
/// lifetime.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ModuleExtensionKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ModuleExtensionKey(pub ExtensionId);

#[async_trait]
impl Key for ModuleExtensionKey {
    type Value = SharedResult<ModuleExtensionEvalOutput>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let ExtensionId {
            defining_bzl,
            ext_name,
        } = self.0.clone();

        // 1. Load the extension's .bzl. A syntax error or missing
        //    `load(...)` in this file becomes a `BzlLoadKey` failure
        //    that surfaces here — under lazy demand this only fires
        //    when something downstream needs this extension.
        let loaded = match ctx.compute(&BzlLoadKey(defining_bzl.clone())).await {
            Ok(SharedResult::Ok(v)) => v,
            Ok(SharedResult::Err(e)) => {
                return SharedResult::err(format!(
                    "ModuleExtensionKey({self}): {}",
                    Arc::unwrap_or_clone(e)
                ));
            }
            Err(e) => return SharedResult::err(format!("ModuleExtensionKey({self}): {e}")),
        };

        // 2. Derive the module list / canonical-naming context from
        //    `ModuleGraphKey` + `BzlmodGraphKey`. Pure projection; no
        //    extension impl runs. Done before symbol extraction so the
        //    innate (`use_repo_rule`) branch can skip the
        //    `module_extension` downcast.
        let demand = match force_extension_demand(ctx).await {
            Ok(d) => d,
            Err(e) => return SharedResult::err(format!("ModuleExtensionKey({self}): {e}")),
        };
        let Some(inputs) = demand.inputs.get(&self.0).cloned() else {
            return SharedResult::err(format!(
                "ModuleExtensionKey({self}): no use_extension declaration found in graph"
            ));
        };

        // 2a. Innate extension (`use_repo_rule`): no `module_extension`
        //     impl runs. Load the repo rule symbol and promote each
        //     captured `proxy(name=…, …)` invocation directly into a
        //     `RepoSpec::StarlarkRule`.
        if let Some(rule_name) = inputs.innate_rule_name.clone() {
            let graph = match ctx.compute(&ModuleGraphKey).await {
                Ok(SharedResult::Ok(g)) => g,
                Ok(SharedResult::Err(e)) => {
                    return SharedResult::err(format!(
                        "ModuleExtensionKey({self}): ModuleGraph: {}",
                        Arc::unwrap_or_clone(e)
                    ));
                }
                Err(e) => return SharedResult::err(format!("ModuleExtensionKey({self}): {e}")),
            };
            let owned = match loaded.module.get(&rule_name) {
                Ok(v) => v,
                Err(e) => {
                    return SharedResult::err(format!(
                        "ModuleExtensionKey({self}): loading repo rule `{rule_name}` from \
                         `{defining_bzl}`: {e}"
                    ));
                }
            };
            // SAFETY: `rule_fv` is stored in the `RepoSpec::StarlarkRule`
            // below alongside `module: loaded.module.clone()`, which owns
            // its heap — value and owner travel together.
            let rule_fv = unsafe { owned.unchecked_frozen_value() };
            {
                use starlark::values::ValueLike;
                if rule_fv
                    .downcast_ref::<loupe_bzlmod::FrozenRepositoryRule>()
                    .is_none()
                {
                    return SharedResult::err(format!(
                        "ModuleExtensionKey({self}): `{rule_name}` in `{defining_bzl}` used by \
                         use_repo_rule is not a repository_rule (got type `{}`)",
                        rule_fv.to_value().get_type()
                    ));
                }
            }
            let mapping = {
                let pkg = PackageId::new(
                    defining_bzl.package.repo.clone(),
                    defining_bzl.package.path.clone(),
                );
                graph.effective_repo_mapping_cached(&pkg.repo)
            };
            let mut specs = BTreeMap::new();
            let mut ext_records = Vec::new();
            for inv in &inputs.innate_invocations {
                let canonical = format!(
                    "{}{}{}{}{}",
                    inputs.owner_canonical, inputs.sep, rule_name, inputs.sep, inv.name
                );
                let merged_attrs = crate::engine::merge_repo_rule_defaults(
                    rule_fv,
                    &canonical,
                    &defining_bzl,
                    inv.attrs.clone(),
                );
                ext_records.push(loupe_bzlmod::bzlmod_graph::ExtGenRecord {
                    canonical: canonical.clone(),
                    owner_canonical: inputs.owner_canonical.clone(),
                    ext_name: rule_name.clone(),
                    repo_short: inv.name.clone(),
                });
                specs.insert(
                    canonical.clone(),
                    loupe_bzlmod::RepoSpec {
                        canonical,
                        kind: loupe_bzlmod::RepoSpecKind::StarlarkRule {
                            module: loaded.module.clone(),
                            rule_fv,
                            attrs: merged_attrs,
                            rule_source_digest: Arc::clone(&loaded.source_digest),
                            repo_mapping: Arc::clone(&mapping),
                        },
                    },
                );
            }
            return SharedResult::ok(ModuleExtensionEvalOutput {
                specs,
                ext_generated: ext_records,
                registered_toolchains: Vec::new(),
            });
        }

        // 3. Extract the named symbol and downcast (ordinary
        //    `use_extension(...)` path).
        let owned = match loaded.module.get(&ext_name) {
            Ok(v) => v,
            Err(e) => {
                return SharedResult::err(format!(
                    "ModuleExtensionKey({self}): loading `{ext_name}` from `{defining_bzl}`: {e}"
                ));
            }
        };
        // SAFETY: `frozen_value` is only read within this compute (the
        // downcast check and `apply_tag_class_defaults` below); `owned` owns
        // the frozen heap and stays in scope through those reads.
        let frozen_value = unsafe { owned.unchecked_frozen_value() };
        {
            use starlark::values::ValueLike;
            if frozen_value
                .downcast_ref::<loupe_bzlmod::FrozenModuleExtension>()
                .is_none()
            {
                return SharedResult::err(format!(
                    "ModuleExtensionKey({self}): `{ext_name}` in `{defining_bzl}` is not a \
                     module_extension (got type `{}`)",
                    frozen_value.to_value().get_type()
                ));
            }
        }

        // 4. Resolve the runtime context the eval needs.
        let graph = match ctx.compute(&ModuleGraphKey).await {
            Ok(SharedResult::Ok(g)) => g,
            Ok(SharedResult::Err(e)) => {
                return SharedResult::err(format!(
                    "ModuleExtensionKey({self}): ModuleGraph: {}",
                    Arc::unwrap_or_clone(e)
                ));
            }
            Err(e) => return SharedResult::err(format!("ModuleExtensionKey({self}): {e}")),
        };
        use loupe_bzlmod::HasBzlmodCache;
        use loupe_bzlmod::dice::HasDeclaredEnv;
        let declared_env = ctx.global_data().declared_env();
        let workspace_root = ctx.global_data().workspace_root().to_path_buf();
        let bzlmod_cache = ctx
            .global_data()
            .bzlmod_cache()
            .map(std::path::Path::to_path_buf);

        // Eval-side repo mapping: the extension's defining .bzl owns
        // label resolution inside the impl. Mirrors the existing
        // `effective_repo_mapping_arc` in `engine.rs`.
        let mapping = {
            let pkg = PackageId::new(
                defining_bzl.package.repo.clone(),
                defining_bzl.package.path.clone(),
            );
            graph.effective_repo_mapping_cached(&pkg.repo)
        };

        // Tag-class defaults: copy the existing `apply_tag_class_defaults`
        // step from `engine.rs` so attr defaults reach the impl.
        // Passing `Some(&graph)` lets `attr.label` / `attr.label_list`
        // / `attr.label_keyed_string_dict` tag-attr values be
        // canonicalised against the using module's repo_mapping.
        let mut modules = inputs.modules.clone();
        crate::engine::apply_tag_class_defaults(frozen_value, &mut modules, Some(&graph));

        // Per-extension isolated working directory. Mirrors
        // `engine.rs::extension_working_dir`.
        let (extension_wd, _td_guard) = match crate::engine::extension_working_dir(
            &defining_bzl,
            &ext_name,
            bzlmod_cache.as_deref(),
        ) {
            Ok(v) => v,
            Err(e) => {
                return SharedResult::err(format!(
                    "ModuleExtensionKey({self}): extension working dir: {e:#}"
                ));
            }
        };

        // HTTP client + runtime handle for `module_ctx.download` /
        // `.download_and_extract`. Falls back to a fresh
        // `Client::new()` when none was wired through
        // `EngineBuilder::with_http_client(...)` — matches the BCR
        // fetch path's behaviour.
        let client_for_eval = {
            use loupe_bzlmod::dice::HasHttpClient;
            ctx.global_data().http_client().unwrap_or_default()
        };
        let handle_for_eval = tokio::runtime::Handle::current();

        // Lazy materialise bridge: an extension impl can call
        // `module_ctx.path(Label("@external_repo//..."))` mid-eval.
        // Channel-actor pattern: blocking-thread eval blocks on the
        // resolver; async parent fields the request, recursively
        // computes the child `MaterializedRepoKey`, sends the path
        // back. See `MaterializedRepoKey::compute` for the original.
        let (req_tx, mut req_rx) =
            tokio::sync::mpsc::channel::<loupe_bzlmod::label_resolver::LabelReq>(16);
        let (resp_tx, resp_rx) =
            tokio::sync::mpsc::channel::<loupe_bzlmod::label_resolver::LabelResp>(16);
        let resolver: Arc<dyn loupe_bzlmod::label_resolver::LabelResolver> =
            Arc::new(loupe_bzlmod::label_resolver::ChannelLabelResolver {
                req_tx,
                resp_rx: Arc::new(tokio::sync::Mutex::new(resp_rx)),
            });

        // 5. Run the impl on a blocking thread (Starlark eval can be
        //    long-running and panicky; isolate it from the tokio
        //    worker pool).
        let module_for_eval = loaded.module.clone();
        let mapping_for_eval = Arc::clone(&mapping);
        let bzlmod_cache_for_eval = bzlmod_cache.clone();
        let workspace_root_for_eval = workspace_root.clone();
        let extension_wd_for_eval = extension_wd.clone();
        let declared_env_for_eval = declared_env.clone();
        let resolver_for_eval = Arc::clone(&resolver);

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

        let eval_result = loop {
            tokio::select! {
                biased;
                Some(req) = req_rx.recv() => {
                    // `maybe_materialise` covers both the static-spec
                    // path (`MaterializedRepoKey`) AND the lazy
                    // extension-generated path (`ExtMaterializedRepoKey`
                    // routed via `RepoOwnerIndexKey`). The latter is
                    // what `@coreutils_darwin_arm64` (an
                    // ext-generated repo under `bazel_lib`'s
                    // toolchains extension) needs.
                    let path_result: loupe_bzlmod::label_resolver::LabelResp =
                        match maybe_materialise(ctx, &req.canonical).await {
                            Ok(Some(crate::repo_root::RepoRoot::Materialised { dir, .. })) => {
                                let mut p = dir;
                                if !req.package.is_empty() {
                                    p = p.join(&req.package);
                                }
                                if !req.name.is_empty() {
                                    p = p.join(&req.name);
                                }
                                Ok(p)
                            }
                            Ok(_) => Err(format!(
                                "label resolve `@@{}//{}:{}`: no RepoSpec registered for canonical `{}` \
                                 (did the engine builder load MODULE.bazel.lock?)",
                                req.canonical, req.package, req.name, req.canonical
                            )),
                            Err(e) => Err(format!(
                                "label resolve `@@{}//{}:{}`: {e}",
                                req.canonical, req.package, req.name
                            )),
                        };
                    let _ = resp_tx.send(path_result).await;
                }
                r = &mut eval_handle => break r,
            }
        };

        // Hold the tempdir guard until eval finishes so the extension
        // working directory doesn't get reaped mid-impl.
        drop(_td_guard);

        let output = match eval_result {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return SharedResult::err(format!(
                    "ModuleExtensionKey({self}): module_extension eval: {e}"
                ));
            }
            Err(e) => {
                return SharedResult::err(format!("ModuleExtensionKey({self}): eval join: {e}"));
            }
        };

        // 6. Promote captured repo calls into per-canonical `RepoSpec`s
        //    matching Bazel's `<owner_canonical><sep><ext><sep><repo>`
        //    shape. Mirrors `engine.rs::build_module_extension_specs`.
        let mut specs = BTreeMap::new();
        let mut ext_records = Vec::new();
        for call in output.repos {
            let canonical = format!(
                "{}{}{}{}{}",
                inputs.owner_canonical, inputs.sep, ext_name, inputs.sep, call.name
            );
            let merged_attrs = crate::engine::merge_repo_rule_defaults(
                call.rule_fv,
                &canonical,
                &defining_bzl,
                call.attrs,
            );
            ext_records.push(loupe_bzlmod::bzlmod_graph::ExtGenRecord {
                canonical: canonical.clone(),
                owner_canonical: inputs.owner_canonical.clone(),
                ext_name: ext_name.clone(),
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

        SharedResult::ok(ModuleExtensionEvalOutput {
            specs,
            ext_generated: ext_records,
            registered_toolchains: output.registered_toolchains,
        })
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Materialise an extension-generated repo: look up its owning
/// `ExtensionId` via [`RepoOwnerIndexKey`], force the owning
/// [`ModuleExtensionKey`] to obtain the captured `RepoSpec`, then run
/// `loupe_bzlmod::materialize::materialize(...)` against it.
///
/// Parallel to `loupe_bzlmod::MaterializedRepoKey` but for the
/// extension-output path: that key reads from the injected
/// `RepoSpecsKey`, which only carries static specs. Specs captured
/// inside an extension's impl never land there (avoids the
/// "InjectedKey mutated mid-compute" hazard); they live in the
/// extension key's `Value` instead.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ExtMaterializedRepoKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ExtMaterializedRepoKey(pub String);

#[async_trait]
impl Key for ExtMaterializedRepoKey {
    type Value = SharedResult<PathBuf>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let canonical = self.0.clone();

        // Look up which extension owns this canonical.
        let demand = match force_extension_demand(ctx).await {
            Ok(d) => d,
            Err(e) => {
                return SharedResult::err(format!("ExtMaterializedRepoKey({canonical}): {e}"));
            }
        };
        let Some(ext_id) = demand.owner_of(&canonical) else {
            return SharedResult::err(format!(
                "ExtMaterializedRepoKey({canonical}): no owner registered"
            ));
        };

        // Force the owning extension.
        let ext_out = match ctx.compute(&ModuleExtensionKey(ext_id.clone())).await {
            Ok(SharedResult::Ok(o)) => o,
            Ok(SharedResult::Err(e)) => {
                return SharedResult::err(format!(
                    "ExtMaterializedRepoKey({canonical}): owning extension {ext_id} failed: {}",
                    Arc::unwrap_or_clone(e)
                ));
            }
            Err(e) => {
                return SharedResult::err(format!("ExtMaterializedRepoKey({canonical}): {e}"));
            }
        };

        let Some(spec) = ext_out.specs.get(&canonical).cloned() else {
            return SharedResult::err(format!(
                "ExtMaterializedRepoKey({canonical}): extension {ext_id} ran but did not \
                 declare this repo (use_repo'd a name the impl never produced)"
            ));
        };

        use loupe_bzlmod::HasBzlmodCache;
        use loupe_bzlmod::dice::HasDeclaredEnv;
        use loupe_bzlmod::dice::HasHttpClient;
        let Some(cache) = ctx.global_data().bzlmod_cache().map(Path::to_path_buf) else {
            return SharedResult::err(format!(
                "ExtMaterializedRepoKey({canonical}): no bzlmod cache configured \
                 (EngineBuilder::with_bzlmod_cache)"
            ));
        };
        let Some(client) = ctx.global_data().http_client() else {
            return SharedResult::err(format!(
                "ExtMaterializedRepoKey({canonical}): no http client configured \
                 (EngineBuilder::with_http_client)"
            ));
        };
        let env = ctx.global_data().declared_env();
        let workspace_root = Some(ctx.global_data().workspace_root().to_path_buf());

        // Channel-actor lazy materialise: ext-generated repo rules
        // (rules_js's `npm_import`) call
        // `rctx.path(Label("@coreutils_darwin_arm64//:coreutils"))`
        // mid-impl. Bridge the blocking-thread eval's resolver
        // requests back to DICE's `maybe_materialise` — same pattern
        // as `MaterializedRepoKey::compute` and
        // `ModuleExtensionKey::compute`.
        let (req_tx, mut req_rx) =
            tokio::sync::mpsc::channel::<loupe_bzlmod::label_resolver::LabelReq>(16);
        let (resp_tx, resp_rx) =
            tokio::sync::mpsc::channel::<loupe_bzlmod::label_resolver::LabelResp>(16);
        let resolver: Arc<dyn loupe_bzlmod::label_resolver::LabelResolver> =
            Arc::new(loupe_bzlmod::label_resolver::ChannelLabelResolver {
                req_tx,
                resp_rx: Arc::new(tokio::sync::Mutex::new(resp_rx)),
            });

        let mat_fut = loupe_bzlmod::materialize::materialize(
            &client,
            &cache,
            &spec,
            env,
            workspace_root,
            resolver,
        );
        tokio::pin!(mat_fut);

        let result = loop {
            tokio::select! {
                biased;
                Some(req) = req_rx.recv() => {
                    let path_result: loupe_bzlmod::label_resolver::LabelResp =
                        match maybe_materialise(ctx, &req.canonical).await {
                            Ok(Some(crate::repo_root::RepoRoot::Materialised { dir, .. })) => {
                                let mut p = dir;
                                if !req.package.is_empty() {
                                    p = p.join(&req.package);
                                }
                                if !req.name.is_empty() {
                                    p = p.join(&req.name);
                                }
                                Ok(p)
                            }
                            Ok(_) => Err(format!(
                                "label resolve `@@{}//{}:{}`: no RepoSpec registered",
                                req.canonical, req.package, req.name
                            )),
                            Err(e) => Err(format!(
                                "label resolve `@@{}//{}:{}`: {e}",
                                req.canonical, req.package, req.name
                            )),
                        };
                    let _ = resp_tx.send(path_result).await;
                }
                r = &mut mat_fut => break r,
            }
        };

        match result {
            Ok(p) => SharedResult::ok(p),
            Err(e) => SharedResult::err(format!("ExtMaterializedRepoKey({canonical}): {e}")),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}
