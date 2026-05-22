//! Transitive `bazel_dep` resolution against the BCR registry.
//!
//! v1 algorithm (lightweight MVS):
//!
//! 1. Start with the workspace's `MODULE.bazel` parsed into a list of
//!    `bazel_dep` declarations.
//! 2. For each declared dep, fetch its `MODULE.bazel` from the
//!    registry; collect its own `bazel_dep` calls; recurse.
//! 3. Across the resulting graph, for each module name, pick the
//!    **maximum requested version** (semver-shaped via [`Version`]).
//!    Conflicting requirements that can't both be satisfied by the max
//!    are accepted — analysis fails downstream rather than at
//!    resolution time, which matches Bazel's "permissive MVS" stance.
//! 4. For each selected `(name, version)`, fetch `source.json` and
//!    build a `RepoSpec::Http`.
//!
//! Skips well-known modules (`bazel_tools`, `platforms`, …) — Bazel
//! embeds those and loupe serves them through the embedded tree or
//! user-supplied vendor.
//!
//! Out of scope for v1: `*_override` directives (only the workspace's
//! main MODULE.bazel parser captures them today, not the parser used
//! here for transitive MODULE.bazels), dev-dependency filtering, MVS
//! tie-breaking by registry ordering.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use crate::archive::ArchiveKind;
use crate::registry::Registry;
use crate::registry::RegistryError;
use crate::registry::fetch_module_bazel;
use crate::registry::fetch_source;
use crate::repo_spec::RepoSpec;
use crate::repo_spec::RepoSpecKind;
use crate::starlark_eval::RepoAttrValue;
use crate::version::Version;

/// One `bazel_dep(name=, version=)` declaration. The caller hands the
/// resolver a `Vec<BazelDepRequest>` for the workspace's main module;
/// the resolver discovers the rest by walking the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BazelDepRequest {
    pub name: String,
    pub version: String,
}

/// Modules Bazel embeds rather than fetching from a registry. Loupe
/// expects these to come through the embedded root tree or a vendor
/// directory.
pub const WELL_KNOWN_MODULES: &[&str] = &["bazel_tools", "platforms", "local_config_platform"];

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),

    #[error("parsing MODULE.bazel for `{name}@{version}`: {source}")]
    ParseModule {
        name: String,
        version: String,
        #[source]
        source: anyhow::Error,
    },
}

#[derive(Debug, Clone)]
pub struct ResolvedModule {
    pub name: String,
    pub version: String,
    pub canonical: String,
    pub spec: RepoSpec,
}

/// One transitive module's contribution to extension evaluation:
/// what `bazel_dep`s it declares (for canonicalizing its
/// `use_extension` bzl_labels) and what extensions it invokes.
#[derive(Debug, Clone)]
pub struct TransitiveModuleInfo {
    pub name: String,
    pub version: String,
    pub canonical: String,
    /// Apparent → canonical from this module's `bazel_dep` calls.
    /// Used by callers to canonicalize `use_extension` bzl_labels
    /// against this module's perspective.
    pub bazel_deps: Vec<(String, String)>,
    /// `use_extension(...) + ext.<class>(...)` calls from this
    /// module's MODULE.bazel.
    pub use_extensions: Vec<UseExtensionLite>,
    /// Raw `register_toolchains(...)` label strings — captured
    /// verbatim from the module's MODULE.bazel so the engine can
    /// canonicalise them against this module's own repo mapping. Bazel
    /// honours transitive `register_toolchains` from every loaded
    /// module; we feed these into `ModuleGraph.registered_toolchains`
    /// at engine-build time so resolution doesn't depend on the
    /// `.loupe/bzlmod/<canonical>` tree being fully materialised first.
    pub registered_toolchains: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct UseExtensionLite {
    pub bzl_label: String,
    pub name: String,
    pub tags: Vec<TagInvocationLite>,
    /// `use_extension(..., dev_dependency = True)` — per bzlmod spec,
    /// dev-only extensions from non-root modules are ignored at
    /// resolution time. The captured tag invocations stay on the
    /// record for diagnostics, but engine-side wiring drops the whole
    /// record when this flag is set on a non-root module.
    pub dev_dependency: bool,
    /// Names captured from `use_repo(<proxy>, name, ...)` calls bound
    /// to this extension. Engine-side wiring synthesises
    /// `name → <owner><sep><ext_name><sep><name>` apparent mappings so
    /// label resolution inside the declaring module's `.bzl` files
    /// can reach ext-generated repos.
    pub use_repo_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TagInvocationLite {
    pub class: String,
    pub attrs: BTreeMap<String, RepoAttrValue>,
}

/// Output of [`resolve`] — selected modules + the per-module
/// extension-usage info needed to wire transitive tags into
/// `module_extension` evaluation.
#[derive(Debug, Clone, Default)]
pub struct ResolveOutput {
    pub modules: BTreeMap<String, ResolvedModule>,
    pub transitive: Vec<TransitiveModuleInfo>,
}

/// Walk the dep graph rooted at `roots`, run MVS, build a
/// canonical → RepoSpec map. The canonical separator (`+` for Bazel
/// 7+) controls the canonical-name shape — same convention as
/// loupe-core's `module_graph::canonicalise_with_sep`.
pub async fn resolve(
    client: &reqwest::Client,
    cache_root: &Path,
    registry: &Registry,
    roots: Vec<BazelDepRequest>,
    sep: char,
) -> Result<BTreeMap<String, ResolvedModule>, ResolveError> {
    // Compat path: callers that only need the modules map call this
    // shape; the full output with transitive tags is via `resolve_full`.
    Ok(resolve_full(client, cache_root, registry, roots, sep)
        .await?
        .modules)
}

/// Like [`resolve`] but also returns each transitive module's
/// `bazel_dep` and `use_extension` records, so callers can plumb
/// transitive-module tags into `module_extension` evaluation.
pub async fn resolve_full(
    client: &reqwest::Client,
    cache_root: &Path,
    registry: &Registry,
    roots: Vec<BazelDepRequest>,
    sep: char,
) -> Result<ResolveOutput, ResolveError> {
    let mut requests: BTreeMap<String, BTreeSet<Version>> = BTreeMap::new();
    let mut visited: BTreeSet<(String, String)> = BTreeSet::new();
    // (module_name, version) → parsed directives. We keep all visited
    // versions and pick the selected one's directives at the end.
    let mut parsed_modules: BTreeMap<(String, String), ParsedModule> = BTreeMap::new();
    let mut queue: Vec<BazelDepRequest> = roots;

    while let Some(req) = queue.pop() {
        // `bazel_tools` is embedded (no fetch), but `platforms` /
        // `local_config_platform` are *not* — Bazel just keeps their
        // canonical names bare. Skipping the fetch leaves them
        // unmaterialised, so a fixture that references
        // `@platforms//os:macos` blows up. Fetch them like any other
        // module; `canonicalise` below still produces the bare name.
        if req.name == "bazel_tools" || req.name == "local_config_platform" {
            continue;
        }
        requests
            .entry(req.name.clone())
            .or_default()
            .insert(Version::parse(&req.version));
        let key = (req.name.clone(), req.version.clone());
        if !visited.insert(key.clone()) {
            continue;
        }
        let bytes =
            fetch_module_bazel(client, cache_root, registry, &req.name, &req.version).await?;
        let src = String::from_utf8_lossy(&bytes);
        let parsed = parse_module_directives(&src).map_err(|e| ResolveError::ParseModule {
            name: req.name.clone(),
            version: req.version.clone(),
            source: e,
        })?;
        for dep in &parsed.bazel_deps {
            // Per bzlmod spec: `dev_dependency=True` is honoured
            // only for the root module. `resolve_full` only walks
            // *transitive* modules, so every dep here is from a
            // non-root context — drop dev_deps to avoid fetching
            // dev-only overridden modules (e.g. `bzlmod_lock_files`
            // declared by `rules_jvm_external@6.6` paired with a
            // `local_path_override`) that BCR doesn't host.
            if dep.dev_dependency {
                continue;
            }
            queue.push(BazelDepRequest {
                name: dep.name.clone(),
                version: dep.version.clone(),
            });
        }
        parsed_modules.insert(key, parsed);
    }

    // MVS: pick the max requested version per module.
    let mut selected: BTreeMap<String, String> = BTreeMap::new();
    for (name, versions) in &requests {
        let max = versions.iter().max().expect("non-empty set");
        selected.insert(name.clone(), max.as_str().to_string());
    }

    let mut modules: BTreeMap<String, ResolvedModule> = BTreeMap::new();
    let mut transitive: Vec<TransitiveModuleInfo> = Vec::new();
    for (name, version) in selected {
        let canonical = canonicalise(&name, sep);
        let source = fetch_source(client, cache_root, registry, &name, &version).await?;
        let archive_kind = ArchiveKind::from_url(&source.url);
        let spec = RepoSpec {
            canonical: canonical.clone(),
            kind: RepoSpecKind::Http {
                urls: vec![source.url],
                integrity: source.integrity,
                strip_prefix: source.strip_prefix,
                archive_kind,
            },
        };
        modules.insert(
            canonical.clone(),
            ResolvedModule {
                name: name.clone(),
                version: version.clone(),
                canonical: canonical.clone(),
                spec,
            },
        );
        if let Some(parsed) = parsed_modules.remove(&(name.clone(), version.clone())) {
            // Canonicalise the bazel_deps to the selected sep.
            let bazel_deps_canonical: Vec<(String, String)> = parsed
                .bazel_deps
                .iter()
                .map(|d| (d.apparent.clone(), canonicalise(&d.name, sep)))
                .collect();
            transitive.push(TransitiveModuleInfo {
                name,
                version,
                canonical,
                bazel_deps: bazel_deps_canonical,
                use_extensions: parsed.use_extensions,
                registered_toolchains: parsed.registered_toolchains,
            });
        }
    }
    Ok(ResolveOutput {
        modules,
        transitive,
    })
}

/// Bazel's canonical-naming rule: regular modules get `<name><sep>`;
/// well-known modules keep their bare name. Mirrors
/// loupe-core's `canonicalise_with_sep` so registry-resolved canonicals
/// match the workspace's bazel_dep-derived ones.
fn canonicalise(name: &str, sep: char) -> String {
    if WELL_KNOWN_MODULES.contains(&name) {
        return name.to_string();
    }
    format!("{name}{sep}")
}

/// Slot-indexed proxy returned by `use_extension(...)` in the
/// transitive-MODULE.bazel parser. `proxy.tag_class(...)` invocations
/// record into `Collector.use_extensions[slot].tags`. `use_repo(ext, ...)`
/// is accepted but ignored at this layer (the workspace's main
/// module's `use_repo` is what wires the consuming view).
#[derive(
    Debug,
    Clone,
    allocative::Allocative,
    starlark::any::ProvidesStaticType,
    derive_more::Display,
    starlark::values::NoSerialize,
)]
#[display("<resolution-ext-proxy slot={}>", slot)]
struct ResolutionExtProxy {
    slot: usize,
}

starlark::starlark_simple_value!(ResolutionExtProxy);

#[starlark::values::starlark_value(type = "resolution_ext_proxy")]
impl<'v> starlark::values::StarlarkValue<'v> for ResolutionExtProxy {
    fn get_attr(
        &self,
        name: &str,
        heap: starlark::values::Heap<'v>,
    ) -> Option<starlark::values::Value<'v>> {
        Some(heap.alloc(ResolutionTagCallable {
            slot: self.slot,
            class: name.to_string(),
        }))
    }

    fn invoke(
        &self,
        _me: starlark::values::Value<'v>,
        _args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<starlark::values::Value<'v>> {
        Ok(eval.heap().alloc(ResolutionExtProxy { slot: self.slot }))
    }
}

#[derive(
    Debug,
    Clone,
    allocative::Allocative,
    starlark::any::ProvidesStaticType,
    derive_more::Display,
    starlark::values::NoSerialize,
)]
#[display("<resolution-tag-callable slot={} class={}>", slot, class)]
struct ResolutionTagCallable {
    slot: usize,
    class: String,
}

starlark::starlark_simple_value!(ResolutionTagCallable);

#[starlark::values::starlark_value(type = "resolution_tag_callable")]
impl<'v> starlark::values::StarlarkValue<'v> for ResolutionTagCallable {
    fn invoke(
        &self,
        _me: starlark::values::Value<'v>,
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<starlark::values::Value<'v>> {
        let collector = eval
            .extra
            .and_then(|e| e.downcast_ref::<ParseCollector>())
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!(
                    "resolution-tag-callable: no ParseCollector on evaluator"
                ))
            })?;
        let named = args
            .names_map()
            .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("tag kwargs: {e}")))?;
        let mut attrs: BTreeMap<String, RepoAttrValue> = BTreeMap::new();
        for (k, v) in named.iter() {
            if let Some(av) = resolution_attr_from_value(*v) {
                attrs.insert(k.as_str().to_string(), av);
            }
        }
        let mut ues = collector.use_extensions.borrow_mut();
        if let Some(rec) = ues.get_mut(self.slot) {
            rec.tags.push(TagInvocationLite {
                class: self.class.clone(),
                attrs,
            });
        }
        Ok(starlark::values::Value::new_none())
    }
}

fn resolution_attr_from_value<'v>(v: starlark::values::Value<'v>) -> Option<RepoAttrValue> {
    if v.is_none() {
        return Some(RepoAttrValue::None);
    }
    if let Some(s) = v.unpack_str() {
        return Some(RepoAttrValue::String(s.to_string()));
    }
    if let Some(b) = v.unpack_bool() {
        return Some(RepoAttrValue::Bool(b));
    }
    if let Some(i) = v.unpack_i32() {
        return Some(RepoAttrValue::Int(i as i64));
    }
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(item.unpack_str()?.to_string());
        }
        return Some(RepoAttrValue::StringList(out));
    }
    if let Some(dict) = starlark::values::dict::DictRef::from_value(v) {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for (k, val) in dict.iter() {
            out.insert(k.unpack_str()?.to_string(), val.unpack_str()?.to_string());
        }
        return Some(RepoAttrValue::StringDict(out));
    }
    None
}

#[derive(Default, starlark::any::ProvidesStaticType)]
struct ParseCollector {
    name: std::cell::RefCell<Option<String>>,
    version: std::cell::RefCell<Option<String>>,
    deps: std::cell::RefCell<Vec<crate::bzlmod_graph::ParsedBazelDep>>,
    use_extensions: std::cell::RefCell<Vec<UseExtensionLite>>,
    registered_toolchains: std::cell::RefCell<Vec<String>>,
    registered_execution_platforms: std::cell::RefCell<Vec<String>>,
}

/// Parsed shape of a transitive MODULE.bazel — what the resolution
/// walk actually needs from it.
#[derive(Debug, Default)]
struct ParsedModule {
    bazel_deps: Vec<BazelDepLine>,
    use_extensions: Vec<UseExtensionLite>,
    registered_toolchains: Vec<String>,
}

#[derive(Debug, Clone)]
struct BazelDepLine {
    name: String,
    version: String,
    apparent: String,
    dev_dependency: bool,
}

/// BCR-resolver entry point. Thin compat wrapper over
/// [`parse_module_bazel_impl`] — the resolver only needs `bazel_deps`
/// / `use_extensions` / `registered_toolchains`. Everything else the
/// underlying parser captures (name/version/registered_execution_platforms)
/// is dropped on the floor here; the vendor walker calls the impl
/// directly to keep those fields.
fn parse_module_directives(src: &str) -> anyhow::Result<ParsedModule> {
    let full = parse_module_bazel_impl(src, false)?;
    Ok(ParsedModule {
        bazel_deps: full
            .bazel_deps
            .into_iter()
            .map(|d| BazelDepLine {
                name: d.name,
                version: d.version,
                apparent: d.apparent,
                dev_dependency: d.dev_dependency,
            })
            .collect(),
        use_extensions: full.use_extensions,
        registered_toolchains: full.registered_toolchains,
    })
}

/// Full lenient `MODULE.bazel` parser. Captures every directive the
/// bzlmod surface declares: `module(...)` identity,
/// `bazel_dep(...)`, `use_extension(...)` (with chained
/// `ext.<class>(...)` tags and `use_repo(...)` names),
/// `register_toolchains(...)`, `register_execution_platforms(...)`.
/// `lenient=true` swallows mid-file eval errors so partial results
/// survive (used by both the BCR resolver and the vendor walker).
pub(crate) fn parse_module_bazel_impl(
    src: &str,
    lenient: bool,
) -> anyhow::Result<crate::bzlmod_graph::ParsedModuleBazel> {
    use starlark::environment::{GlobalsBuilder, LibraryExtension, Module};
    use starlark::eval::Evaluator;
    use starlark::starlark_module;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::Value;
    use starlark::values::none::NoneType;

    type Collector = ParseCollector;

    #[starlark_module]
    fn lenient_module_globals(b: &mut GlobalsBuilder) {
        fn bazel_dep<'v>(
            #[starlark(require = named)] name: &str,
            #[starlark(require = named, default = "")] version: &str,
            #[starlark(require = named)] repo_name: Option<Value<'v>>,
            #[starlark(require = named, default = false)] dev_dependency: bool,
            #[starlark(require = named)] max_compatibility_level: Option<Value<'v>>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<NoneType> {
            let _ = max_compatibility_level;
            // Per bzlmod spec: `dev_dependency = True` only applies when
            // the declaring module is the root. This parser exclusively
            // sees transitive (non-root) modules — every dep reached
            // here is non-root by construction — so dev deps drop out
            // entirely. Skipping them avoids fetching dev-only
            // overridden modules (e.g. `bzlmod_lock_files` declared by
            // `rules_jvm_external@6.6` paired with a
            // `local_path_override`) that BCR doesn't host.
            let collector = eval
                .extra
                .and_then(|e| e.downcast_ref::<Collector>())
                .ok_or_else(|| anyhow::anyhow!("missing Collector"))?;
            let apparent = repo_name
                .and_then(|v| v.unpack_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| name.to_string());
            // Per bzlmod spec, `dev_dependency=True` is honoured at
            // the consumer end (root vs non-root). The parser
            // captures the flag; engine-side wiring decides whether
            // to drop the dep. Recording it (instead of skipping)
            // also keeps `scan_vendor` honest — a vendor user might
            // be the root and care about their own dev_deps.
            collector
                .deps
                .borrow_mut()
                .push(crate::bzlmod_graph::ParsedBazelDep {
                    name: name.to_string(),
                    version: version.to_string(),
                    apparent,
                    dev_dependency,
                });
            Ok(NoneType)
        }
        fn module<'v>(
            #[starlark(require = named, default = "")] name: &str,
            #[starlark(require = named, default = "")] version: &str,
            #[starlark(require = named)] compatibility_level: Option<Value<'v>>,
            #[starlark(require = named, default = "")] repo_name: &str,
            #[starlark(require = named)] bazel_compatibility: Option<Value<'v>>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<NoneType> {
            // `compatibility_level` is an int in Bazel; `bazel_compatibility`
            // is a list of version strings. Loupe doesn't model
            // either yet — accept-and-ignore.
            let _ = (compatibility_level, repo_name, bazel_compatibility);
            if let Some(collector) = eval.extra.and_then(|e| e.downcast_ref::<Collector>()) {
                if !name.is_empty() {
                    *collector.name.borrow_mut() = Some(name.to_string());
                }
                if !version.is_empty() {
                    *collector.version.borrow_mut() = Some(version.to_string());
                }
            }
            Ok(NoneType)
        }
        fn register_toolchains<'v>(
            args: &starlark::eval::Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<NoneType> {
            // Capture each positional string label so engine-side
            // toolchain resolution can pick up transitively-registered
            // toolchains from BCR-fetched modules without needing those
            // modules' archives materialised on disk yet.
            if let Some(collector) = eval.extra.and_then(|e| e.downcast_ref::<Collector>()) {
                let positions: Vec<_> = match args.positions(eval.heap()) {
                    Ok(it) => it.collect(),
                    Err(_) => Vec::new(),
                };
                let mut sink = collector.registered_toolchains.borrow_mut();
                for v in positions {
                    if let Some(s) = v.unpack_str() {
                        sink.push(s.to_string());
                    }
                }
            }
            Ok(NoneType)
        }
        fn register_execution_platforms<'v>(
            args: &starlark::eval::Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<NoneType> {
            if let Some(collector) = eval.extra.and_then(|e| e.downcast_ref::<Collector>()) {
                let positions: Vec<_> = match args.positions(eval.heap()) {
                    Ok(it) => it.collect(),
                    Err(_) => Vec::new(),
                };
                let mut sink = collector.registered_execution_platforms.borrow_mut();
                for v in positions {
                    if let Some(s) = v.unpack_str() {
                        sink.push(s.to_string());
                    }
                }
            }
            Ok(NoneType)
        }
        fn use_extension<'v>(
            #[starlark(require = pos)] bzl_label: &str,
            #[starlark(require = pos)] extension_name: &str,
            #[starlark(require = named, default = false)] dev_dependency: bool,
            #[starlark(require = named, default = false)] isolate: bool,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<Value<'v>> {
            let _ = isolate;
            let collector = eval
                .extra
                .and_then(|e| e.downcast_ref::<Collector>())
                .ok_or_else(|| anyhow::anyhow!("missing Collector"))?;
            let slot = {
                let mut ues = collector.use_extensions.borrow_mut();
                let s = ues.len();
                ues.push(UseExtensionLite {
                    bzl_label: bzl_label.to_string(),
                    name: extension_name.to_string(),
                    tags: Vec::new(),
                    dev_dependency,
                    use_repo_names: Vec::new(),
                });
                s
            };
            Ok(eval.heap().alloc(ResolutionExtProxy { slot }))
        }
        fn use_repo<'v>(
            args: &starlark::eval::Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<NoneType> {
            use starlark::values::ValueLike;
            // Capture every positional repo-name string against the
            // slot pointed at by the first positional (which must be a
            // `ResolutionExtProxy`). Used by engine-side wiring to map
            // `name → <owner><sep><ext_name><sep><name>` for transitive
            // modules so loads through those apparent names resolve.
            let Some(collector) = eval.extra.and_then(|e| e.downcast_ref::<Collector>()) else {
                return Ok(NoneType);
            };
            let positions: Vec<_> = match args.positions(eval.heap()) {
                Ok(it) => it.collect(),
                Err(_) => return Ok(NoneType),
            };
            let mut iter = positions.into_iter();
            let Some(proxy_value) = iter.next() else {
                return Ok(NoneType);
            };
            let Some(proxy) = proxy_value.downcast_ref::<ResolutionExtProxy>() else {
                return Ok(NoneType);
            };
            let slot = proxy.slot;
            let mut names: Vec<String> = Vec::new();
            for v in iter {
                if let Some(s) = v.unpack_str() {
                    names.push(s.to_string());
                }
            }
            // `use_repo(ext, key = "real_name")` style — kwargs alias
            // a repo's user-visible name. Capture the keys.
            if let Ok(named) = args.names_map() {
                for k in named.keys() {
                    names.push(k.as_str().to_string());
                }
            }
            let mut ues = collector.use_extensions.borrow_mut();
            if let Some(rec) = ues.get_mut(slot) {
                rec.use_repo_names.extend(names);
            }
            Ok(NoneType)
        }
        fn use_repo_rule<'v>(
            args: &starlark::eval::Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> anyhow::Result<Value<'v>> {
            let _ = args;
            Ok(eval.heap().alloc(ResolutionExtProxy { slot: usize::MAX }))
        }
        fn include<'v>(args: &starlark::eval::Arguments<'v, '_>) -> anyhow::Result<NoneType> {
            let _ = args;
            Ok(NoneType)
        }
        fn single_version_override<'v>(
            args: &starlark::eval::Arguments<'v, '_>,
        ) -> anyhow::Result<NoneType> {
            let _ = args;
            Ok(NoneType)
        }
        fn multiple_version_override<'v>(
            args: &starlark::eval::Arguments<'v, '_>,
        ) -> anyhow::Result<NoneType> {
            let _ = args;
            Ok(NoneType)
        }
        fn archive_override<'v>(
            args: &starlark::eval::Arguments<'v, '_>,
        ) -> anyhow::Result<NoneType> {
            let _ = args;
            Ok(NoneType)
        }
        fn git_override<'v>(args: &starlark::eval::Arguments<'v, '_>) -> anyhow::Result<NoneType> {
            let _ = args;
            Ok(NoneType)
        }
        fn local_path_override<'v>(
            args: &starlark::eval::Arguments<'v, '_>,
        ) -> anyhow::Result<NoneType> {
            let _ = args;
            Ok(NoneType)
        }
    }

    let ast = AstModule::parse("MODULE.bazel", src.to_string(), &Dialect::Standard)
        .map_err(|e| anyhow::anyhow!("parse: {e}"))?;
    // Bazel's MODULE.bazel surface is real Starlark — real-world
    // modules (e.g. `rules_go@0.37.0`) call `print(...)` at top level.
    // `GlobalsBuilder::standard()` omits it, so extend explicitly.
    let globals = GlobalsBuilder::extended_by(&[LibraryExtension::Print])
        .with(lenient_module_globals)
        .build();
    let collector = Collector::default();
    let eval_outcome = Module::with_temp_heap(|module| -> anyhow::Result<()> {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&collector);
        eval.eval_module(ast, &globals)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("eval: {e}"))
    });
    if !lenient {
        eval_outcome?;
    }
    Ok(crate::bzlmod_graph::ParsedModuleBazel {
        name: collector.name.into_inner(),
        version: collector.version.into_inner(),
        bazel_deps: collector.deps.into_inner(),
        use_extensions: collector.use_extensions.into_inner(),
        registered_toolchains: collector.registered_toolchains.into_inner(),
        registered_execution_platforms: collector.registered_execution_platforms.into_inner(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[test]
    fn parses_bazel_deps_flat() {
        let deps = parse_module_directives(
            r#"module(name = "foo", version = "1.0")
bazel_dep(name = "a", version = "1.0")
bazel_dep(name = "b", version = "2.5")
"#,
        )
        .unwrap();
        assert_eq!(deps.bazel_deps.len(), 2);
        assert_eq!(deps.bazel_deps[0].name, "a");
        assert_eq!(deps.bazel_deps[1].version, "2.5");
    }

    #[test]
    fn parser_captures_dev_dependency_flag() {
        // Regression: `rules_jvm_external@6.6`'s MODULE.bazel pairs a
        // `bazel_dep(name = "bzlmod_lock_files", version = "0.0.0",
        // dev_dependency = True)` with a `local_path_override`. BCR
        // doesn't host `bzlmod_lock_files`; Bazel never tries because
        // `dev_dependency = True` is ignored on non-root modules.
        // The parser captures the flag so the resolver can filter
        // it out at queue-time; non-root filtering itself lives in
        // `resolve_full`.
        let parsed = parse_module_directives(
            r#"module(name = "rules_jvm_external", version = "6.6")
bazel_dep(name = "regular_dep", version = "1.0")
bazel_dep(name = "bzlmod_lock_files", version = "0.0.0", dev_dependency = True)
"#,
        )
        .unwrap();
        let flagged: Vec<(&str, bool)> = parsed
            .bazel_deps
            .iter()
            .map(|d| (d.name.as_str(), d.dev_dependency))
            .collect();
        assert_eq!(
            flagged,
            vec![("regular_dep", false), ("bzlmod_lock_files", true)]
        );
    }

    #[test]
    fn lenient_parser_accepts_top_level_print() {
        // Regression: `rules_go@0.37.0`'s MODULE.bazel calls
        // `print("WARNING: ...")` at top level. `GlobalsBuilder::standard()`
        // doesn't include `print`; without the `Print` extension the
        // whole module fails to parse and `bazel_dep` records are
        // dropped along with it.
        let parsed = parse_module_directives(
            r#"module(name = "x")
print("hello from MODULE.bazel")
bazel_dep(name = "a", version = "1.0")
"#,
        )
        .unwrap();
        assert_eq!(parsed.bazel_deps.len(), 1);
        assert_eq!(parsed.bazel_deps[0].name, "a");
    }

    #[test]
    fn use_extension_accepts_dev_dependency_and_isolate() {
        // Regression: BCR-served third-party MODULE.bazel files
        // routinely call `use_extension(..., dev_dependency = True)`
        // (e.g. aspect_bazel_lib). The lenient parser must accept the
        // documented bzlmod kwargs as-is — a leading underscore on the
        // Rust param name would silently rename the Starlark binding.
        let parsed = parse_module_directives(
            r#"module(name = "x")
ext = use_extension("@foo//:e.bzl", "ext", dev_dependency = True, isolate = True)
"#,
        )
        .unwrap();
        assert_eq!(parsed.use_extensions.len(), 1);
        assert_eq!(parsed.use_extensions[0].name, "ext");
    }

    #[test]
    fn parses_module_with_extensions() {
        let parsed = parse_module_directives(
            r#"module(name = "x")
bazel_dep(name = "rules_oci", version = "1.7.0")
ext = use_extension("@rules_oci//oci:extensions.bzl", "oci")
ext.pull(name = "alpine", digest = "sha256:abc")
use_repo(ext, "alpine")
register_toolchains("@rules_oci//oci:tc")
"#,
        )
        .unwrap();
        assert_eq!(parsed.bazel_deps.len(), 1);
        assert_eq!(parsed.bazel_deps[0].name, "rules_oci");
        // Extension capture: the `ext.pull(...)` invocation is
        // recorded against the use_extension record.
        assert_eq!(parsed.use_extensions.len(), 1);
        let ext = &parsed.use_extensions[0];
        assert_eq!(ext.bzl_label, "@rules_oci//oci:extensions.bzl");
        assert_eq!(ext.name, "oci");
        assert_eq!(ext.tags.len(), 1);
        assert_eq!(ext.tags[0].class, "pull");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolves_flat_graph_from_registry() {
        let server = MockServer::start().await;
        for (name, version) in [("a", "1.0"), ("b", "2.0")] {
            Mock::given(method("GET"))
                .and(path(
                    format!("/modules/{name}/{version}/MODULE.bazel").as_str(),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(
                    format!("module(name=\"{name}\", version=\"{version}\")").into_bytes(),
                ))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(
                    format!("/modules/{name}/{version}/source.json").as_str(),
                ))
                .respond_with(
                    ResponseTemplate::new(200).set_body_bytes(
                        serde_json::json!({"url": format!("https://x/{name}-{version}.tar.gz")})
                            .to_string()
                            .into_bytes(),
                    ),
                )
                .mount(&server)
                .await;
        }

        let registry = Registry::custom(server.uri());
        let cache = tempfile::tempdir().unwrap();
        let resolved = resolve(
            &reqwest::Client::new(),
            cache.path(),
            &registry,
            vec![
                BazelDepRequest {
                    name: "a".to_string(),
                    version: "1.0".to_string(),
                },
                BazelDepRequest {
                    name: "b".to_string(),
                    version: "2.0".to_string(),
                },
            ],
            '+',
        )
        .await
        .unwrap();

        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains_key("a+"));
        assert!(resolved.contains_key("b+"));
        let a = &resolved["a+"];
        match &a.spec.kind {
            RepoSpecKind::Http { urls, .. } => assert_eq!(urls[0], "https://x/a-1.0.tar.gz"),
            _ => panic!("expected Http"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolves_transitive_with_mvs_max() {
        let server = MockServer::start().await;
        // Root requests a@1.0 and b@1.0.
        // a@1.0 depends on c@1.0.
        // b@1.0 depends on c@1.5. → MVS picks c@1.5.
        let module_files = [
            (
                "a",
                "1.0",
                r#"module(name="a", version="1.0")
bazel_dep(name="c", version="1.0")
"#,
            ),
            (
                "b",
                "1.0",
                r#"module(name="b", version="1.0")
bazel_dep(name="c", version="1.5")
"#,
            ),
            ("c", "1.0", r#"module(name="c", version="1.0")"#),
            ("c", "1.5", r#"module(name="c", version="1.5")"#),
        ];
        for (name, version, body) in &module_files {
            Mock::given(method("GET"))
                .and(path(
                    format!("/modules/{name}/{version}/MODULE.bazel").as_str(),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.as_bytes().to_vec()))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(
                    format!("/modules/{name}/{version}/source.json").as_str(),
                ))
                .respond_with(
                    ResponseTemplate::new(200).set_body_bytes(
                        serde_json::json!({"url": format!("https://x/{name}-{version}.tar.gz")})
                            .to_string()
                            .into_bytes(),
                    ),
                )
                .mount(&server)
                .await;
        }

        let registry = Registry::custom(server.uri());
        let cache = tempfile::tempdir().unwrap();
        let resolved = resolve(
            &reqwest::Client::new(),
            cache.path(),
            &registry,
            vec![
                BazelDepRequest {
                    name: "a".to_string(),
                    version: "1.0".to_string(),
                },
                BazelDepRequest {
                    name: "b".to_string(),
                    version: "1.0".to_string(),
                },
            ],
            '+',
        )
        .await
        .unwrap();

        assert_eq!(resolved["c+"].version, "1.5", "MVS should pick max version");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn well_known_modules_skipped() {
        let server = MockServer::start().await;
        let resolved = resolve(
            &reqwest::Client::new(),
            tempfile::tempdir().unwrap().path(),
            &Registry::custom(server.uri()),
            vec![BazelDepRequest {
                name: "bazel_tools".to_string(),
                version: "1.0".to_string(),
            }],
            '+',
        )
        .await
        .unwrap();
        assert!(resolved.is_empty(), "well-known modules should be skipped");
    }
}
