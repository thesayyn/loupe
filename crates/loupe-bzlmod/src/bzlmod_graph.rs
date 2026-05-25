//! Resolved bzlmod state — the single in-memory shape every
//! downstream consumer needs.
//!
//! Two producers populate it:
//!
//! - [`scan_vendor`] for the `bazel mod vendor` flow (everything's
//!   already on disk under `<vendor>/<canonical>/`).
//! - [`resolve_bzlmod`] for the in-process resolver (drives BCR + the
//!   module-extension impls and assembles records from their output).
//!
//! Both produce the same shape. `ModuleGraphKey` reads it from
//! `DiceData` and synthesises `ModuleGraph` purely in memory — no
//! `read_dir`, no race against lazy materialisation.
//!
//! The user-visible "two paths" lives at the engine-builder level
//! (which producer ran); everything below treats it as one
//! representation.

use std::path::Path;
use std::sync::Arc;

use allocative::Allocative;
use dice::DiceTransactionUpdater;
use dice::{DiceComputations, InjectedKey, NoValueSerialize, ValueSerialize};
use pagable::{Pagable, pagable_typetag};

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

/// Walk a `bazel mod vendor`-style directory and assemble the
/// `BzlmodGraph` from what's on disk.
///
/// - `workspace` is the main workspace root; its `MODULE.bazel` is
///   parsed for [`BzlmodGraph::main`].
/// - `vendor` is the directory holding `<canonical>/` subdirs (the
///   output of `bazel mod vendor`). Subdirs with a `MODULE.bazel`
///   become [`ModuleRecord`]s in `transitive`; subdirs without one
///   whose name matches the ext-generated convention
///   (`<owner><sep><sep><ext><sep><repo>`) become [`ExtGenRecord`]s.
///
/// The vendor flow is what `--external-repo-root` users hit. No
/// network, no extension evaluation — every byte is already on disk.
pub fn scan_vendor(workspace: &Path, vendor: &Path) -> anyhow::Result<BzlmodGraph> {
    use anyhow::Context as _;

    let main_bytes = std::fs::read(workspace.join("MODULE.bazel"))
        .with_context(|| format!("reading {}/MODULE.bazel", workspace.display()))?;
    let main_src = String::from_utf8_lossy(&main_bytes);
    let main_parsed =
        parse_module_bazel(&main_src, false).context("parsing workspace MODULE.bazel")?;
    let main_name = main_parsed.name.unwrap_or_default();
    let main_version = main_parsed.version.unwrap_or_default();

    let sep = detect_separator_from_dir(vendor);

    let main = ModuleRecord {
        name: main_name.clone(),
        version: main_version,
        canonical: String::new(), // main canonical is empty by convention
        is_root: true,
        bazel_deps: main_parsed
            .bazel_deps
            .into_iter()
            .map(|d| BazelDepRecord {
                canonical: canonicalise_bazel_dep(&d.name, sep),
                name: d.name,
                apparent: d.apparent,
                version: d.version,
                dev_dependency: d.dev_dependency,
            })
            .collect(),
        use_extensions: main_parsed.use_extensions,
        registered_toolchains: main_parsed.registered_toolchains,
        registered_execution_platforms: main_parsed.registered_execution_platforms,
    };

    let mut transitive: Vec<ModuleRecord> = Vec::new();
    let mut ext_generated: Vec<ExtGenRecord> = Vec::new();
    let mut subdirs: Vec<String> = std::fs::read_dir(vendor)
        .with_context(|| format!("reading {}", vendor.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    subdirs.sort();
    let dbl = format!("{sep}{sep}");
    for canonical in subdirs {
        let mb = vendor.join(&canonical).join("MODULE.bazel");
        if let Ok(bytes) = std::fs::read(&mb) {
            let src = String::from_utf8_lossy(&bytes);
            let Ok(parsed) = parse_module_bazel(&src, true) else {
                continue;
            };
            transitive.push(ModuleRecord {
                name: parsed.name.unwrap_or_default(),
                version: parsed.version.unwrap_or_default(),
                canonical: canonical.clone(),
                is_root: false,
                bazel_deps: parsed
                    .bazel_deps
                    .into_iter()
                    .map(|d| BazelDepRecord {
                        canonical: canonicalise_bazel_dep(&d.name, sep),
                        name: d.name,
                        apparent: d.apparent,
                        version: d.version,
                        dev_dependency: d.dev_dependency,
                    })
                    .collect(),
                use_extensions: parsed.use_extensions,
                registered_toolchains: parsed.registered_toolchains,
                registered_execution_platforms: parsed.registered_execution_platforms,
            });
            continue;
        }
        // No MODULE.bazel — try to classify as an ext-generated
        // repo by canonical-name structure.
        if let Some(rec) = classify_ext_gen(&canonical, &dbl, sep) {
            ext_generated.push(rec);
        }
    }

    Ok(BzlmodGraph {
        main,
        transitive,
        ext_generated,
        sep,
    })
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

/// Lenient-mode parser shared by `scan_vendor` and resolver-side
/// callers. Captures every directive a `MODULE.bazel` declares:
/// `module(...)` identity, `bazel_dep(...)`, `use_extension(...)`,
/// `register_toolchains(...)`, `register_execution_platforms(...)`,
/// `use_repo(...)`. Unknown directives become no-ops so the rest of
/// the file still parses (real-world `MODULE.bazel`s call
/// `single_version_override`, `override_repo`, etc. — we don't
/// model those for graph synthesis).
///
/// `lenient=false` propagates parse errors; `lenient=true` swallows
/// them and returns whatever the collector captured before the
/// error. Vendor-flow callers go lenient because BCR-published
/// modules sometimes call Bazel surface we don't model.
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

/// Classify a directory under a vendor root by its canonical name's
/// shape. Returns `Some` for ext-generated repos (matching
/// `<owner><sep><sep><ext><sep><repo>` or the well-known-owner
/// single-sep variant); returns `None` for anything that doesn't
/// look extension-generated (the caller skips those).
fn classify_ext_gen(name: &str, dbl: &str, sep: char) -> Option<ExtGenRecord> {
    // Doubled-separator form: <name><sep><sep><ext><sep><repo>.
    if let Some((owner_short, rest)) = name.split_once(dbl) {
        let owner_canonical = format!("{owner_short}{sep}");
        let (ext_name, repo_short) = rest.rsplit_once(sep)?;
        return Some(ExtGenRecord {
            canonical: name.to_string(),
            owner_canonical,
            ext_name: ext_name.to_string(),
            repo_short: repo_short.to_string(),
        });
    }
    // Well-known-owner single-sep form (no doubling because the
    // owner has no separator suffix).
    for known in ["platforms", "bazel_tools", "local_config_platform"] {
        let prefix = format!("{known}{sep}");
        if let Some(rest) = name.strip_prefix(&prefix)
            && let Some((ext_name, repo_short)) = rest.split_once(sep)
        {
            return Some(ExtGenRecord {
                canonical: name.to_string(),
                owner_canonical: known.to_string(),
                ext_name: ext_name.to_string(),
                repo_short: repo_short.to_string(),
            });
        }
    }
    None
}

/// Best-effort separator detection from a vendor dir's contents.
/// Looks for any subdir name ending in `~` → Bazel 7-era; otherwise
/// defaults to `+`. Matches the heuristic in
/// `loupe-core::lockfile::parse_lockfile`.
fn detect_separator_from_dir(dir: &Path) -> char {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return '+';
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(s) = name.to_str() else { continue };
        // Sep is the trailing byte of a module canonical (e.g.
        // `rules_oci~`). Skip `<owner><sep><sep>...` subdirs by
        // requiring the trailing char to be a non-doubled separator.
        if s.ends_with('~') && !s.contains("~~") {
            return '~';
        }
    }
    '+'
}
