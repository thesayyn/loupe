//! `MODULE.bazel` parsing and the workspace-wide repo mapping.
//!
//! Each `bazel_dep(name=X, version=Y)` call is recorded and contributes
//! one entry to the workspace's apparent → canonical repo mapping.
//! Loupe doesn't fetch — DESIGN.md mandates "all deps assumed vendored
//! on disk." The engine's external-repo root supplies the on-disk path
//! for each canonical repo (typically derived from `bazel info
//! output_base` or a `bazel mod vendor` output directory).
//!
//! ## Canonical naming
//!
//! Bazel 7's bzlmod uses the convention `<apparent>+` for canonical
//! repo names when a single version of a module is in the graph
//! (`rules_oci+`, `aspect_bazel_lib+`, …). Older bzlmod versions used
//! `<apparent>~<version>` and Bazel 6's WORKSPACE used the apparent
//! name directly. v1 mirrors the Bazel 7 `+` convention; if a future
//! caller needs a different scheme we add an override hook then.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use starlark::any::ProvidesStaticType;
use starlark::environment::{GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::none::NoneType;
use starlark::values::{Value, ValueLike};

use loupe_labels::{CanonicalRepo, Label, PackageId, PackagePath, RepoMapping};

/// Proxy returned by `use_extension(...)`. Carries a slot index into
/// the parsing-time `ModuleBuilder.use_extensions` list — every
/// `proxy.tag_class(...)` invocation that follows is recorded against
/// that slot. `use_repo(proxy, "foo")` also reads this slot to
/// canonicalise `foo`.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    allocative::Allocative,
    starlark::any::ProvidesStaticType,
    derive_more::Display,
    starlark::values::NoSerialize,
)]
#[display("<bzlmod-extension-proxy slot={}>", slot)]
pub struct BzlmodExtensionProxy {
    slot: usize,
}

starlark::starlark_simple_value!(BzlmodExtensionProxy);

#[starlark::values::starlark_value(type = "bzlmod_extension_proxy")]
impl<'v> starlark::values::StarlarkValue<'v> for BzlmodExtensionProxy {
    fn get_attr(
        &self,
        attribute: &str,
        heap: starlark::values::Heap<'v>,
    ) -> Option<starlark::values::Value<'v>> {
        Some(heap.alloc(BzlmodTagCallable {
            slot: self.slot,
            class: attribute.to_string(),
        }))
    }

    fn dir_attr(&self) -> Vec<String> {
        Vec::new()
    }

    fn at(
        &self,
        _index: starlark::values::Value<'v>,
        heap: starlark::values::Heap<'v>,
    ) -> starlark::Result<starlark::values::Value<'v>> {
        Ok(heap.alloc(BzlmodExtensionProxy { slot: self.slot }))
    }

    fn invoke(
        &self,
        _me: starlark::values::Value<'v>,
        _args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<starlark::values::Value<'v>> {
        // `proxy(...)` — uncommon. v1 ignores; the named-class form
        // (`proxy.tag(...)`) is what real MODULE.bazel files use.
        Ok(eval.heap().alloc(BzlmodExtensionProxy { slot: self.slot }))
    }

    fn equals(&self, other: starlark::values::Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<BzlmodExtensionProxy>()
            .map(|p| p.slot == self.slot)
            .unwrap_or(false))
    }
}

/// `proxy.<class>` bound callable — records a [`TagInvocation`] into
/// the parsing-time `ModuleBuilder.use_extensions[slot].tags` when
/// invoked.
#[derive(
    Debug,
    Clone,
    allocative::Allocative,
    starlark::any::ProvidesStaticType,
    derive_more::Display,
    starlark::values::NoSerialize,
)]
#[display("<bzlmod-tag-callable slot={} class={}>", slot, class)]
pub struct BzlmodTagCallable {
    slot: usize,
    class: String,
}

starlark::starlark_simple_value!(BzlmodTagCallable);

#[starlark::values::starlark_value(type = "bzlmod_tag_callable")]
impl<'v> starlark::values::StarlarkValue<'v> for BzlmodTagCallable {
    fn invoke(
        &self,
        _me: starlark::values::Value<'v>,
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<starlark::values::Value<'v>> {
        let builder = builder_from_eval(eval).map_err(starlark::Error::new_other)?;
        let named = args
            .names_map()
            .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("tag kwargs: {e}")))?;
        let mut attrs: BTreeMap<String, TagAttrLiteral> = BTreeMap::new();
        for (k, v) in named.iter() {
            if let Some(lit) = tag_attr_literal_from_value(*v) {
                attrs.insert(k.as_str().to_string(), lit);
            }
            // Unsupported attr types (Label values, depsets, …) are
            // silently dropped — the extension impl would fail later
            // if it tried to read them.
        }
        let mut ues = builder.use_extensions.borrow_mut();
        if let Some(rec) = ues.get_mut(self.slot) {
            rec.tags.push(TagInvocation {
                class: self.class.clone(),
                attrs,
            });
        }
        Ok(starlark::values::Value::new_none())
    }
}

fn tag_attr_literal_from_value<'v>(v: starlark::values::Value<'v>) -> Option<TagAttrLiteral> {
    if let Some(s) = v.unpack_str() {
        return Some(TagAttrLiteral::String(s.to_string()));
    }
    if let Some(b) = v.unpack_bool() {
        return Some(TagAttrLiteral::Bool(b));
    }
    if let Some(i) = v.unpack_i32() {
        return Some(TagAttrLiteral::Int(i as i64));
    }
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(item.unpack_str()?.to_string());
        }
        return Some(TagAttrLiteral::StringList(out));
    }
    if let Some(dict) = starlark::values::dict::DictRef::from_value(v) {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for (k, val) in dict.iter() {
            out.insert(k.unpack_str()?.to_string(), val.unpack_str()?.to_string());
        }
        return Some(TagAttrLiteral::StringDict(out));
    }
    None
}

/// Bzlmod tag-attribute literal. Narrower than full Bazel attr types —
/// extensions only need JSON-friendly scalars and the two collection
/// shapes. Labels in tags get coerced to strings (rules read them
/// through `ctx.attr` anyway).
#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub enum TagAttrLiteral {
    String(String),
    Int(i64),
    Bool(bool),
    StringList(Vec<String>),
    StringDict(BTreeMap<String, String>),
}

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct TagInvocation {
    pub class: String,
    pub attrs: BTreeMap<String, TagAttrLiteral>,
}

/// One `ext = use_extension(bzl_label, "name", ...)` declaration in
/// MODULE.bazel, plus the chained `ext.<class>(...)` tag invocations.
/// `bzl_label` is the raw string from the call — not resolved yet.
#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct UseExtensionRecord {
    pub bzl_label: String,
    pub name: String,
    pub dev_dependency: bool,
    pub tags: Vec<TagInvocation>,
    /// Apparent names brought into this module's scope via
    /// `use_repo(ext, ...)`. Each name maps to the ext-generated
    /// canonical at parse time (`<owner_canonical><sep><ext>+<name>`).
    pub use_repo_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleDecl {
    pub name: String,
    pub version: String,
    pub canonical: CanonicalRepo,
}

/// One `bazel_dep(...)` declaration from `MODULE.bazel`.
#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleDep {
    /// `name=` arg — what BUILD/`.bzl` files reference as `@<name>`.
    pub apparent: String,
    /// `version=` arg. Loupe doesn't enforce version selection at v1
    /// (deps are assumed vendored), but the field rides along so a
    /// future MVS pass can land without re-parsing.
    pub version: String,
    /// Canonical repo name. Matches Bazel 7's `<apparent>+` convention
    /// so on-disk paths line up with `<output_base>/external/<canonical>`
    /// (or a vendored equivalent).
    pub canonical: CanonicalRepo,
}

/// Compute the canonical repo name Bazel 7 bzlmod assigns to a
/// `bazel_dep` call. Public so callers wiring an external-repo root
/// can predict the directory name without re-parsing `MODULE.bazel`.
pub fn canonical_for_apparent(apparent: &str) -> CanonicalRepo {
    CanonicalRepo::new(format!("{apparent}+"))
}

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleGraph {
    pub main: ModuleDecl,
    /// Every `bazel_dep` declaration in declaration order.
    pub deps: Vec<ModuleDep>,
    /// apparent name → canonical repo. Contains the main module and
    /// every `bazel_dep`. `owner` is pre-stamped to the main module's
    /// canonical so `Label("@//pkg:x")` self-refs resolve when the
    /// load originates from the main repo.
    pub repo_mapping: RepoMapping,
    /// Per-repo apparent → canonical mappings sourced from
    /// `MODULE.bazel.lock`. Each external repo carries its own view of
    /// transitive deps; resolving a load from inside repo X consults
    /// `per_repo_mapping[X]` first, then falls back to the main
    /// `repo_mapping`. Each entry's `owner` is the key. See
    /// [`crate::lockfile`].
    pub per_repo_mapping: BTreeMap<CanonicalRepo, RepoMapping>,
    /// Toolchains registered across the resolved module graph, in
    /// declaration order: main module's `register_toolchains(...)`
    /// calls first, then each `bazel_dep`'s in graph order. Labels
    /// are pre-canonicalised against the originating module's
    /// per-repo mapping (from `MODULE.bazel.lock`) so the resolver
    /// can match by `Label` equality without re-walking the mapping.
    /// Transitive parsing requires `EngineBuilder::external_repo_root`
    /// to be set; without it only the main module's calls are
    /// captured. v1 doesn't expand `:all` patterns or honour
    /// `dev_dependency` filtering.
    pub registered_toolchains: Vec<Label>,
    /// Execution platforms registered across the module graph. Same
    /// shape as `registered_toolchains`. v1 captures only — platform
    /// resolution itself isn't run.
    pub registered_execution_platforms: Vec<Label>,
    /// `use_extension(...)` declarations from the main MODULE.bazel
    /// in declaration order, with each chained `proxy.tag(...)` and
    /// `use_repo(proxy, ...)` recorded against the matching slot.
    /// `EngineBuilder::build()` walks this list, runs each
    /// extension's `impl(module_ctx)`, and merges the captured
    /// `http_archive(...)`-style calls into the bzlmod repo-spec map.
    pub use_extensions: Vec<UseExtensionRecord>,
    /// Bzlmod canonical separator detected from the lockfile (byte
    /// form for Allocative compat). `+` for Bazel 7+ (default when no
    /// lockfile is present), `~` for older `lockFileVersion: 11`
    /// workspaces. Used everywhere canonicals like
    /// `<owner><sep><ext><sep><repo>` get synthesized.
    canonical_separator_byte: u8,
}

impl ModuleGraph {
    /// The bzlmod canonical separator as a `char` (`+` or `~`).
    pub fn canonical_separator(&self) -> char {
        self.canonical_separator_byte as char
    }
}

impl ModuleGraph {
    pub fn resolve_apparent(&self, apparent: &str) -> Option<&CanonicalRepo> {
        self.repo_mapping.lookup(apparent)
    }

    /// Effective `apparent → canonical` mapping for `pkg`'s repo.
    /// Starts from the main module's mapping, then layers
    /// `per_repo_mapping[pkg.repo]` on top — so the rule's defining
    /// repo's view (lockfile-recorded + bazel_dep-derived) wins for
    /// names main never declared. The returned mapping's `owner` is
    /// set to `pkg.repo` so `Label("@//pkg:x")` self-refs resolve
    /// from the perspective of the load site. Use everywhere a label
    /// string is canonicalised in a non-main context: `toolchains=[...]`
    /// declarations on a rule loaded from an external `.bzl`,
    /// select() branch keys inside external BUILDs, etc.
    pub fn effective_repo_mapping(&self, pkg: &PackageId) -> RepoMapping {
        let mut out = self.repo_mapping.clone();
        if let Some(per) = self.per_repo_mapping.get(&pkg.repo) {
            for (k, v) in &per.entries {
                out.entries.insert(k.clone(), v.clone());
            }
        }
        out.owner = Some(pkg.repo.clone());
        out
    }
}

#[derive(Debug, Default, ProvidesStaticType)]
struct ModuleBuilder {
    main: RefCell<Option<ModuleDecl>>,
    deps: RefCell<Vec<ModuleDep>>,
    registered_toolchains: RefCell<Vec<String>>,
    registered_execution_platforms: RefCell<Vec<String>>,
    /// Use-extension records keyed by slot index (the order the
    /// `use_extension(...)` calls appear in MODULE.bazel). Each
    /// record's `tags` and `use_repo_names` are appended as the
    /// MODULE.bazel evaluator chains through `.tag(...)` / `use_repo`.
    use_extensions: RefCell<Vec<UseExtensionRecord>>,
    /// Apparent names brought into the consuming module's view via
    /// `use_repo(<ext_proxy>, "name1", ...)`. Kept as a flat list
    /// for the legacy lockfile-driven synthesis path; the structured
    /// records in `use_extensions` are what the new ext-eval path
    /// consults.
    used_repo_names: RefCell<Vec<String>>,
}

fn builder_from_eval<'a, 'v>(eval: &'a Evaluator<'v, '_, '_>) -> anyhow::Result<&'a ModuleBuilder> {
    eval.extra
        .ok_or_else(|| anyhow::anyhow!("internal: no ModuleBuilder on evaluator"))?
        .downcast_ref::<ModuleBuilder>()
        .ok_or_else(|| anyhow::anyhow!("internal: extra is not a ModuleBuilder"))
}

fn unpack_label_args<'v>(
    args: &starlark::eval::Arguments<'v, '_>,
    eval: &Evaluator<'v, '_, '_>,
    fn_name: &str,
) -> anyhow::Result<Vec<String>> {
    let positions = args
        .positions(eval.heap())
        .map_err(|e| anyhow::anyhow!("{fn_name}: {e}"))?;
    let mut out = Vec::new();
    for v in positions {
        let s = v.unpack_str().ok_or_else(|| {
            anyhow::anyhow!(
                "{fn_name}: positional arg must be a string label, got {}",
                v.get_type()
            )
        })?;
        out.push(s.to_string());
    }
    Ok(out)
}

#[starlark_module]
fn module_bazel_globals(b: &mut GlobalsBuilder) {
    fn module<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = "")] version: &str,
        #[starlark(require = named)] compatibility_level: Option<Value<'v>>,
        // Same `None`-from-Starlark tolerance as `bazel_dep`.
        #[starlark(require = named)] repo_name: Option<Value<'v>>,
        #[starlark(require = named)] bazel_compatibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        // `compatibility_level` is an int in Bazel; `bazel_compatibility`
        // is a list of version-spec strings. Both are accepted but not
        // used in v1 — the analysis engine doesn't enforce module-graph
        // compatibility.
        let _ = (compatibility_level, repo_name, bazel_compatibility);
        let builder = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("internal: no ModuleBuilder on evaluator"))?
            .downcast_ref::<ModuleBuilder>()
            .ok_or_else(|| anyhow::anyhow!("internal: extra is not a ModuleBuilder"))?;
        let mut slot = builder.main.borrow_mut();
        if slot.is_some() {
            return Err(anyhow::anyhow!("MODULE.bazel called module(...) twice"));
        }
        *slot = Some(ModuleDecl {
            name: name.to_string(),
            version: version.to_string(),
            canonical: CanonicalRepo::MAIN,
        });
        Ok(NoneType)
    }

    /// `register_toolchains(*labels)` — Bazel's bzlmod toolchain
    /// registration. v1 captures the labels in declaration order; the
    /// toolchain resolver walks this list at `ctx.toolchains[T]`
    /// lookup time. STUB: doesn't expand `:all`-style patterns; each
    /// label is treated as exact.
    fn register_toolchains<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let labels = unpack_label_args(args, eval, "register_toolchains")?;
        let builder = builder_from_eval(eval)?;
        builder.registered_toolchains.borrow_mut().extend(labels);
        Ok(NoneType)
    }

    /// `register_execution_platforms(*labels)`. v1 captures the
    /// labels; platform resolution itself isn't run.
    fn register_execution_platforms<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let labels = unpack_label_args(args, eval, "register_execution_platforms")?;
        let builder = builder_from_eval(eval)?;
        builder
            .registered_execution_platforms
            .borrow_mut()
            .extend(labels);
        Ok(NoneType)
    }

    /// `use_extension(bzl_label, extension_name, dev_dependency=False, isolate=False)`.
    /// Records the call into `ModuleBuilder.use_extensions` and
    /// returns a slot-indexed proxy so subsequent `proxy.tag(...)`
    /// and `use_repo(proxy, ...)` calls find their way back to the
    /// same record. The actual extension `impl(module_ctx)` evaluation
    /// happens later, at `EngineBuilder::build()` time, using these
    /// captured records.
    fn use_extension<'v>(
        #[starlark(require = pos)] bzl_label: &str,
        #[starlark(require = pos)] extension_name: &str,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        #[starlark(require = named, default = false)] isolate: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = isolate;
        let builder = builder_from_eval(eval)?;
        let slot = {
            let mut ues = builder.use_extensions.borrow_mut();
            let slot = ues.len();
            ues.push(UseExtensionRecord {
                bzl_label: bzl_label.to_string(),
                name: extension_name.to_string(),
                dev_dependency,
                tags: Vec::new(),
                use_repo_names: Vec::new(),
            });
            slot
        };
        Ok(eval.heap().alloc(BzlmodExtensionProxy { slot }))
    }

    /// `use_repo(extension_proxy, *names, **renamed)`. Records the
    /// names into the proxy's slot so the engine can map
    /// `apparent → canonical` for ext-generated repos.
    fn use_repo<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let builder = builder_from_eval(eval)?;
        let positionals: Vec<Value<'v>> = args
            .positions(eval.heap())
            .map_err(|e| anyhow::anyhow!("use_repo positionals: {e}"))?
            .collect();
        let proxy_slot = positionals
            .first()
            .and_then(|v| v.downcast_ref::<BzlmodExtensionProxy>())
            .map(|p| p.slot);
        let mut names: Vec<String> = Vec::new();
        for v in positionals.iter().skip(1) {
            if let Some(s) = v.unpack_str() {
                names.push(s.to_string());
            }
        }
        if let Ok(named) = args.names_map() {
            for k in named.keys() {
                names.push(k.as_str().to_string());
            }
        }
        builder.used_repo_names.borrow_mut().extend(names.clone());
        if let Some(slot) = proxy_slot {
            let mut ues = builder.use_extensions.borrow_mut();
            if let Some(rec) = ues.get_mut(slot) {
                rec.use_repo_names.extend(names);
            }
        }
        Ok(NoneType)
    }

    /// `use_repo_rule(bzl_label, name)` returns a callable repo rule.
    /// v1 doesn't materialise via this path — repo_rules called
    /// directly from MODULE.bazel are out of scope (they're a
    /// pre-bzlmod legacy form). Returns a slot-less proxy so the
    /// call chain doesn't crash; any invocation is a no-op.
    fn use_repo_rule<'v>(
        #[starlark(require = pos)] bzl_label: &str,
        #[starlark(require = pos)] name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (bzl_label, name);
        Ok(eval.heap().alloc(BzlmodExtensionProxy { slot: usize::MAX }))
    }

    /// `include(label)`. STUB — v1 doesn't dereference includes.
    fn include<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `multiple_version_override(...)`. STUB.
    fn multiple_version_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `override_repo(...)`. STUB.
    fn override_repo<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `inject_repo(...)`. STUB.
    fn inject_repo<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `single_version_override(...)`. STUB — v1 doesn't run MVS.
    fn single_version_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `archive_override(...)`. STUB.
    fn archive_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `local_path_override(module_name, path)`. STUB.
    fn local_path_override<'v>(
        #[starlark(require = named)] module_name: Option<&str>,
        #[starlark(require = named)] path: Option<&str>,
    ) -> anyhow::Result<NoneType> {
        let _ = (module_name, path);
        Ok(NoneType)
    }

    /// `git_override(...)`. STUB.
    fn git_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `bazel_dep(name, version=…, repo_name=…, dev_dependency=…)`.
    /// Recorded into the module graph; `repo_name` (the apparent-name
    /// override) lets `MODULE.bazel` re-bind a dep under a different
    /// short name. v1 honors `repo_name` if set, falls back to `name`.
    fn bazel_dep<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = "")] version: &str,
        // `repo_name` is documented as `str`, but real MODULE.bazel
        // files pass `None` explicitly (e.g. rules_js: `bazel_dep(...,
        // repo_name = None)`). Accept any value and unpack to `&str`
        // or fall back to empty when None / missing.
        #[starlark(require = named)] repo_name: Option<Value<'v>>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = dev_dependency; // accepted; v1 doesn't distinguish
        let builder = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("internal: no ModuleBuilder on evaluator"))?
            .downcast_ref::<ModuleBuilder>()
            .ok_or_else(|| anyhow::anyhow!("internal: extra is not a ModuleBuilder"))?;
        let repo_name_str = repo_name
            .and_then(|v| v.unpack_str().map(str::to_string))
            .unwrap_or_default();
        let apparent = if repo_name_str.is_empty() {
            name
        } else {
            repo_name_str.as_str()
        };
        builder.deps.borrow_mut().push(ModuleDep {
            apparent: apparent.to_string(),
            version: version.to_string(),
            canonical: canonical_for_apparent(name),
        });
        Ok(NoneType)
    }
}

/// Result of evaluating one MODULE.bazel file. Doesn't canonicalise —
/// the caller decides which package + repo mapping to resolve against
/// (main vs transitive).
struct EvalResult {
    main: Option<ModuleDecl>,
    deps: Vec<ModuleDep>,
    raw_toolchains: Vec<String>,
    raw_execution_platforms: Vec<String>,
    used_repo_names: Vec<String>,
    use_extensions: Vec<UseExtensionRecord>,
}

fn run_module_eval(src: &str, display_name: &str, lenient: bool) -> Result<EvalResult> {
    let ast = AstModule::parse(display_name, src.to_string(), &Dialect::Standard)
        .map_err(|e| anyhow::anyhow!("parsing {display_name}: {e}"))?;
    // Bazel's MODULE.bazel surface is real Starlark — real-world
    // modules (e.g. `rules_go@0.37.0`) call `print(...)` at top level.
    // `GlobalsBuilder::standard()` omits it, so extend explicitly.
    let globals = GlobalsBuilder::extended_by(&[LibraryExtension::Print])
        .with(module_bazel_globals)
        .build();
    let builder = ModuleBuilder::default();
    // `lenient` mode: eval errors mid-file (e.g. an unmodelled
    // extension tag-class method, an unknown override) shouldn't
    // discard results from statements that already ran. The
    // builder's RefCells captured everything up to the error.
    // Used for transitive `external/<canonical>/MODULE.bazel` walks
    // where we just want to harvest `register_toolchains` calls and
    // would otherwise lose them to the first unmodelled call.
    // Strict mode (the main MODULE.bazel) propagates errors so
    // misuse like `register_toolchains(123)` still surfaces.
    let eval_outcome = Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&builder);
        eval.eval_module(ast, &globals)
            .map(|_| ())
            .map_err(|e| anyhow!("evaluating {display_name}: {e}"))
    });
    if !lenient {
        eval_outcome?;
    }
    Ok(EvalResult {
        main: builder.main.into_inner(),
        deps: builder.deps.into_inner(),
        raw_toolchains: builder.registered_toolchains.into_inner(),
        raw_execution_platforms: builder.registered_execution_platforms.into_inner(),
        used_repo_names: builder.used_repo_names.into_inner(),
        use_extensions: builder.use_extensions.into_inner(),
    })
}

/// Stamp the lockfile's canonical separator onto a `bazel_dep`-derived
/// canonical name. Bazel's `bazel_dep` impl always emits `<name>+`;
/// older lockfiles (`lockFileVersion: 11`) actually use `~`, and a
/// small set of "well-known" modules (`bazel_tools`, `platforms`,
/// `local_config_platform`) keep their bare name as canonical
/// regardless of the lockfile separator — Bazel hardcodes these.
fn canonicalise_with_sep(raw: &str, sep: char) -> CanonicalRepo {
    let trimmed = raw.trim_end_matches(['+', '~']);
    if matches!(
        trimmed,
        "bazel_tools" | "platforms" | "local_config_platform"
    ) {
        CanonicalRepo::new(trimmed)
    } else {
        CanonicalRepo::new(format!("{trimmed}{sep}"))
    }
}

/// Resolve a `use_extension(...)` `bzl_label` to the canonical name of
/// the module that defines the extension. Bazel's ext-generated repos
/// have canonical `<defining_module_canonical><sep><ext><sep><repo>`,
/// where the defining module is the .bzl's owning repo — NOT the
/// using module. For `@aspect_tools_telemetry//:extension.bzl` used
/// from MAIN, the defining module is `aspect_tools_telemetry+`, so
/// generated repos are `aspect_tools_telemetry++telemetry+...`, not
/// `+telemetry+...`. Matches the spec-registration formula in
/// `Engine::build_module_extension_specs`.
fn resolve_ext_owner_canonical(
    bzl_label: &str,
    using_module_canonical: &str,
    repo_mapping: &RepoMapping,
    sep: char,
) -> String {
    if let Some(rest) = bzl_label.strip_prefix("@@") {
        if let Some(slash) = rest.find("//") {
            return rest[..slash].to_string();
        }
        return String::new();
    }
    if let Some(rest) = bzl_label.strip_prefix('@') {
        if let Some(slash) = rest.find("//") {
            let apparent = &rest[..slash];
            if apparent.is_empty() {
                return using_module_canonical.to_string();
            }
            if let Some(c) = repo_mapping.lookup(apparent) {
                return c.as_str().to_string();
            }
            return match apparent {
                "bazel_tools" | "platforms" | "local_config_platform" => apparent.to_string(),
                other => format!("{other}{sep}"),
            };
        }
        return String::new();
    }
    if bzl_label.starts_with("//") {
        return using_module_canonical.to_string();
    }
    String::new()
}

/// Resolve each raw label string into a canonical `Label`. Strings
/// that fail to parse are dropped; v1 doesn't expand `:all` patterns.
fn canonicalise_labels(
    raw: &[String],
    current_package: &PackageId,
    repo_mapping: &RepoMapping,
) -> Vec<Label> {
    raw.iter()
        .filter_map(|s| Label::resolve(s, current_package, repo_mapping).ok())
        .collect()
}

/// Build a [`ModuleGraph`] from a resolved [`BzlmodGraph`] —
/// source-agnostic in-memory synthesis. Replaces the on-disk walker
/// that used to live inside [`parse_with_lockfile_override`]: every
/// per-repo mapping, register_toolchains canonicalisation, and
/// extension record is derived from the `BzlmodGraph` snapshot plus
/// the lockfile's recorded per-repo mapping entries.
///
/// `lockfile_path`: optional override; when `None`, the function
/// reads `<workspace>/MODULE.bazel.lock` if it exists.
pub fn synthesise_module_graph(
    workspace_root: &Path,
    bzlmod_graph: &loupe_bzlmod::bzlmod_graph::BzlmodGraph,
    lockfile_path: Option<std::path::PathBuf>,
) -> Result<ModuleGraph> {
    let lockfile =
        match lockfile_path {
            Some(p) => crate::lockfile::load_from(p)
                .with_context(|| "loading overridden MODULE.bazel.lock")?,
            None => crate::lockfile::load(workspace_root)
                .with_context(|| "loading MODULE.bazel.lock")?,
        };
    let sep = if bzlmod_graph.sep != '\0' {
        bzlmod_graph.sep
    } else {
        lockfile.canonical_separator()
    };

    // Main module decl.
    let main_record = &bzlmod_graph.main;
    let main = ModuleDecl {
        name: main_record.name.clone(),
        version: main_record.version.clone(),
        canonical: CanonicalRepo::MAIN,
    };

    // Main module's bazel_deps (with proper canonical-sep).
    let deps: Vec<ModuleDep> = main_record
        .bazel_deps
        .iter()
        .map(|d| ModuleDep {
            apparent: d.apparent.clone(),
            version: d.version.clone(),
            canonical: CanonicalRepo::new(d.canonical.clone()),
        })
        .collect();

    // Main repo_mapping: apparent → canonical from main's bazel_deps.
    let mut repo_mapping = RepoMapping::owned_by(main.canonical.clone());
    repo_mapping
        .entries
        .insert(main_record.name.clone(), main.canonical.clone());
    for dep in &deps {
        repo_mapping
            .entries
            .insert(dep.apparent.clone(), dep.canonical.clone());
    }

    let main_pkg = PackageId::new(main.canonical.clone(), PackagePath::root());

    // Per-repo mapping baseline: lockfile-recorded entries (Bazel
    // 7+'s rich `recordedRepoMappingEntries`). Authoritative; later
    // synthesis only fills in entries the lockfile didn't record.
    let mut per_repo_mapping: BTreeMap<CanonicalRepo, RepoMapping> = lockfile.per_repo_mapping;

    // Synthesise each transitive module's apparent→canonical view
    // from its parsed `bazel_dep` list. Lockfile entries win on
    // conflict (they carry `repo_name=` overrides correctly); this
    // fills in everything else.
    for module in &bzlmod_graph.transitive {
        let canon = CanonicalRepo::new(module.canonical.clone());
        let entry = per_repo_mapping
            .entry(canon.clone())
            .or_insert_with(|| RepoMapping::owned_by(canon.clone()));
        // Self-reference: the module's own apparent name points at
        // its own canonical (so `@<own_name>//...` loads inside the
        // module resolve correctly).
        entry
            .entries
            .entry(module.name.clone())
            .or_insert_with(|| CanonicalRepo::new(module.canonical.clone()));
        for d in &module.bazel_deps {
            entry
                .entries
                .entry(d.apparent.clone())
                .or_insert_with(|| CanonicalRepo::new(d.canonical.clone()));
        }
        // `use_repo(ext, "foo")` brings `foo →
        // <defining_module_canonical><sep><ext_name><sep><foo>` into the
        // module's apparent-name view. The OWNER for canonical naming
        // is the module that DEFINES the extension (its .bzl's repo),
        // not the using module — see `resolve_ext_owner_canonical`.
        for ue in &module.use_extensions {
            if ue.dev_dependency && !module.is_root {
                continue;
            }
            let owner = resolve_ext_owner_canonical(&ue.bzl_label, &module.canonical, entry, sep);
            for short in &ue.use_repo_names {
                let generated = format!("{}{}{}{}{}", owner, sep, ue.name, sep, short);
                entry
                    .entries
                    .entry(short.clone())
                    .or_insert_with(|| CanonicalRepo::new(generated));
            }
        }
    }

    // Ext-generated repo views: each generated repo inherits its
    // owner module's full apparent-name view, plus gets every
    // sibling in the same `(owner, ext_name)` group bound to its
    // short name.
    let mut by_owner_ext: BTreeMap<
        (String, String),
        Vec<&loupe_bzlmod::bzlmod_graph::ExtGenRecord>,
    > = BTreeMap::new();
    for ext in &bzlmod_graph.ext_generated {
        by_owner_ext
            .entry((ext.owner_canonical.clone(), ext.ext_name.clone()))
            .or_default()
            .push(ext);
    }
    for ((owner_canonical, _ext_name), members) in &by_owner_ext {
        let owner_canon = CanonicalRepo::new(owner_canonical.clone());
        let owner_view = per_repo_mapping
            .get(&owner_canon)
            .map(|m| m.entries.clone())
            .unwrap_or_default();
        for sibling in members {
            let sib_canon = CanonicalRepo::new(sibling.canonical.clone());
            let entry = per_repo_mapping
                .entry(sib_canon.clone())
                .or_insert_with(|| RepoMapping::owned_by(sib_canon));
            // Owner inheritance: every name the owner module
            // resolves resolves the same way inside this generated
            // repo. Lets loads like `@rules_cc//cc:defs.bzl` inside
            // a `cc_compatibility_proxy` resolve to `rules_cc~`.
            for (k, v) in &owner_view {
                entry.entries.entry(k.clone()).or_insert_with(|| v.clone());
            }
            // Owner short-name → owner canonical (so the generated
            // repo can also reference its owner under the bare
            // short name, mirroring what Bazel does).
            if let Some(owner_short) = owner_canonical.trim_end_matches(sep).split(sep).next_back()
                && !owner_short.is_empty()
            {
                entry
                    .entries
                    .entry(owner_short.to_string())
                    .or_insert_with(|| owner_canon.clone());
            }
            // Sibling mappings.
            for other in members {
                if other.canonical == sibling.canonical {
                    continue;
                }
                entry
                    .entries
                    .entry(other.repo_short.clone())
                    .or_insert_with(|| CanonicalRepo::new(other.canonical.clone()));
            }
        }
    }

    // Main module's `use_repo(ext, "foo")` calls bring ext-
    // generated apparents into main's `repo_mapping`. Same shape as
    // the transitive synthesis above, but landed on the top-level
    // map instead of per_repo_mapping. Owner = defining module's
    // canonical (resolved from `ue.bzl_label`), NOT main's empty
    // canonical, so the apparent maps to the same canonical the
    // spec registration produces.
    for ue in &main_record.use_extensions {
        let owner =
            resolve_ext_owner_canonical(&ue.bzl_label, main.canonical.as_str(), &repo_mapping, sep);
        for short in &ue.use_repo_names {
            let canonical = format!("{}{}{}{}{}", owner, sep, ue.name, sep, short);
            repo_mapping
                .entries
                .entry(short.clone())
                .or_insert_with(|| CanonicalRepo::new(canonical));
        }
    }

    // Registered toolchains: main's first, then each transitive
    // module's. Each module's labels canonicalise against that
    // module's own per_repo_mapping (already populated above).
    let mut registered_toolchains: Vec<Label> =
        canonicalise_labels(&main_record.registered_toolchains, &main_pkg, &repo_mapping);
    let mut registered_execution_platforms: Vec<Label> = canonicalise_labels(
        &main_record.registered_execution_platforms,
        &main_pkg,
        &repo_mapping,
    );
    for module in &bzlmod_graph.transitive {
        let canon = CanonicalRepo::new(module.canonical.clone());
        let pkg = PackageId::new(canon.clone(), PackagePath::root());
        let mapping = per_repo_mapping
            .get(&canon)
            .cloned()
            .unwrap_or_else(|| RepoMapping::owned_by(canon));
        registered_toolchains.extend(canonicalise_labels(
            &module.registered_toolchains,
            &pkg,
            &mapping,
        ));
        registered_execution_platforms.extend(canonicalise_labels(
            &module.registered_execution_platforms,
            &pkg,
            &mapping,
        ));
    }

    // Convert main module's use_extensions from loupe-bzlmod's
    // `UseExtensionLite` shape to loupe-core's `UseExtensionRecord`.
    // The two carry the same information; the shape difference is
    // historical (different crates, slightly different attr
    // representations).
    let use_extensions: Vec<UseExtensionRecord> = main_record
        .use_extensions
        .iter()
        .map(convert_use_extension)
        .collect();

    Ok(ModuleGraph {
        main,
        deps,
        repo_mapping,
        per_repo_mapping,
        registered_toolchains,
        registered_execution_platforms,
        use_extensions,
        canonical_separator_byte: sep as u8,
    })
}

fn convert_use_extension(ue: &loupe_bzlmod::resolution::UseExtensionLite) -> UseExtensionRecord {
    use loupe_bzlmod::RepoAttrValue;
    let tags = ue
        .tags
        .iter()
        .map(|t| TagInvocation {
            class: t.class.clone(),
            attrs: t
                .attrs
                .iter()
                .map(|(k, v)| {
                    let literal = match v {
                        RepoAttrValue::None => TagAttrLiteral::String(String::new()),
                        RepoAttrValue::String(s) => TagAttrLiteral::String(s.clone()),
                        RepoAttrValue::Int(i) => TagAttrLiteral::Int(*i),
                        RepoAttrValue::Bool(b) => TagAttrLiteral::Bool(*b),
                        RepoAttrValue::StringList(items) => {
                            TagAttrLiteral::StringList(items.clone())
                        }
                        RepoAttrValue::StringDict(d) => TagAttrLiteral::StringDict(d.clone()),
                        RepoAttrValue::StringListDict(_) => TagAttrLiteral::String(String::new()),
                    };
                    (k.clone(), literal)
                })
                .collect(),
        })
        .collect();
    UseExtensionRecord {
        bzl_label: ue.bzl_label.clone(),
        name: ue.name.clone(),
        dev_dependency: ue.dev_dependency,
        tags,
        use_repo_names: ue.use_repo_names.clone(),
    }
}

pub fn parse(workspace_root: &Path) -> Result<ModuleGraph> {
    parse_with_lockfile_override(workspace_root, None, &[])
}

/// Same as [`parse`] but lets the caller override the
/// `MODULE.bazel.lock` path and supply external repo roots. Each root
/// is a [`RepoRoot`](crate::repo_root::RepoRoot) — embedded
/// (`include_dir`-baked) or filesystem (`bazel mod vendor` output,
/// `<output_base>/external/`, etc.). External roots unlock transitive
/// `register_toolchains` / `register_execution_platforms` parsing —
/// without them only the main module's calls are captured. Bazel 7
/// writes the rich lockfile to `<output_base>/MODULE.bazel.lock`
/// rather than the workspace; callers needing the per-repo records
/// point `lockfile_path` at the `output_base` copy and pass a
/// filesystem root rooted at `<output_base>/external/`.
pub fn parse_with_lockfile_override(
    workspace_root: &Path,
    lockfile_path: Option<std::path::PathBuf>,
    external_repo_roots: &[crate::repo_root::RepoRoot],
) -> Result<ModuleGraph> {
    let module_bazel_path = workspace_root.join("MODULE.bazel");
    let module_bazel_src = std::fs::read_to_string(&module_bazel_path)
        .with_context(|| format!("reading {}", module_bazel_path.display()))?;
    let lockfile =
        match lockfile_path {
            Some(p) => crate::lockfile::load_from(p)
                .with_context(|| "loading overridden MODULE.bazel.lock")?,
            None => crate::lockfile::load(workspace_root)
                .with_context(|| "loading MODULE.bazel.lock")?,
        };
    parse_with_pre_read_inputs(
        workspace_root,
        &module_bazel_src,
        lockfile,
        external_repo_roots,
    )
}

/// Like [`parse_with_lockfile_override`] but the caller has already
/// read MODULE.bazel and constructed the [`Lockfile`] — typically
/// because both reads went through DICE-keyed `FileContentKey` lookups
/// for replay-correctness. The body is identical; only the read sites
/// move out.
pub fn parse_with_pre_read_inputs(
    workspace_root: &Path,
    module_bazel_src: &str,
    lockfile: crate::lockfile::Lockfile,
    external_repo_roots: &[crate::repo_root::RepoRoot],
) -> Result<ModuleGraph> {
    let _ = workspace_root; // reserved for future external-repo MODULE.bazel reads
    let extra_registrations: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let extra_bazel_deps: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let extra_registrations = &extra_registrations;
    let extra_bazel_deps = &extra_bazel_deps;
    let src = module_bazel_src;
    let main_eval = run_module_eval(src, "MODULE.bazel", /* lenient */ false)?;
    let main = main_eval
        .main
        .ok_or_else(|| anyhow::anyhow!("MODULE.bazel did not call module(...)"))?;
    // Re-canonicalise bazel_dep canonicals using the lockfile's
    // separator. `module_bazel_globals` hardcodes `+` (Bazel 7's
    // convention); older lockfiles (lockFileVersion 11) use `~`.
    // Without this fix, on-disk paths and lockfile per-repo mappings
    // are keyed under `<name>~` while we'd look them up by `<name>+`.
    let sep = lockfile.canonical_separator();
    let deps: Vec<ModuleDep> = main_eval
        .deps
        .into_iter()
        .map(|d| ModuleDep {
            canonical: canonicalise_with_sep(d.canonical.as_str(), sep),
            ..d
        })
        .collect();
    let mut repo_mapping = RepoMapping::owned_by(main.canonical.clone());
    repo_mapping
        .entries
        .insert(main.name.clone(), main.canonical.clone());
    for dep in &deps {
        repo_mapping
            .entries
            .insert(dep.apparent.clone(), dep.canonical.clone());
    }

    let main_pkg = PackageId::new(main.canonical.clone(), PackagePath::root());
    let mut registered_toolchains =
        canonicalise_labels(&main_eval.raw_toolchains, &main_pkg, &repo_mapping);
    let mut registered_execution_platforms =
        canonicalise_labels(&main_eval.raw_execution_platforms, &main_pkg, &repo_mapping);

    // Start from lockfile-derived per-repo mappings (rich: includes
    // module-extension-generated repos and `repo_name=` overrides).
    // The external walk below augments each module's entry with its
    // own `bazel_dep(...)` declarations — needed because the lockfile
    // only records mappings reachable from the main module's
    // extension closure, leaving transitively-loaded `.bzl`s
    // (rules_oci's `@aspect_bazel_lib` references) without a mapping.
    let mut per_repo_mapping: BTreeMap<CanonicalRepo, RepoMapping> = lockfile.per_repo_mapping;

    if !external_repo_roots.is_empty() {
        // Walk every directory in every external root that has a
        // `MODULE.bazel` — that's the full transitive set of bzlmod
        // modules in the resolved graph (Bazel materialises them all).
        // Iterating `deps` alone would miss transitive deps:
        // rules_oci's MODULE.bazel registers toolchains, but so does
        // aspect_bazel_lib's, and that's only reachable through
        // rules_oci → aspect_bazel_lib.
        //
        // Multiple roots: `bazel mod vendor` only vendors bzlmod-
        // declared repos. Repository-rule outputs marked `local =
        // True` (e.g. `host_platform_repo`) live in
        // `<output_base>/external/`. Passing both lets the walker
        // pick up either source. First root that has a given
        // canonical wins for mapping-population purposes; later
        // roots' entries skip via the `or_insert` family.
        let mut canonicals: Vec<String> = Vec::new();
        let mut all_repo_names: Vec<String> = Vec::new();
        // Record which root provided each canonical (by index into the
        // input slice) so the per-module MODULE.bazel reload below
        // reads from the right one.
        let mut canonical_to_root: BTreeMap<String, usize> = BTreeMap::new();
        for (idx, ext_root) in external_repo_roots.iter().enumerate() {
            for name in ext_root.canonicals() {
                if !all_repo_names.contains(&name) {
                    all_repo_names.push(name.clone());
                }
                if !canonicals.contains(&name)
                    && ext_root
                        .read(&name, std::path::Path::new("MODULE.bazel"))
                        .is_some()
                {
                    canonicals.push(name.clone());
                    canonical_to_root.insert(name, idx);
                }
            }
        }
        // Synthesise per-repo mappings for extension-generated repos
        // straight from their on-disk canonical names. Bazel encodes
        // each as `<owner><sep><sep><ext>(<sep><repo>)+`, where
        // `<sep>` matches the lockfile's separator. The lockfile
        // only records mappings actually USED during its creation
        // run; this directory walk fills in the rest so loads from
        // inside generated repos resolve.
        //
        // For each generated repo we register three classes of
        // entries:
        //   - the **owner** module's view → the generated repo
        //     (load `@<repo_short>` from the owner's bzl)
        //   - the generated repo's own view → its owner module
        //     (load `@<owner_apparent>` from the generated repo's
        //     bzl, e.g. for shared constants)
        //   - sibling generated repos in the same extension →
        //     each other (toolchain lists in `toolchains_repo`
        //     reference per-platform `@<platform_repo_short>`).
        let dbl = format!("{sep}{sep}");
        let mut by_extension: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for name in &all_repo_names {
            // Bazel encodes extension-generated repos two slightly
            // different ways depending on whether the owner module's
            // canonical carries a separator suffix:
            //
            // - Modules with `<name><sep>` canonicals (`aspect_bazel_lib~`):
            //   `<name><sep><sep><ext><sep><repo>` — i.e. the
            //   separator at the owner/ext boundary doubles up.
            // - Well-known modules with bare canonicals (`platforms`,
            //   `bazel_tools`, `local_config_platform`):
            //   `<name><sep><ext><sep><repo>` — single separator,
            //   because the owner's canonical doesn't carry one.
            //
            // Try the doubled form first (covers the common case),
            // fall back to a known-prefix match against well-known
            // canonicals (covers `platforms~host_platform~host_platform`).
            let parts = name
                .split_once(&dbl)
                .map(|(o, rest)| (o.to_string(), rest.to_string(), format!("{o}{sep}")))
                .or_else(|| {
                    for known in ["platforms", "bazel_tools", "local_config_platform"] {
                        let prefix = format!("{known}{sep}");
                        if let Some(rest) = name.strip_prefix(&prefix)
                            && rest.contains(sep)
                        {
                            return Some((known.to_string(), rest.to_string(), known.to_string()));
                        }
                    }
                    None
                });
            let Some((owner_short, rest, owner_canonical_str)) = parts else {
                continue;
            };
            let Some((ext_name, repo_short)) = rest.rsplit_once(sep) else {
                continue;
            };
            let owner_canonical = CanonicalRepo::new(owner_canonical_str);
            let full_canonical = CanonicalRepo::new(name.clone());
            per_repo_mapping
                .entry(owner_canonical.clone())
                .or_insert_with(|| RepoMapping::owned_by(owner_canonical.clone()))
                .entries
                .entry(repo_short.to_string())
                .or_insert(full_canonical.clone());
            per_repo_mapping
                .entry(full_canonical.clone())
                .or_insert_with(|| RepoMapping::owned_by(full_canonical.clone()))
                .entries
                .entry(owner_short.clone())
                .or_insert(owner_canonical);
            let group_key = format!("{owner_short}{dbl}{ext_name}");
            by_extension
                .entry(group_key)
                .or_default()
                .push((repo_short.to_string(), name.clone()));
        }
        for members in by_extension.values() {
            for (sibling_short, sibling_canonical) in members {
                let sibling_canonical = CanonicalRepo::new(sibling_canonical.clone());
                for (other_short, other_canonical) in members {
                    if other_short == sibling_short {
                        continue;
                    }
                    per_repo_mapping
                        .entry(sibling_canonical.clone())
                        .or_insert_with(|| RepoMapping::owned_by(sibling_canonical.clone()))
                        .entries
                        .entry(other_short.clone())
                        .or_insert_with(|| CanonicalRepo::new(other_canonical.clone()));
                }
            }
        }
        canonicals.sort(); // deterministic order
        for canonical_name in canonicals {
            let canonical = CanonicalRepo::new(canonical_name.clone());
            // Skip the main repo (already handled above) — main's
            // canonical never appears in `external/` anyway, but be
            // defensive.
            if canonical.is_main() {
                continue;
            }
            let Some(&root_idx) = canonical_to_root.get(&canonical_name) else {
                continue;
            };
            let this_root = &external_repo_roots[root_idx];
            let Some(bytes) = this_root.read(&canonical_name, std::path::Path::new("MODULE.bazel"))
            else {
                continue;
            };
            let Ok(src) = String::from_utf8(bytes) else {
                continue;
            };
            let display_name = format!("external/{canonical_name}/MODULE.bazel");
            let Ok(dep_eval) = run_module_eval(&src, &display_name, /* lenient */ true) else {
                continue;
            };
            let pkg = PackageId::new(canonical.clone(), PackagePath::root());
            // Merge synthesised mapping (this module's `bazel_dep`s)
            // with the lockfile entry (which wins on conflict — it
            // carries `repo_name=` overrides + extension-generated
            // names the synthesis can't see).
            let entry = per_repo_mapping
                .entry(canonical.clone())
                .or_insert_with(|| RepoMapping::owned_by(canonical.clone()));
            for d in &dep_eval.deps {
                let dep_canon = canonicalise_with_sep(d.canonical.as_str(), sep);
                entry.entries.entry(d.apparent.clone()).or_insert(dep_canon);
            }
            // `use_repo(<ext_proxy>, "name1", ...)` brings extension-
            // generated repos into THIS module's apparent-name view.
            // v1 doesn't track which extension a name came from — we
            // just look for an on-disk extension-generated repo
            // whose short suffix matches the name and register the
            // mapping. Wrong if two extensions in the same module
            // generate same-named repos, but Bazel rejects that
            // anyway.
            for short_name in &dep_eval.used_repo_names {
                if entry.entries.contains_key(short_name) {
                    continue;
                }
                let needle = format!("{sep}{short_name}");
                let matched = all_repo_names
                    .iter()
                    .find(|n| n.ends_with(&needle) && *n != short_name);
                if let Some(full) = matched {
                    entry
                        .entries
                        .insert(short_name.clone(), CanonicalRepo::new(full.clone()));
                }
            }
            let mapping = entry.clone();
            registered_toolchains.extend(canonicalise_labels(
                &dep_eval.raw_toolchains,
                &pkg,
                &mapping,
            ));
            registered_execution_platforms.extend(canonicalise_labels(
                &dep_eval.raw_execution_platforms,
                &pkg,
                &mapping,
            ));
        }

        // Generated repos inherit their owner module's apparent-name
        // mapping. Bazel: an extension running for module X gets X's
        // repo_mapping, and that same view applies inside the repos
        // it spawns. So `rules_oci~~oci~oci_crane_toolchains`'s
        // `defs.bzl` should resolve `@bazel_skylib` through
        // rules_oci's `bazel_dep` — not through `Label::resolve`'s
        // `<apparent>+` fallback, which produces a non-existent
        // canonical on Bazel 7's `~`-separator workspaces.
        //
        // Run this AFTER the per-canonical loop above so each owner's
        // `bazel_dep` entries are already in `per_repo_mapping[owner]`
        // and we copy a complete view into the generated repos.
        let mut inheritance: Vec<(CanonicalRepo, BTreeMap<String, CanonicalRepo>)> = Vec::new();
        for name in &all_repo_names {
            let Some((owner_short, _rest)) = name.split_once(&dbl) else {
                continue;
            };
            let owner = CanonicalRepo::new(format!("{owner_short}{sep}"));
            let owner_mapping = per_repo_mapping
                .get(&owner)
                .map(|m| m.entries.clone())
                .unwrap_or_default();
            let full = CanonicalRepo::new(name.clone());
            inheritance.push((full, owner_mapping));
        }
        for (full, owner_mapping) in inheritance {
            let entry = per_repo_mapping
                .entry(full.clone())
                .or_insert_with(|| RepoMapping::owned_by(full));
            for (k, v) in owner_mapping {
                entry.entries.entry(k).or_insert(v);
            }
        }
    }
    // `use_repo(ext, "foo")` brings `foo` into the consuming module's
    // scope, pointing at the ext-generated repo. Compute the canonical
    // (`<defining_module_canonical><sep><ext_name><sep><name>`) and add
    // to `repo_mapping`. Owner is the EXTENSION-DEFINING module (the
    // .bzl's repo), not the using module — see
    // `resolve_ext_owner_canonical`.
    for rec in &main_eval.use_extensions {
        let owner = resolve_ext_owner_canonical(
            &rec.bzl_label,
            main.canonical.as_str(),
            &repo_mapping,
            sep,
        );
        for name in &rec.use_repo_names {
            let canonical = CanonicalRepo::new(format!("{owner}{sep}{}{sep}{name}", rec.name));
            repo_mapping.entries.insert(name.clone(), canonical);
        }
    }

    // Fold in BCR-derived per-transitive-module `apparent → canonical`
    // mappings. Without this, label resolution inside e.g.
    // `@aspect_rules_js//npm:extensions.bzl` would hit the
    // `Label::resolve` fallback and produce `bazel_features+` when the
    // lockfile uses `~` — the materialised cache lives under
    // `bazel_features~` and the load 404s. The transitive walker
    // can't fill this in because the modules aren't on disk yet at
    // ModuleGraphKey-compute time.
    for (declaring_canonical, mapping) in extra_bazel_deps {
        if mapping.is_empty() {
            continue;
        }
        let declaring = CanonicalRepo::new(declaring_canonical.clone());
        let entry = per_repo_mapping
            .entry(declaring.clone())
            .or_insert_with(|| RepoMapping::owned_by(declaring));
        for (apparent, canonical) in mapping {
            entry
                .entries
                .entry(apparent.clone())
                .or_insert_with(|| CanonicalRepo::new(canonical.clone()));
        }
    }

    // Fold in transitively-registered toolchains the caller supplied
    // (BCR resolution path). Canonicalise each label against the
    // declaring module's own repo mapping — `@coreutils_toolchains//:all`
    // from `aspect_bazel_lib`'s perspective resolves through
    // aspect_bazel_lib's `use_repo` declarations, not main's.
    for (declaring_canonical, labels) in extra_registrations {
        if labels.is_empty() {
            continue;
        }
        let declaring = CanonicalRepo::new(declaring_canonical.clone());
        let pkg = PackageId::new(declaring.clone(), PackagePath::root());
        let mapping = per_repo_mapping
            .get(&declaring)
            .cloned()
            .unwrap_or_else(|| RepoMapping::owned_by(declaring));
        registered_toolchains.extend(canonicalise_labels(labels, &pkg, &mapping));
    }

    Ok(ModuleGraph {
        main,
        deps,
        repo_mapping,
        per_repo_mapping,
        registered_toolchains,
        registered_execution_platforms,
        use_extensions: main_eval.use_extensions,
        canonical_separator_byte: sep as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_minimal_module_bazel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "1.2.3")"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        assert_eq!(graph.main.name, "demo");
        assert_eq!(graph.main.version, "1.2.3");
        assert!(graph.main.canonical.is_main());
        assert_eq!(
            graph.resolve_apparent("demo").unwrap(),
            &CanonicalRepo::MAIN
        );
    }

    #[test]
    fn bazel_dep_populates_repo_mapping_with_canonical_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
bazel_dep(name = "rules_python", version = "0.31.0")
bazel_dep(name = "rules_oci", version = "2.2.6", repo_name = "oci")
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        assert_eq!(graph.deps.len(), 2);
        // Default: apparent equals name.
        assert_eq!(
            graph.resolve_apparent("rules_python").unwrap(),
            &CanonicalRepo::new("rules_python+")
        );
        // `repo_name` overrides the apparent: BUILD code can reach
        // `@oci//...` even though the canonical is still `rules_oci+`.
        assert_eq!(
            graph.resolve_apparent("oci").unwrap(),
            &CanonicalRepo::new("rules_oci+")
        );
        assert!(graph.resolve_apparent("rules_oci").is_none());
    }

    #[test]
    fn use_extension_accepts_documented_bzlmod_kwargs() {
        // Regression: `#[starlark_module]` exposes Rust param names
        // verbatim, so a leading underscore on `isolate` (or
        // `dev_dependency`) silently renames the Starlark kwarg and
        // breaks any MODULE.bazel that passes it. Bazel's own surface
        // documents both kwargs — keep the names aligned.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
ext = use_extension(
    "@foo//:e.bzl", "ext",
    dev_dependency = True,
    isolate = True,
)
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        assert_eq!(graph.use_extensions.len(), 1);
        assert_eq!(graph.use_extensions[0].name, "ext");
        assert!(graph.use_extensions[0].dev_dependency);
    }

    #[test]
    fn register_toolchains_captures_labels_in_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
register_toolchains(
    "//toolchains:tc_first",
    "@aspect_bazel_lib//toolchains:tar_toolchain",
    ":tc_local",
)
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        let names: Vec<String> = graph
            .registered_toolchains
            .iter()
            .map(Label::to_string)
            .collect();
        // `@aspect_bazel_lib` isn't in the main module's mapping (no
        // `bazel_dep`), so it falls back to the `<apparent>+` canonical
        // convention via `Label::resolve` — same caveat as elsewhere.
        assert_eq!(
            names,
            vec![
                "@@//toolchains:tc_first".to_string(),
                "@@aspect_bazel_lib+//toolchains:tar_toolchain".to_string(),
                "@@//:tc_local".to_string(),
            ],
        );
    }

    #[test]
    fn register_execution_platforms_captures_labels() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
register_execution_platforms("//platforms:linux_x86_64")
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        let names: Vec<String> = graph
            .registered_execution_platforms
            .iter()
            .map(Label::to_string)
            .collect();
        assert_eq!(names, vec!["@@//platforms:linux_x86_64".to_string()]);
    }

    #[test]
    fn register_toolchains_rejects_non_string_arg() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
register_toolchains(123)
"#,
        )
        .unwrap();
        let err = parse(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("must be a string label"),
            "expected string-label error, got: {err}"
        );
    }

    #[test]
    fn transitive_module_bazel_files_contribute_register_toolchains() {
        // Layout:
        //   <ws>/MODULE.bazel — main, depends on `dep_a`
        //   <ext>/dep_a+/MODULE.bazel — registers ":local_tc"
        // The dep's `:local_tc` should canonicalise via the dep's
        // own canonical, not the main module's repo.
        let workspace = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("MODULE.bazel"),
            r#"
module(name = "main_mod", version = "0.0.0")
bazel_dep(name = "dep_a", version = "1.0")
register_toolchains("//main_pkg:tc_main")
"#,
        )
        .unwrap();
        let dep_a_dir = ext.path().join("dep_a+");
        fs::create_dir_all(&dep_a_dir).unwrap();
        fs::write(
            dep_a_dir.join("MODULE.bazel"),
            r#"
module(name = "dep_a", version = "1.0")
register_toolchains(":local_tc", "//dep_pkg:tc_b")
"#,
        )
        .unwrap();
        // Empty workspace lockfile is fine — per_repo_mapping
        // for `dep_a+` falls back to empty, which `Label::resolve`
        // tolerates because both labels are repo-prefix-free.
        let graph = parse_with_lockfile_override(
            workspace.path(),
            None,
            &[crate::repo_root::RepoRoot::Filesystem(
                ext.path().to_path_buf(),
            )],
        )
        .unwrap();
        let names: Vec<String> = graph
            .registered_toolchains
            .iter()
            .map(Label::to_string)
            .collect();
        // Main first, then dep — declaration order across the graph.
        // Each canonicalises against its own module's repo.
        assert_eq!(
            names,
            vec![
                "@@//main_pkg:tc_main".to_string(),
                "@@dep_a+//:local_tc".to_string(),
                "@@dep_a+//dep_pkg:tc_b".to_string(),
            ],
        );
    }

    #[test]
    fn walks_transitive_of_transitive_module_bazel_files() {
        // Layout:
        //   <ws>/MODULE.bazel — main, depends only on dep_a
        //   <ext>/dep_a+/MODULE.bazel — module(...) only, no deps
        //   <ext>/dep_b+/MODULE.bazel — register_toolchains(":local_tc")
        //
        // dep_b is NOT a direct bazel_dep of main; in real Bazel it
        // would be reachable through dep_a → dep_b. We don't model
        // graph traversal — we just walk every directory under the
        // external root that has a MODULE.bazel. dep_b's
        // registrations should land in `registered_toolchains`.
        let workspace = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("MODULE.bazel"),
            r#"
module(name = "main_mod", version = "0.0.0")
bazel_dep(name = "dep_a", version = "1.0")
"#,
        )
        .unwrap();
        let dep_a = ext.path().join("dep_a+");
        std::fs::create_dir_all(&dep_a).unwrap();
        fs::write(
            dep_a.join("MODULE.bazel"),
            r#"module(name = "dep_a", version = "1.0")"#,
        )
        .unwrap();
        let dep_b = ext.path().join("dep_b+");
        std::fs::create_dir_all(&dep_b).unwrap();
        fs::write(
            dep_b.join("MODULE.bazel"),
            r#"
module(name = "dep_b", version = "1.0")
register_toolchains(":local_tc")
"#,
        )
        .unwrap();
        let graph = parse_with_lockfile_override(
            workspace.path(),
            None,
            &[crate::repo_root::RepoRoot::Filesystem(
                ext.path().to_path_buf(),
            )],
        )
        .unwrap();
        let names: Vec<String> = graph
            .registered_toolchains
            .iter()
            .map(Label::to_string)
            .collect();
        assert!(
            names.iter().any(|n| n == "@@dep_b+//:local_tc"),
            "expected dep_b's toolchain registration to be picked up via the on-disk \
             walk; got {names:?}"
        );
    }

    #[test]
    fn lenient_eval_keeps_partial_register_toolchains_on_unknown_call() {
        // `aspect_bazel_lib`-style real-world MODULE.bazel files do
        // a `register_toolchains(...)` then later call something we
        // don't model (a tag-class method on a use_extension proxy,
        // an obscure override). Without lenient eval the whole
        // module is dropped; with it, the registrations recorded
        // before the error survive.
        let workspace = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("MODULE.bazel"),
            r#"
module(name = "main_mod", version = "0.0.0")
bazel_dep(name = "dep_a", version = "1.0")
"#,
        )
        .unwrap();
        let dep_a = ext.path().join("dep_a+");
        std::fs::create_dir_all(&dep_a).unwrap();
        // First a valid registration, then an unmodelled call —
        // `register_toolchains` with a non-string positional errors
        // at *runtime*, not at compile time, so the prior call
        // survives. Real-world lenient cases (extension methods on
        // a use_extension proxy) hit this same runtime-error path.
        fs::write(
            dep_a.join("MODULE.bazel"),
            r#"
module(name = "dep_a", version = "1.0")
register_toolchains(":early_tc")
register_toolchains(123)
register_toolchains(":late_tc")
"#,
        )
        .unwrap();
        let graph = parse_with_lockfile_override(
            workspace.path(),
            None,
            &[crate::repo_root::RepoRoot::Filesystem(
                ext.path().to_path_buf(),
            )],
        )
        .unwrap();
        let names: Vec<String> = graph
            .registered_toolchains
            .iter()
            .map(Label::to_string)
            .collect();
        assert!(
            names.iter().any(|n| n == "@@dep_a+//:early_tc"),
            "expected dep_a's `:early_tc` to survive lenient eval; got {names:?}"
        );
    }

    #[test]
    fn transitive_parsing_skips_deps_without_module_bazel() {
        // dep_b+ has no MODULE.bazel in the external root — should be
        // a silent skip, not an error.
        let workspace = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("MODULE.bazel"),
            r#"
module(name = "main_mod", version = "0.0.0")
bazel_dep(name = "dep_b", version = "1.0")
"#,
        )
        .unwrap();
        // Note: no dep_b+ directory created.
        let graph = parse_with_lockfile_override(
            workspace.path(),
            None,
            &[crate::repo_root::RepoRoot::Filesystem(
                ext.path().to_path_buf(),
            )],
        )
        .unwrap();
        assert!(graph.registered_toolchains.is_empty());
    }

    #[test]
    fn missing_module_call_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MODULE.bazel"), "").unwrap();
        let err = parse(dir.path()).unwrap_err();
        assert!(err.to_string().contains("did not call module"));
    }

    #[test]
    fn well_known_modules_keep_bare_canonical() {
        // `platforms`, `bazel_tools`, `local_config_platform` are
        // special-cased on disk: Bazel doesn't suffix them with the
        // canonical separator, so neither should we when
        // re-canonicalising via the lockfile sep.
        assert_eq!(
            canonicalise_with_sep("platforms+", '~').as_str(),
            "platforms"
        );
        assert_eq!(
            canonicalise_with_sep("bazel_tools+", '~').as_str(),
            "bazel_tools"
        );
        // Everything else gets the lockfile's separator stamped on —
        // including `rules_cc`, which in modern Bazel is a regular
        // bzlmod module (`rules_cc~`).
        assert_eq!(
            canonicalise_with_sep("rules_cc+", '~').as_str(),
            "rules_cc~"
        );
        assert_eq!(
            canonicalise_with_sep("aspect_bazel_lib+", '~').as_str(),
            "aspect_bazel_lib~"
        );
        assert_eq!(
            canonicalise_with_sep("rules_oci~", '+').as_str(),
            "rules_oci+"
        );
    }
}
