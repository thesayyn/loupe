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

use std::collections::BTreeMap;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::{Context, Result, anyhow};
use dupe::Dupe;
use pagable::Pagable;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::values::ValueLike;
use starlark::values::structs::AllocStruct;

use crate::label::Label;
use crate::package::{AttrValue, UnconfiguredTarget};
use crate::starlark_types::ctx::CtxGen;
use crate::starlark_types::provider::{
    ProviderId, ProviderInstance, ProviderType, provider_instance_fields,
};
use crate::starlark_types::rule::{FrozenRuleClass, RuleClass, rule_class_impl};
use crate::starlark_types::selector::{Selector, SelectorList, SelectorPiece};

/// Build configuration. v1 has exactly one (the empty/default config),
/// but the type is plumbed everywhere so multi-config (Phase 5+) can
/// land without API surgery.
///
/// `name` becomes the `<config>` segment in
/// `bazel-out/<config>/bin/...` paths. Bazel uses content-derived
/// strings like `k8-fastbuild`; Loupe v1 uses a fixed `"loupe"` so
/// outputs are stable for fixture comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Allocative, Pagable)]
pub struct Configuration {
    name: String,
}

impl Configuration {
    /// The default v1 configuration.
    pub fn default_v1() -> Self {
        Configuration {
            name: "loupe".to_string(),
        }
    }

    /// `<config>` segment used in `bazel-out/<config>/bin/...`.
    pub fn segment(&self) -> &str {
        &self.name
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
/// fields are the same loose `AttrValue`s `UnconfiguredTarget` uses,
/// so the whole thing round-trips through DICE without Starlark
/// pointers. Phase 5+ may grow a richer field type if rulesets need
/// `depset`-shaped provider fields.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct CapturedProvider {
    pub id: ProviderId,
    pub fields: BTreeMap<String, AttrValue>,
}

/// Result of analyzing one configured target.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct AnalyzedTarget {
    pub label: Label,
    pub configuration: Configuration,
    pub rule_kind: String,
    /// Providers in the order returned by the impl. Phase 5+ adds an
    /// indexed view (`AnalyzedTarget::provider(&ProviderId)`).
    pub providers: Vec<CapturedProvider>,
}

impl AnalyzedTarget {
    pub fn provider(&self, id: &ProviderId) -> Option<&CapturedProvider> {
        self.providers.iter().find(|p| &p.id == id)
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

/// Run analysis for one target. Synchronous — caller is responsible
/// for forcing `PackageKey` and `BzlLoadKey` ahead of time and passing
/// them in.
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

    let providers = Module::with_temp_heap(|module| -> Result<Vec<CapturedProvider>> {
        // Anchor the defining .bzl's frozen heap so the FrozenValue
        // for the impl stays alive while we invoke it.
        module
            .frozen_heap()
            .add_reference(defining_bzl.frozen_heap());

        let _globals = analysis_globals();
        let mut eval = Evaluator::new(&module);

        // Build `ctx.attr` as a struct of resolved attr values.
        let mut attr_pairs: Vec<(String, starlark::values::Value<'_>)> =
            Vec::with_capacity(target.attrs.len());
        // Universal attrs (`name`, `visibility`, `tags`) flow through
        // the same path; `attrs_schema` ignores them. `materialize_attr`
        // is the hook a future Phase will use to resolve selectors and
        // label deps differently from raw attrs — declared here so the
        // call site stays stable while the resolver grows.
        let _ = &attrs_schema;
        for (name, raw) in &target.attrs {
            let resolved = materialize_attr(raw)?;
            attr_pairs.push((name.clone(), alloc_attr_value(module.heap(), resolved)));
        }
        let attr_struct = module.heap().alloc(AllocStruct(
            attr_pairs.iter().map(|(k, v)| (k.as_str(), *v)),
        ));

        let ctx_value = module.heap().alloc(CtxGen {
            label_str: target.label.to_string(),
            workspace_name: workspace_name.to_string(),
            attr: attr_struct,
        });

        let impl_fn = rule_class_impl(either::Either::Right(frozen_rc));
        let result = eval
            .eval_function(impl_fn, &[ctx_value], &[])
            .map_err(|e| anyhow!("invoking rule `{}` impl: {e}", target.kind))?;

        process_providers(result, target.label.repo())
    })?;

    let _ = configuration; // configuration is plumbed; v1 doesn't read it yet.
    Ok(AnalyzedTarget {
        label: target.label.clone(),
        configuration: configuration.clone(),
        rule_kind: target.kind.clone(),
        providers,
    })
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
    }
}

/// Convert the impl's return value into `CapturedProvider`s. Bazel
/// requires it to be a list of provider instances; v1 also tolerates
/// the empty list.
fn process_providers<'v>(
    result: starlark::values::Value<'v>,
    repo_for_unattributed: &crate::label::CanonicalRepo,
) -> Result<Vec<CapturedProvider>> {
    let _ = repo_for_unattributed;
    let list = starlark::values::list::ListRef::from_value(result).ok_or_else(|| {
        anyhow!(
            "rule impl must return a list of providers, got {}",
            result.get_type()
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
        let (id, fields_pairs) = match pi {
            either::Either::Left(live) => {
                let type_value = live.type_ptr.to_value();
                let id = provider_id_from_type(type_value)?;
                (id, provider_instance_fields(live))
            }
            either::Either::Right(frozen) => {
                let type_value = frozen.type_ptr.to_value();
                let id = provider_id_from_type(type_value)?;
                (id, provider_instance_fields(frozen))
            }
        };
        let mut fields = BTreeMap::new();
        for (k, v) in fields_pairs {
            fields.insert(k.to_string(), unpack_provider_field(v));
        }
        out.push(CapturedProvider { id, fields });
    }
    Ok(out)
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

/// Unpack a Starlark value into our captured `AttrValue` form. Mirrors
/// `build_eval::unpack_attr` (which is BUILD-side); kept separate
/// because analysis-side values can include providers/depsets/etc.
/// the BUILD-side path doesn't need to handle yet.
fn unpack_provider_field<'v>(v: starlark::values::Value<'v>) -> AttrValue {
    if v.is_none() {
        return AttrValue::None;
    }
    if let Some(s) = v.unpack_str() {
        return AttrValue::String(s.to_string());
    }
    if let Some(b) = v.unpack_bool() {
        return AttrValue::Bool(b);
    }
    if let Some(i) = v.unpack_i32() {
        return AttrValue::Int(i as i64);
    }
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        return AttrValue::List(list.iter().map(unpack_provider_field).collect());
    }
    AttrValue::Opaque(v.to_string())
}

fn analysis_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(crate::starlark_types::register_for_analysis)
        .build()
}

/// Suppress unused-import warning on platforms that don't use this.
#[allow(dead_code)]
fn _touch() {
    let _: Option<&RuleClass<'_>> = None;
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
            defining_bzl: Label::in_package(pkg(), "defs.bzl"),
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
        assert_eq!(a.segment(), "loupe");
    }

    #[test]
    fn provider_lookup_finds_by_id() {
        let mut fields = BTreeMap::new();
        fields.insert("greeting".into(), AttrValue::String("hi".into()));
        let target = AnalyzedTarget {
            label: Label::in_package(pkg(), "world"),
            configuration: Configuration::default(),
            rule_kind: "hello".into(),
            providers: vec![CapturedProvider {
                id: pid("HelloInfo"),
                fields: fields.clone(),
            }],
        };
        let found = target.provider(&pid("HelloInfo")).unwrap();
        assert_eq!(found.fields, fields);
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
