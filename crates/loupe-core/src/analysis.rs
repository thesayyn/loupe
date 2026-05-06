//! Analysis — turning `UnconfiguredTarget`s into `AnalyzedTarget`s.
//!
//! Phase 5 deliverables (per `PLAN.md`):
//! > Implement `Configuration`, `AnalysisKey`, the analysis compute
//! > function (recursive analysis of label deps, select resolution,
//! > ctx construction, rule impl invocation), `Target` value with
//! > provider indexing, `ctx` minimum (attr, label, workspace_name).
//!
//! The compute function:
//!
//! 1. Force `PackageKey` for the target's package, look up the
//!    `UnconfiguredTarget`.
//! 2. Force `BzlLoadKey` for the rule's defining `.bzl`, find the
//!    `FrozenRuleClass` it defines.
//! 3. (Phase 5 stub: no `attr.label` yet, so no recursive analysis of
//!    dep labels. The plumbing is here so Phase 5+ can drop it in.)
//! 4. (Phase 5 stub: no `select()` resolution since fixture #1 has no
//!    selectors. `resolve_selector` exists for the next ruleset.)
//! 5. Build a Starlark heap for the analysis run, allocate `ctx` with
//!    resolved attrs, invoke the impl through `Evaluator::eval_function`.
//! 6. Process the impl's return value — must be a list of provider
//!    instances — into `Vec<CapturedProvider>` and freeze.
//!
//! Everything in `AnalyzedTarget` is plain Rust data. No Starlark
//! pointers cross the DICE boundary — that's the DESIGN.md invariant
//! for cross-recompute persistence.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::{Context, Result, anyhow};
use dupe::Dupe;
use pagable::Pagable;
use starlark::any::ProvidesStaticType;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::values::structs::AllocStruct;
use starlark::values::{FrozenValue, ValueLike};

use crate::action::Action;
use crate::label::{Label, PackageId};
use crate::package::{AttrValue, UnconfiguredTarget};
use crate::starlark_types::ctx::CtxGen;
use crate::starlark_types::file::File;
use crate::starlark_types::provider::{ProviderId, ProviderInstance, ProviderType};
use crate::starlark_types::rule::FrozenRuleClass;
use crate::starlark_types::selector::{Selector, SelectorList, SelectorPiece};

/// Build configuration. v1 has exactly one (the empty/default config),
/// but the type is plumbed everywhere so multi-config (Phase 5+) can
/// land without API surgery.
///
/// `name` becomes the `<config>` segment in
/// `bazel-out/<config>/bin/...` paths. Bazel uses content-derived
/// strings like `k8-fastbuild`; Loupe v1 uses a fixed placeholder
/// (`"fake-cfg"`) so outputs are stable for fixture comparisons. The
/// segment name is intentionally non-product-y so a human eyeballing
/// `bazel-out/...` paths never mistakes a Loupe-emitted path for one
/// real Bazel produced.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Allocative, Pagable)]
pub struct Configuration {
    name: String,
    /// Target platform label, if set. Currently only consumed by the
    /// toolchain resolver for `target_compatible_with` matching;
    /// other Bazel uses of platform (constraint setting fallback,
    /// transitions) are STUB.
    target_platform: Option<crate::label::Label>,
}

impl Configuration {
    /// The default v1 configuration. The segment name is a fixed
    /// non-product placeholder (`"fake-cfg"`) so paths in test output
    /// don't accidentally read like a product name.
    pub fn default_v1() -> Self {
        Configuration {
            name: "fake-cfg".to_string(),
            target_platform: None,
        }
    }

    /// Set the target platform — a `platform(...)` target's label.
    /// Constraint matching (`target_compatible_with`) only kicks in
    /// when this is set; with no platform, the resolver falls back
    /// to first-match-wins on `toolchain_type`.
    pub fn with_target_platform(mut self, label: crate::label::Label) -> Self {
        self.target_platform = Some(label);
        self
    }

    /// `<config>` segment used in `bazel-out/<config>/bin/...`.
    pub fn segment(&self) -> &str {
        &self.name
    }

    pub fn target_platform(&self) -> Option<&crate::label::Label> {
        self.target_platform.as_ref()
    }
}

impl Default for Configuration {
    fn default() -> Self {
        Self::default_v1()
    }
}

impl std::fmt::Display for Configuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl Dupe for Configuration {}

/// One provider on an analyzed target. Identity is the `ProviderId`;
/// fields are live `FrozenValue`s into `AnalyzedTarget.analysis_heap`,
/// preserving the impl's full typed shape (lists of `File`, `depset`,
/// structs, …). Reading a field requires the consumer to anchor that
/// heap via `frozen_heap().add_reference(...)` first — see DESIGN.md
/// "non-negotiable invariants".
#[derive(Debug, Clone, Allocative)]
pub struct CapturedProvider {
    pub id: ProviderId,
    pub fields: BTreeMap<String, FrozenValue>,
}

impl PartialEq for CapturedProvider {
    fn eq(&self, other: &Self) -> bool {
        // Equality on (id, field-name set + per-field FrozenValue
        // pointer identity). Two analyses producing the same providers
        // typically allocate distinct frozen heaps, so PartialEq on
        // CapturedProvider is most useful inside a single
        // analysis-output module.
        self.id == other.id
            && self.fields.len() == other.fields.len()
            && self.fields.iter().all(|(k, v)| {
                other
                    .fields
                    .get(k)
                    .map(|w| v.to_value().ptr_eq(w.to_value()))
                    .unwrap_or(false)
            })
    }
}
impl Eq for CapturedProvider {}

/// Result of analyzing one configured target.
#[derive(Debug, Clone, Allocative)]
pub struct AnalyzedTarget {
    pub label: Label,
    pub configuration: Configuration,
    pub rule_kind: String,
    /// Providers in the order returned by the impl. `AnalyzedTarget::provider`
    /// looks up by `ProviderId`.
    pub providers: Vec<CapturedProvider>,
    /// Actions the impl recorded via `ctx.actions.*`. Empty for
    /// source-file targets.
    pub actions: Vec<Action>,
    /// The frozen analysis-output heap that owns every `FrozenValue`
    /// in `providers` (and `actions`'s `File`s, transitively). Held as
    /// `Arc<FrozenModule>` so an `AnalyzedTarget` is cheap to dupe and
    /// the heap stays pinned for the engine's lifetime. Downstream
    /// rules that read a dep's providers anchor this heap on their
    /// own analysis module.
    pub analysis_heap: Arc<FrozenModule>,
}

impl PartialEq for AnalyzedTarget {
    fn eq(&self, other: &Self) -> bool {
        self.label == other.label
            && self.configuration == other.configuration
            && self.rule_kind == other.rule_kind
            && Arc::ptr_eq(&self.analysis_heap, &other.analysis_heap)
            && self.providers == other.providers
            && self.actions == other.actions
    }
}
impl Eq for AnalyzedTarget {}

impl AnalyzedTarget {
    pub fn provider(&self, id: &ProviderId) -> Option<&CapturedProvider> {
        self.providers.iter().find(|p| &p.id == id)
    }

    /// The `DefaultInfo` provider — Bazel guarantees every analyzed
    /// target has one (synthesized empty by `run_analysis` if the
    /// impl didn't return one explicitly).
    pub fn default_info(&self) -> &CapturedProvider {
        self.provider(&crate::starlark_types::default_info::default_info_id())
            .expect("internal: every AnalyzedTarget must carry a DefaultInfo")
    }

    /// `DefaultInfo.files` flattened to a `Vec<File>`. v1 accepts
    /// either a flat list or a depset of `File`; everything else
    /// errors via [`unpack_default_files`]. Empty when the impl
    /// didn't set `files=` (synthesized DefaultInfo).
    pub fn default_files(&self) -> Vec<crate::starlark_types::file::File> {
        let p = self.default_info();
        match p.fields.get("files") {
            None => Vec::new(),
            Some(v) => unpack_default_files(v.to_value()).unwrap_or_default(),
        }
    }

    /// `DefaultInfo.executable` if set; `None` otherwise.
    pub fn default_executable(&self) -> Option<crate::starlark_types::file::File> {
        let p = self.default_info();
        p.fields
            .get("executable")
            .and_then(|v| crate::starlark_types::file::File::from_value(v.to_value()).cloned())
    }

    /// Find an action whose primary output matches `short_path`.
    /// Convenience for tests / aquery-style inspectors.
    pub fn action_for_output(&self, short_path: &str) -> Option<&Action> {
        self.actions.iter().find(|a| {
            a.primary_output()
                .map(|f| f.short_path() == short_path)
                .unwrap_or(false)
        })
    }
}

/// Build the `ctx.files`, `ctx.file`, `ctx.executable` structs by
/// walking the analyzed target's label-typed attrs and pulling each
/// dep's `DefaultInfo` field. All three live as fields on `Ctx` so
/// `ctx.files.<attr>` is plain attribute access at impl-call time.
///
/// - `ctx.files.<attr>`: a flat `[File]` of every dep's
///   `DefaultInfo.files`, in dep order.
/// - `ctx.file.<attr>`: only set for `attr.label` (single-dep) attrs;
///   value is the dep's `DefaultInfo.files[0]` or `None` if empty.
/// - `ctx.executable.<attr>`: only set for `attr.label(executable=True)`
///   attrs; value is the dep's `DefaultInfo.executable`.
fn build_dep_file_structs<'v>(
    heap: starlark::values::Heap<'v>,
    target: &UnconfiguredTarget,
    resolved_deps: &BTreeMap<String, ResolvedDep>,
    attrs_schema: &BTreeMap<String, crate::starlark_types::attr::AttrSpec>,
) -> (
    starlark::values::Value<'v>,
    starlark::values::Value<'v>,
    starlark::values::Value<'v>,
) {
    let _ = target;
    let mut files_pairs: Vec<(String, starlark::values::Value<'v>)> = Vec::new();
    let mut file_pairs: Vec<(String, starlark::values::Value<'v>)> = Vec::new();
    let mut exec_pairs: Vec<(String, starlark::values::Value<'v>)> = Vec::new();

    for (attr_name, dep) in resolved_deps {
        let spec = attrs_schema.get(attr_name);

        // Flatten every dep's DefaultInfo.files into one list per
        // attr. `ctx.files.deps` is what most ruleset code iterates.
        let mut all_files: Vec<starlark::values::Value<'v>> = Vec::new();
        for at in dep.iter_targets() {
            for f in at.default_files() {
                all_files.push(heap.alloc(f));
            }
        }
        files_pairs.push((attr_name.clone(), heap.alloc(all_files)));

        // ctx.file.<attr>: single-dep attrs only.
        if let ResolvedDep::Single(at) = dep {
            let single = at.default_files().into_iter().next();
            let v = match single {
                Some(f) => heap.alloc(f),
                None => starlark::values::Value::new_none(),
            };
            file_pairs.push((attr_name.clone(), v));
        }

        // ctx.executable.<attr>: only for `attr.label(executable=True)`.
        let exec_dep = match (spec, dep) {
            (Some(s), ResolvedDep::Single(at)) if s.executable => Some(at),
            _ => None,
        };
        if let Some(at) = exec_dep {
            let v = match at.default_executable() {
                Some(f) => heap.alloc(f),
                None => starlark::values::Value::new_none(),
            };
            exec_pairs.push((attr_name.clone(), v));
        }
    }

    let files = heap.alloc(AllocStruct(
        files_pairs.iter().map(|(k, v)| (k.as_str(), *v)),
    ));
    let file = heap.alloc(AllocStruct(
        file_pairs.iter().map(|(k, v)| (k.as_str(), *v)),
    ));
    let exec = heap.alloc(AllocStruct(
        exec_pairs.iter().map(|(k, v)| (k.as_str(), *v)),
    ));
    (files, file, exec)
}

/// Unpack a Starlark value into a flat `Vec<File>`. v1 accepts a
/// flat list of `File`. STUB: real `depset(File)` traversal lands
/// when a ruleset surfaces one — until then, ruleset code that
/// passes `depset(...)` to `DefaultInfo.files=` fails here with a
/// type error and the ruleset must wrap its accumulator in
/// `depset(...).to_list()`.
pub fn unpack_default_files<'v>(
    v: starlark::values::Value<'v>,
) -> Result<Vec<crate::starlark_types::file::File>> {
    use crate::starlark_types::file::File;
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        return list
            .iter()
            .map(|item| {
                File::from_value(item).cloned().ok_or_else(|| {
                    anyhow!(
                        "DefaultInfo.files entry is not a File (got {})",
                        item.get_type()
                    )
                })
            })
            .collect();
    }
    Err(anyhow!(
        "DefaultInfo.files must be a list of File (got {})",
        v.get_type()
    ))
}

/// Per-`run_analysis` mutable state. Lives in `Evaluator::extra` so
/// `ctx.actions.*` and friends can record into it without the impl
/// passing handles around. Mirrors the BUILD-side `BuildContext`.
#[derive(Debug, ProvidesStaticType)]
pub struct AnalysisContext {
    pub package: PackageId,
    pub configuration: Configuration,
    pub target_label: Label,
    /// Actions recorded by `ctx.actions.{write, run, run_shell,
    /// symlink}`. Order matches call order — Bazel's aquery output
    /// preserves declaration order.
    pub actions: RefCell<Vec<Action>>,
    /// Files declared via `ctx.actions.declare_file`. Indexed by
    /// short_path (the workspace-relative path) so a future
    /// `ctx.outputs` view can surface them by attr name.
    pub declared_files: RefCell<BTreeMap<String, File>>,
}

impl AnalysisContext {
    pub fn new(target_label: Label, configuration: Configuration) -> Self {
        let package = target_label.package.clone();
        AnalysisContext {
            package,
            configuration,
            target_label,
            actions: RefCell::new(Vec::new()),
            declared_files: RefCell::new(BTreeMap::new()),
        }
    }
}

/// Configuration-time resolution of a `SelectorList`. Phase 4
/// preserved selectors verbatim through loading; Phase 5 picks a
/// branch per the active configuration.
///
/// v1 implements only the trivial case: every selector resolves to
/// its `//conditions:default` branch. That's enough for fixture #1
/// (no selects) and for any ruleset that doesn't `select()` on real
/// platform constraints. Phase 5+ wires `Configuration` into actual
/// constraint matching.
pub fn resolve_selector(list: &SelectorList) -> Result<AttrValue> {
    let mut concatenated: Vec<AttrValue> = Vec::new();
    let mut all_lists = true;
    for piece in &list.pieces {
        let resolved = match piece {
            SelectorPiece::Concrete(v) => v.clone(),
            SelectorPiece::Selector(sel) => resolve_one_selector(sel)?,
        };
        if !matches!(resolved, AttrValue::List(_)) {
            all_lists = false;
        }
        concatenated.push(resolved);
    }
    if concatenated.len() == 1 {
        return Ok(concatenated.into_iter().next().unwrap());
    }
    if all_lists {
        let mut joined = Vec::new();
        for v in concatenated {
            if let AttrValue::List(items) = v {
                joined.extend(items);
            }
        }
        return Ok(AttrValue::List(joined));
    }
    Err(anyhow!(
        "select-list with non-list pieces is not supported in v1"
    ))
}

fn resolve_one_selector(sel: &Selector) -> Result<AttrValue> {
    if let Some(v) = sel.branches.get("//conditions:default") {
        return Ok(v.clone());
    }
    Err(anyhow!(
        "v1 only resolves select() to //conditions:default; \
         got branches {:?} (no_match_error: {:?})",
        sel.branches.keys().collect::<Vec<_>>(),
        sel.no_match_error
    ))
}

/// Per-attr resolved dependency from `compute_analysis`.
///
/// Mirrors `AttrKind::{Label, LabelList}`: `Single` for `attr.label`,
/// `List` for `attr.label_list`. Each carries the dep's
/// fully-analyzed `AnalyzedTarget`.
#[derive(Debug, Clone)]
pub enum ResolvedDep {
    Single(Arc<AnalyzedTarget>),
    List(Vec<Arc<AnalyzedTarget>>),
}

/// Run analysis for one target. Synchronous — caller is responsible
/// for forcing `PackageKey`, `BzlLoadKey`, and recursive `AnalysisKey`
/// for each label-typed attr ahead of time, and passing the resolved
/// deps in via `resolved_deps`.
///
/// `defining_bzl` is the `FrozenModule` produced by the rule's
/// defining `.bzl`. Its frozen heap is anchored on the analysis
/// module so the `FrozenRuleClass` we look up by symbol stays valid
/// while we invoke the impl.
pub fn run_analysis(
    target: &UnconfiguredTarget,
    defining_bzl: Arc<FrozenModule>,
    rule_symbol: &str,
    workspace_name: &str,
    configuration: &Configuration,
    resolved_deps: &BTreeMap<String, ResolvedDep>,
    resolved_toolchains: crate::starlark_types::toolchains::Toolchains,
) -> Result<AnalyzedTarget> {
    let rule_value = defining_bzl
        .get_option(rule_symbol)
        .with_context(|| format!("looking up `{}` in defining .bzl", rule_symbol))?
        .ok_or_else(|| {
            anyhow!(
                "internal: defining .bzl no longer exports `{}`",
                rule_symbol
            )
        })?;

    let frozen_rc = rule_value
        .value()
        .downcast_ref::<FrozenRuleClass>()
        .ok_or_else(|| {
            anyhow!(
                "`{}` is not a RuleClass (got {})",
                rule_symbol,
                rule_value.value().get_type()
            )
        })?;

    let attrs_schema = frozen_rc.attrs.clone();
    let _ = &attrs_schema;
    // The impl FrozenValue lives on `defining_bzl`'s heap, but we
    // can't construct a `Value<'v>` for it out here without a target
    // heap — so we re-promote inside the closure.
    let impl_fn_frozen: starlark::values::FrozenValue = frozen_rc.implementation;

    let actx = AnalysisContext::new(target.label.clone(), configuration.clone());

    // Build the analysis module on a temp Heap, evaluate the impl,
    // and freeze. The temp `Heap` itself goes away when the closure
    // returns, but the produced `FrozenModule` carries an independent
    // `FrozenHeap` whose `Arc` we hold from then on — and that's what
    // owns every provider field's `FrozenValue`.
    let frozen_module: Arc<FrozenModule> =
        Module::with_temp_heap(|module| -> Result<Arc<FrozenModule>> {
            // Anchor every frozen heap that contributes to the
            // analysis: the defining `.bzl` (rule impl + provider
            // types), and each dep's `analysis_heap` (so FrozenValues
            // surfaced through `Target` / `ctx.attr.<dep>` stay valid
            // after this module's own freeze).
            module
                .frozen_heap()
                .add_reference(defining_bzl.frozen_heap());
            for dep in resolved_deps.values() {
                for at in dep.iter_targets() {
                    module
                        .frozen_heap()
                        .add_reference(at.analysis_heap.frozen_heap());
                }
            }
            // Anchor each resolved toolchain impl's heap, too — the
            // ToolchainInfo provider's FrozenValues live there and
            // ctx.toolchains[T] hands them through to the consumer.
            for at in resolved_toolchains.by_type.values() {
                module
                    .frozen_heap()
                    .add_reference(at.analysis_heap.frozen_heap());
            }

            let _globals = analysis_globals();

            let result_value = {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&actx);

                // Build `ctx.attr` as a struct of resolved attr
                // values. Label-typed attrs become `Target` /
                // `[Target]` from `resolved_deps`; everything else
                // routes through `alloc_attr_value`.
                let mut attr_pairs: Vec<(String, starlark::values::Value<'_>)> =
                    Vec::with_capacity(target.attrs.len());
                for (name, raw) in &target.attrs {
                    let v = match resolved_deps.get(name) {
                        Some(ResolvedDep::Single(t)) => module
                            .heap()
                            .alloc(crate::starlark_types::target::Target::new(Arc::clone(t))),
                        Some(ResolvedDep::List(ts)) => {
                            let xs: Vec<starlark::values::Value<'_>> = ts
                                .iter()
                                .map(|t| {
                                    module
                                        .heap()
                                        .alloc(crate::starlark_types::target::Target::new(
                                            Arc::clone(t),
                                        ))
                                })
                                .collect();
                            module.heap().alloc(xs)
                        }
                        None => alloc_attr_value(module.heap(), materialize_attr(raw)?),
                    };
                    attr_pairs.push((name.clone(), v));
                }
                let attr_struct = module.heap().alloc(AllocStruct(
                    attr_pairs.iter().map(|(k, v)| (k.as_str(), *v)),
                ));

                // Build `ctx.files` / `ctx.file` / `ctx.executable`
                // by walking the resolved deps and reading each
                // dep's `DefaultInfo`. These three structs are
                // pre-built once per analysis so the impl reads them
                // as plain attribute access without us doing any
                // per-read work.
                let (files_struct, file_struct, exec_struct) =
                    build_dep_file_structs(module.heap(), target, resolved_deps, &attrs_schema);

                let toolchains_value = module.heap().alloc(resolved_toolchains.clone());
                let ctx_value = module.heap().alloc(CtxGen {
                    label_str: target.label.to_string(),
                    workspace_name: workspace_name.to_string(),
                    config_segment: configuration.segment().to_string(),
                    attr: attr_struct,
                    files: files_struct,
                    file: file_struct,
                    executable: exec_struct,
                    toolchains: toolchains_value,
                });

                eval.eval_function(impl_fn_frozen.to_value(), &[ctx_value], &[])
                    .map_err(|e| anyhow!("invoking rule `{}` impl: {e}", target.kind))?
            };

            // Stash the impl's return so its FrozenValues survive
            // the freeze and we can walk them out by name.
            module.set("__providers__", result_value);
            let frozen = module
                .freeze()
                .map_err(|e| anyhow!("freezing analysis module for `{}`: {e:?}", target.label))?;
            Ok(Arc::new(frozen))
        })?;

    let mut providers = collect_providers_from_frozen(&frozen_module)?;
    // Bazel guarantees every analyzed target carries a `DefaultInfo`.
    // If the impl didn't return one, synthesize an empty one — empty
    // `files`, no `executable`, no `runfiles`. Downstream
    // `target.files` / `ctx.files.<dep>` accesses then return empty
    // collections instead of erroring on a missing provider.
    let default_id = crate::starlark_types::default_info::default_info_id();
    if !providers.iter().any(|p| p.id == default_id) {
        providers.push(CapturedProvider {
            id: default_id,
            fields: BTreeMap::new(),
        });
    }
    let actions = actx.actions.into_inner();

    Ok(AnalyzedTarget {
        label: target.label.clone(),
        configuration: configuration.clone(),
        rule_kind: target.kind.clone(),
        providers,
        actions,
        analysis_heap: frozen_module,
    })
}

impl ResolvedDep {
    fn iter_targets(&self) -> Box<dyn Iterator<Item = &Arc<AnalyzedTarget>> + '_> {
        match self {
            ResolvedDep::Single(t) => Box::new(std::iter::once(t)),
            ResolvedDep::List(ts) => Box::new(ts.iter()),
        }
    }
}

/// Pull the impl's `[Provider, ...]` return from the frozen analysis
/// module's `__providers__` binding and reconstruct a `Vec<CapturedProvider>`
/// holding `FrozenValue` references into the same heap. Provider field
/// values are not copied.
fn collect_providers_from_frozen(frozen: &FrozenModule) -> Result<Vec<CapturedProvider>> {
    let result = frozen
        .get_option("__providers__")
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| anyhow!("internal: analysis module missing `__providers__`"))?;
    let result_value = result.value();
    let list = starlark::values::list::ListRef::from_value(result_value).ok_or_else(|| {
        anyhow!(
            "rule impl must return a list of providers, got {}",
            result_value.get_type()
        )
    })?;
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        let pi = ProviderInstance::from_value(item).ok_or_else(|| {
            anyhow!(
                "rule impl returned non-provider in list: got {}",
                item.get_type()
            )
        })?;
        let frozen_pi = match pi {
            either::Either::Right(f) => f,
            either::Either::Left(_) => {
                return Err(anyhow!(
                    "internal: provider instance in frozen module is still live — \
                     all values should have been frozen"
                ));
            }
        };
        let id = provider_id_from_type(frozen_pi.type_ptr.to_value())?;
        let fields: BTreeMap<String, FrozenValue> = frozen_pi
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        out.push(CapturedProvider { id, fields });
    }
    Ok(out)
}

/// Resolve a captured `AttrValue` for use by the impl. Phase 5 doesn't
/// have configurable attrs in `UnconfiguredTarget` yet (selectors are
/// captured as `Opaque(...)` strings in v1's pre-Phase-5 capture), so
/// this is currently a passthrough. Wiring up `select()`-typed attrs
/// is a Phase 5+ step.
fn materialize_attr(v: &AttrValue) -> Result<AttrValue> {
    Ok(v.clone())
}

fn alloc_attr_value<'v>(
    heap: starlark::values::Heap<'v>,
    v: AttrValue,
) -> starlark::values::Value<'v> {
    match v {
        AttrValue::None => starlark::values::Value::new_none(),
        AttrValue::Bool(b) => starlark::values::Value::new_bool(b),
        AttrValue::Int(i) => heap.alloc(i),
        AttrValue::String(s) => heap.alloc(s),
        AttrValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(alloc_attr_value(heap, item));
            }
            heap.alloc(out)
        }
        AttrValue::Dict(map) => {
            let pairs: Vec<(String, AttrValue)> = map.into_iter().collect();
            let alloc_pairs: Vec<(String, starlark::values::Value<'v>)> = pairs
                .into_iter()
                .map(|(k, v)| (k, alloc_attr_value(heap, v)))
                .collect();
            heap.alloc(AllocStruct(
                alloc_pairs.iter().map(|(k, v)| (k.as_str(), *v)),
            ))
        }
        AttrValue::Opaque(s) => heap.alloc(s),
        // Fallback only — `compute_analysis` substitutes a `Target`
        // value for label-typed attrs before we get here. Reaching
        // this branch means analysis was driven without
        // `resolved_deps`, e.g. from a unit test that bypasses DICE;
        // surface the canonical-string form so the impl can still
        // string-compare for diagnostics.
        AttrValue::Label(l) => heap.alloc(l.to_string()),
        AttrValue::LabelList(ls) => {
            let xs: Vec<starlark::values::Value<'v>> =
                ls.into_iter().map(|l| heap.alloc(l.to_string())).collect();
            heap.alloc(xs)
        }
    }
}

fn provider_id_from_type<'v>(type_value: starlark::values::Value<'v>) -> Result<ProviderId> {
    let pt = type_value.downcast_ref::<ProviderType>().ok_or_else(|| {
        anyhow!(
            "internal: ProviderInstance.type_ptr is not a ProviderType (got {})",
            type_value.get_type()
        )
    })?;
    pt.id
        .get()
        .cloned()
        .ok_or_else(|| anyhow!("internal: ProviderType is missing its ProviderId stamp"))
}

fn analysis_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(crate::starlark_types::register_for_analysis)
        .build()
}

/// Build an empty frozen analysis heap for synthetic targets that
/// don't run an impl (e.g. `source_file` from `exports_files`). Such
/// targets still need `analysis_heap` plumbed so the type stays
/// uniform — they just don't allocate any FrozenValue-backed
/// providers.
pub fn empty_analysis_heap() -> Arc<FrozenModule> {
    Module::with_temp_heap(|module| {
        Arc::new(
            module
                .freeze()
                .expect("freezing an empty Module never fails"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{CanonicalRepo, PackagePath};
    use std::collections::BTreeMap;

    fn pkg() -> crate::label::PackageId {
        crate::label::PackageId::new(CanonicalRepo::MAIN, PackagePath::root())
    }

    fn pid(symbol: &str) -> ProviderId {
        ProviderId {
            defining_bzl: Some(Label::in_package(pkg(), "defs.bzl")),
            symbol: symbol.to_string(),
        }
    }

    #[test]
    fn configuration_dupe_yields_equal_value() {
        // DICE values must implement Dupe. Smoke-check that cloning
        // preserves the configured segment.
        let a = Configuration::default();
        let b = a.clone();
        assert_eq!(a, b);
        assert_eq!(a.segment(), "fake-cfg");
    }

    #[test]
    fn provider_lookup_finds_by_id() {
        // The provider list is searched by `ProviderId`; equality
        // tolerates the same `Arc<FrozenModule>` analysis_heap so the
        // synthetic target here doesn't allocate a fresh heap per
        // assertion.
        let heap = empty_analysis_heap();
        let target = AnalyzedTarget {
            label: Label::in_package(pkg(), "world"),
            configuration: Configuration::default(),
            rule_kind: "hello".into(),
            providers: vec![CapturedProvider {
                id: pid("HelloInfo"),
                fields: BTreeMap::new(),
            }],
            actions: Vec::new(),
            analysis_heap: heap,
        };
        let found = target.provider(&pid("HelloInfo")).unwrap();
        assert_eq!(found.id, pid("HelloInfo"));
        assert!(target.provider(&pid("NotThere")).is_none());
    }

    fn list(items: Vec<&str>) -> AttrValue {
        AttrValue::List(
            items
                .into_iter()
                .map(|s| AttrValue::String(s.into()))
                .collect(),
        )
    }

    #[test]
    fn resolve_selector_picks_default_branch() {
        let mut branches = BTreeMap::new();
        branches.insert("//cpu:k8".into(), list(vec!["k8.cc"]));
        branches.insert("//conditions:default".into(), list(vec!["default.cc"]));
        let sel = SelectorList {
            pieces: vec![SelectorPiece::Selector(Selector {
                branches,
                no_match_error: None,
            })],
        };
        assert_eq!(resolve_selector(&sel).unwrap(), list(vec!["default.cc"]));
    }

    #[test]
    fn resolve_selector_concatenates_lists_in_piece_order() {
        // `[a] + select({default: [middle]}) + [c]` resolves to the
        // flat list `[a, middle, c]`.
        let mut branches = BTreeMap::new();
        branches.insert("//conditions:default".into(), list(vec!["middle"]));
        let sel = SelectorList {
            pieces: vec![
                SelectorPiece::Concrete(list(vec!["a"])),
                SelectorPiece::Selector(Selector {
                    branches,
                    no_match_error: None,
                }),
                SelectorPiece::Concrete(list(vec!["c"])),
            ],
        };
        assert_eq!(
            resolve_selector(&sel).unwrap(),
            list(vec!["a", "middle", "c"])
        );
    }

    #[test]
    fn resolve_selector_errors_when_default_is_missing() {
        // v1 hasn't taught the resolver about real platform constraints,
        // so the only branch it picks is `//conditions:default`. A
        // selector without one is an error — better to surface than to
        // silently pick something arbitrary.
        let mut branches = BTreeMap::new();
        branches.insert("//cpu:k8".into(), list(vec!["k8.cc"]));
        let sel = SelectorList {
            pieces: vec![SelectorPiece::Selector(Selector {
                branches,
                no_match_error: Some("explicit error".into()),
            })],
        };
        let err = resolve_selector(&sel).unwrap_err();
        assert!(err.to_string().contains("//conditions:default"));
    }

    #[test]
    fn provider_id_display_uses_bazel_percent_form() {
        // `<bzl>%<symbol>` matches Bazel's cquery output: e.g.
        // `//:defs.bzl%HelloInfo`. The `bzl` half displays canonically
        // as `@@//:defs.bzl`.
        let id = pid("HelloInfo");
        assert_eq!(id.to_string(), "@@//:defs.bzl%HelloInfo");
    }
}
