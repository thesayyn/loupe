//! Resolved bzlmod state — the single in-memory shape every
//! downstream consumer needs.
//!
//! Populated by [`resolve_bzlmod`]: BCR resolution plus the
//! module-extension impls, assembled into records from their output.
//! `ModuleGraphKey` reads it from `DiceData` and synthesises
//! `ModuleGraph` purely in memory — no `read_dir`, no race against
//! lazy materialisation.

use std::sync::Arc;

use allocative::Allocative;
use dice::DiceTransactionUpdater;
use dice::{DiceComputations, InjectedKey, NoValueSerialize, ValueSerialize};
use pagable::{Pagable, pagable_typetag};

use crate::resolution::RepoRuleInvocation;
use crate::resolution::UseExtensionLite;

/// The full bzlmod state of a workspace, source-agnostic.
///
/// `main` is the workspace's own `MODULE.bazel`. `transitive` is
/// every other module reached through `bazel_dep` (BCR-resolved or
/// vendored). `ext_generated` is every repo a module extension
/// produced, addressed by its canonical name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Allocative)]
pub struct BzlmodGraph {
    pub main: ModuleRecord,
    pub transitive: Vec<ModuleRecord>,
    pub ext_generated: Vec<ExtGenRecord>,
    /// Canonical separator (`+` Bazel 7+ default; `~` for older
    /// `lockFileVersion: 11` workspaces). Detected once at producer
    /// time; downstream callers consult `bzlmod_graph.sep` instead of
    /// re-deriving it. `char` doesn't impl `Allocative`; skipped from
    /// the visitor since it's a single Unicode scalar (negligible).
    #[allocative(skip)]
    pub sep: char,
}

impl BzlmodGraph {
    /// All module canonicals (main + transitive) for quick membership
    /// checks. Ext-generated repos are intentionally excluded — they
    /// aren't modules.
    pub fn module_canonicals(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.main.canonical.as_str())
            .chain(self.transitive.iter().map(|m| m.canonical.as_str()))
    }
}

/// One `MODULE.bazel`'s parsed contents — everything Bazel's bzlmod
/// surface declares: identity, deps, extensions used, toolchains and
/// execution platforms registered.
///
/// Used uniformly for main and transitive modules. The `is_root`
/// field flags the main one so consumers honoring root-only semantics
/// (`use_extension(..., dev_dependency=True)`) can filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Allocative)]
pub struct ModuleRecord {
    pub name: String,
    pub version: String,
    pub canonical: String,
    pub is_root: bool,
    pub bazel_deps: Vec<BazelDepRecord>,
    pub use_extensions: Vec<UseExtensionLite>,
    /// `use_repo_rule(...)` proxy invocations from this module's
    /// MODULE.bazel — module-local repos materialised via the
    /// innate-extension path.
    pub repo_rule_invocations: Vec<RepoRuleInvocation>,
    pub registered_toolchains: Vec<String>,
    pub registered_execution_platforms: Vec<String>,
}

/// One `bazel_dep(...)` declaration from a `MODULE.bazel`, post-
/// canonicalisation. `name` is the bzlmod module name (drives the
/// canonical); `apparent` is what BUILD/`.bzl` code references it as
/// (`repo_name=` override or `name` by default); `canonical` is the
/// resolved on-disk name (`<name><sep>` for regular modules, bare
/// for well-known like `platforms`).
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct BazelDepRecord {
    pub name: String,
    pub apparent: String,
    pub version: String,
    pub canonical: String,
    pub dev_dependency: bool,
}

/// One repo a module extension generated. Canonical follows Bazel's
/// `<owner_canonical><sep><sep><ext_name><sep><repo_short>` shape;
/// `owner_canonical` is the canonical of the *defining* module
/// (`aspect_bazel_lib~`) regardless of which module consumed the
/// extension. `repo_short` is the name the extension's impl passed
/// to its repo_rule constructor.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct ExtGenRecord {
    pub canonical: String,
    pub owner_canonical: String,
    pub ext_name: String,
    pub repo_short: String,
}

/// Marker for `DiceData`: hold the engine's resolved `BzlmodGraph`.
/// Set once at engine-build time, never mutated — `ModuleGraphKey`'s
/// compute reads it as authoritative state. Wrap the inner type in
/// `Arc` so reads are cheap.
#[derive(Clone, Default)]
pub struct ResolvedBzlmod(pub Arc<BzlmodGraph>);

impl allocative::Allocative for ResolvedBzlmod {
    fn visit<'a, 'b: 'a>(&self, _visitor: &'a mut allocative::Visitor<'b>) {}
}

impl std::fmt::Debug for ResolvedBzlmod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedBzlmod")
            .field("modules", &self.0.transitive.len())
            .field("ext_generated", &self.0.ext_generated.len())
            .finish()
    }
}

pub trait HasResolvedBzlmod {
    fn resolved_bzlmod(&self) -> Option<Arc<BzlmodGraph>>;
}

impl HasResolvedBzlmod for dice::DiceData {
    fn resolved_bzlmod(&self) -> Option<Arc<BzlmodGraph>> {
        self.get::<ResolvedBzlmod>().ok().map(|r| Arc::clone(&r.0))
    }
}

pub trait SetResolvedBzlmod {
    fn set_resolved_bzlmod(&mut self, graph: BzlmodGraph);
}

impl SetResolvedBzlmod for dice::DiceDataBuilder {
    fn set_resolved_bzlmod(&mut self, graph: BzlmodGraph) {
        self.set(ResolvedBzlmod(Arc::new(graph)));
    }
}

/// Singleton `InjectedKey` carrying the resolved [`BzlmodGraph`].
/// Set initially via [`inject_bzlmod_graph`] and refreshed when
/// extension eval produces new ext-generated repos (those change
/// `ext_generated`, which downstream `ModuleGraphKey` reads to
/// populate `per_repo_mapping`).
///
/// Equality is **structural** on the inner [`BzlmodGraph`]: a fresh
/// `inject_bzlmod_graph` with logically-identical contents (e.g. a
/// no-op rebuild between extension-eval rounds) does NOT invalidate
/// downstream. Pointer-identity equality used to live here and caused
/// a fetch hang — every `apply_bzlmod_snapshot` allocated a fresh
/// `Arc<BzlmodGraph>` whose pointer differed from the previous one,
/// cascading into a re-materialisation storm of every `BzlLoadKey` /
/// `MaterializedRepoKey` even when the underlying graph was unchanged.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BzlmodGraphKey")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct BzlmodGraphKey;

impl InjectedKey for BzlmodGraphKey {
    type Value = Option<Arc<BzlmodGraph>>;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        // Structural compare via `Arc::deref` → `BzlmodGraph::PartialEq`.
        // Cheap when the Arcs match by pointer (early-out is handled
        // by the standard Arc equality impl).
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Force [`BzlmodGraphKey`]. Returns `None` when no graph has been
/// injected yet (test/direct-Dice callers that build a `Dice` without
/// going through `EngineBuilder::build`).
pub async fn force_bzlmod_graph(
    ctx: &mut DiceComputations<'_>,
) -> Result<Option<Arc<BzlmodGraph>>, String> {
    ctx.compute(&BzlmodGraphKey)
        .await
        .map_err(|e| format!("BzlmodGraphKey: {e}"))
}

/// Inject `graph` as the current value of [`BzlmodGraphKey`] on
/// `updater`. The caller must `commit().await` the updater to publish
/// the value. Pass `None` to clear (e.g. the no-`EngineBuilder` test
/// fallback).
pub fn inject_bzlmod_graph(
    updater: &mut DiceTransactionUpdater,
    graph: Option<BzlmodGraph>,
) -> Result<(), String> {
    updater
        .changed_to([(BzlmodGraphKey, graph.map(Arc::new))])
        .map_err(|e| format!("inject_bzlmod_graph: {e}"))
}

/// Drive BCR resolution + extension evaluation and assemble the
/// `BzlmodGraph`. The actual fetching/eval is in `resolution.rs`
/// and the engine builder; this function is a wrapper that takes the
/// pieces those steps produce and stitches them into the unified
/// shape.
///
/// `main` and `transitive` come from the resolver; `ext_generated`
/// comes from extension evaluation's repo-call captures.
pub fn assemble_bzlmod_graph(
    main: ModuleRecord,
    transitive: Vec<ModuleRecord>,
    ext_generated: Vec<ExtGenRecord>,
    sep: char,
) -> BzlmodGraph {
    BzlmodGraph {
        main,
        transitive,
        ext_generated,
        sep,
    }
}

/// Lenient-mode parser used by the resolver. Captures every directive a `MODULE.bazel` declares:
/// `module(...)` identity, `bazel_dep(...)`, `use_extension(...)`,
/// `register_toolchains(...)`, `register_execution_platforms(...)`,
/// `use_repo(...)`. Unknown directives become no-ops so the rest of
/// the file still parses (real-world `MODULE.bazel`s call
/// `single_version_override`, `override_repo`, etc. — we don't
/// model those for graph synthesis).
///
/// `lenient=false` propagates parse errors; `lenient=true` swallows
/// them and returns whatever the collector captured before the
/// error. The resolver goes lenient for transitive modules because
/// BCR-published `MODULE.bazel`s sometimes call Bazel surface we don't
/// model.
pub fn parse_module_bazel(src: &str, lenient: bool) -> anyhow::Result<ParsedModuleBazel> {
    crate::resolution::parse_module_bazel_impl(src, lenient)
}

/// Output of [`parse_module_bazel`].
#[derive(Debug, Default)]
pub struct ParsedModuleBazel {
    pub name: Option<String>,
    pub version: Option<String>,
    pub bazel_deps: Vec<ParsedBazelDep>,
    pub use_extensions: Vec<UseExtensionLite>,
    pub repo_rule_invocations: Vec<RepoRuleInvocation>,
    pub registered_toolchains: Vec<String>,
    pub registered_execution_platforms: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedBazelDep {
    pub name: String,
    pub version: String,
    pub apparent: String,
    pub dev_dependency: bool,
}

/// Bazel's bazel_dep canonical-naming rule: `<name><sep>` except
/// well-known modules (`bazel_tools`, `platforms`,
/// `local_config_platform`) keep their bare name. Mirrors
/// `loupe-core::module_graph::canonicalise_with_sep`.
pub fn canonicalise_bazel_dep(name: &str, sep: char) -> String {
    if matches!(name, "bazel_tools" | "platforms" | "local_config_platform") {
        name.to_string()
    } else {
        format!("{name}{sep}")
    }
}

