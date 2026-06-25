//! `aspect()` — a reusable analysis pass attached to a target's deps.
//!
//! An aspect is declared once and applied to the targets reached through
//! the attributes named in `attr_aspects` (and to the direct targets of
//! an `attr.label(aspects=[…])`). When applied to a target it runs its
//! `implementation(target, ctx)`, registering actions and returning
//! providers that merge onto the target's own provider set — so a rule
//! depending on that target can read `dep[AspectProvider]`.
//!
//! Loupe runs aspects for real (analysis only, no action execution):
//! the [`Aspect`] value carries the implementation plus the metadata the
//! engine needs to apply it — `attr_aspects` (propagation edges),
//! `requires` (aspects that must run first on the same target),
//! `toolchains`, `fragments`, and the aspect's own `attrs`. Identity
//! (`defining_bzl` + `symbol`) is stamped at `.bzl` freeze by
//! [`crate::bzl_eval`] so the engine can recover the frozen aspect across
//! a DICE-key boundary (mirrors `RuleClass`).

use std::sync::OnceLock;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_complex_values;
use starlark::starlark_module;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

use crate::starlark_types::rule::{AttrsMap, unpack_attrs_public};

/// What `aspect()` returns. Generic over `V` so the implementation
/// function and required-aspect references freeze with their defining
/// `.bzl`.
#[derive(Debug, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<aspect>")]
pub struct AspectGen<V: ValueLifetimeless> {
    /// The aspect's `implementation` function: `impl(target, ctx)`.
    pub implementation: V,
    /// `attr_aspects = [...]` — attribute names along which the aspect
    /// propagates (e.g. `["deps"]`). The engine applies the aspect to
    /// the targets reached through these attrs of each visited target.
    pub attr_aspects: Vec<String>,
    /// `requires = [other_aspect, …]` — aspects that must run first on
    /// the same target; their providers are visible to this aspect's
    /// `target[...]` lookups.
    pub requires: Vec<V>,
    /// The aspect's own `attrs` (typically private `_foo` defaults),
    /// surfaced on the aspect ctx as `ctx.attr.<name>`.
    pub attrs: AttrsMap<V>,
    /// `toolchains = [...]` — toolchain type labels the aspect resolves
    /// for `ctx.toolchains[T]`.
    pub toolchains: Vec<String>,
    /// `fragments = [...]` — configuration fragments for `ctx.fragments`.
    pub fragments: Vec<String>,
    /// `provides = [...]` — declared provider symbols (advisory; not
    /// enforced).
    pub provides: Vec<String>,
    /// `required_providers = [...]` — Bazel's node-visit gate, as an
    /// **OR of AND-sets**: the aspect's impl runs on a node only if the
    /// node carries *every* provider in *at least one* inner set. A flat
    /// `[A, B]` is one AND-set (`[[A, B]]`); a nested `[[A, B], [C]]` is the
    /// OR. Each entry is a full provider-id string (`<bzl>%Symbol`), so
    /// two `Info`s from different `.bzl`s stay distinct. The aspect still
    /// propagates along `attr_aspects` edges regardless of the gate. Empty
    /// ⇒ the impl runs on every propagated node.
    pub required_providers: Vec<Vec<String>>,
    /// Binding symbol (`js_proto_aspect`), stamped at `.bzl` freeze.
    pub symbol: OnceLock<String>,
    /// Canonical Label of the `.bzl` that ran `aspect(...)`, stamped at
    /// freeze. With `symbol`, lets the engine recover this frozen aspect
    /// from an [`AspectRef`](crate::dice_keys) across a DICE-key boundary.
    pub defining_bzl: OnceLock<loupe_labels::Label>,
    /// Effective repo mapping of the defining `.bzl`, stamped alongside
    /// `defining_bzl` (resolves apparent names in attr/toolchain labels).
    pub repo_mapping: OnceLock<std::sync::Arc<loupe_labels::RepoMapping>>,
}

pub type Aspect<'v> = AspectGen<Value<'v>>;
pub type FrozenAspect = AspectGen<FrozenValue>;

starlark_complex_values!(Aspect);

// `AttrsMap` already hand-rolls `Trace`; the `requires`/`implementation`
// `V`s are traced by the derive. The `OnceLock`/`Vec<String>` fields are
// `#[trace(static)]`.
unsafe impl<'v> Trace<'v> for Aspect<'v> {
    fn trace(&mut self, tracer: &starlark::values::Tracer<'v>) {
        self.implementation.trace(tracer);
        for r in &mut self.requires {
            r.trace(tracer);
        }
        self.attrs.trace(tracer);
    }
}

impl<'v> Freeze for Aspect<'v> {
    type Frozen = FrozenAspect;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let mut requires = Vec::with_capacity(self.requires.len());
        for r in self.requires {
            requires.push(r.freeze(freezer)?);
        }
        let mut attrs = std::collections::BTreeMap::new();
        for (k, v) in self.attrs.0 {
            attrs.insert(k, v.freeze(freezer)?);
        }
        Ok(AspectGen {
            implementation: self.implementation.freeze(freezer)?,
            attr_aspects: self.attr_aspects,
            requires,
            attrs: AttrsMap(attrs),
            toolchains: self.toolchains,
            fragments: self.fragments,
            provides: self.provides,
            required_providers: self.required_providers,
            symbol: self.symbol,
            defining_bzl: self.defining_bzl,
            repo_mapping: self.repo_mapping,
        })
    }
}

#[starlark_value(type = "aspect")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for AspectGen<V> where Self: ProvidesStaticType<'v> {}

/// Full provider-id string (`<defining_bzl>%Symbol`, or bare symbol for
/// native) from a `required_providers` entry — the *identity* key, not a
/// bare symbol, so two `Info` providers from different `.bzl`s stay
/// distinct (see [`crate::starlark_types::provider::ProviderId`]). Reads
/// the stamped `ProviderId` off a frozen or live `ProviderType`. `None`
/// for anything that isn't a provider type.
fn provider_id_string<'v>(v: Value<'v>) -> Option<String> {
    use crate::starlark_types::provider::{FrozenProviderType, ProviderType};
    if let Some(pt) = v.downcast_ref::<FrozenProviderType>() {
        return pt.id.get().map(|id| id.to_string());
    }
    if let Some(pt) = v.downcast_ref::<ProviderType>() {
        return pt.id.get().map(|id| id.to_string());
    }
    None
}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
pub fn aspect_globals(b: &mut GlobalsBuilder) {
    /// Defines an aspect. Returns an [`Aspect`] the engine applies to the
    /// targets reached through `attr_aspects` edges (and direct
    /// `attr.label(aspects=[…])` targets), running `implementation` and
    /// merging its providers onto each visited target.
    fn aspect<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(require = named)] attr_aspects: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] required_providers: Option<Value<'v>>,
        #[starlark(require = named)] required_aspect_providers: Option<Value<'v>>,
        #[starlark(require = named)] provides: Option<Value<'v>>,
        #[starlark(require = named)] toolchains: Option<Value<'v>>,
        #[starlark(require = named)] fragments: Option<Value<'v>>,
        #[starlark(require = named)] host_fragments: Option<Value<'v>>,
        #[starlark(require = named)] apply_to_generating_rules: Option<Value<'v>>,
        #[starlark(require = named)] exec_groups: Option<Value<'v>>,
        #[starlark(require = named)] subrules: Option<Value<'v>>,
        #[starlark(require = named)] requires: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<Aspect<'v>> {
        // `required_providers` / `required_aspect_providers` gate which
        // nodes the aspect visits; Loupe applies aspects to the declared
        // edges without filtering by provider (the impls themselves guard
        // on `target[Provider]`). `host_fragments` / `apply_to_generating_rules`
        // / `exec_groups` / `subrules` are accepted-and-ignored.
        let _ = (
            required_aspect_providers,
            host_fragments,
            apply_to_generating_rules,
            exec_groups,
            subrules,
            doc,
        );
        // `required_providers` gates which propagated nodes the aspect's
        // impl runs on, as Bazel's OR-of-AND-sets. Accept a flat
        // `[ProviderType, …]` (a single AND-set: node needs all of them) or
        // the nested `[[A, B], [C]]` shape (node needs all of one inner
        // set). Each entry is a full provider-id string, so the gate keys
        // on identity, not bare symbol.
        let required_providers: Vec<Vec<String>> = {
            let id_set = |l: &starlark::values::list::ListRef<'v>| -> Vec<String> {
                l.iter().filter_map(provider_id_string).collect()
            };
            let mut out: Vec<Vec<String>> = Vec::new();
            if let Some(rp) = required_providers.filter(|x| !x.is_none())
                && let Some(l) = starlark::values::list::ListRef::from_value(rp)
            {
                let nested = l
                    .iter()
                    .all(|item| starlark::values::list::ListRef::from_value(item).is_some());
                if nested && !l.is_empty() {
                    // `[[A, B], [C]]` — each inner list is one AND-set.
                    for item in l.iter() {
                        if let Some(inner) = starlark::values::list::ListRef::from_value(item) {
                            let s = id_set(inner);
                            if !s.is_empty() {
                                out.push(s);
                            }
                        }
                    }
                } else {
                    // `[A, B]` — a single AND-set requiring all of them.
                    let s = id_set(l);
                    if !s.is_empty() {
                        out.push(s);
                    }
                }
            }
            out
        };
        let str_list = |v: Option<Value<'v>>| -> Vec<String> {
            v.filter(|x| !x.is_none())
                .and_then(starlark::values::list::ListRef::from_value)
                .map(|l| {
                    l.iter()
                        .filter_map(|i| i.unpack_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let requires_vec: Vec<Value<'v>> = requires
            .filter(|x| !x.is_none())
            .and_then(starlark::values::list::ListRef::from_value)
            .map(|l| l.iter().collect())
            .unwrap_or_default();
        // `toolchains` entries are bare label strings OR `Label(...)`
        // values (js_proto_aspect uses `Label("//js/toolchains:…")`).
        // config_common.toolchain_type(...) structs would need decoding
        // here — add when a probe surfaces one.
        let toolchains_vec: Vec<String> = toolchains
            .filter(|x| !x.is_none())
            .and_then(starlark::values::list::ListRef::from_value)
            .map(|l| {
                l.iter()
                    .filter_map(|i| {
                        i.unpack_str().map(str::to_string).or_else(|| {
                            i.downcast_ref::<loupe_labels::LabelValue>()
                                .map(|lv| lv.raw.clone())
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(AspectGen {
            implementation,
            attr_aspects: str_list(attr_aspects),
            requires: requires_vec,
            attrs: AttrsMap(unpack_attrs_public(attrs)?),
            toolchains: toolchains_vec,
            fragments: str_list(fragments),
            provides: str_list(provides),
            required_providers,
            symbol: OnceLock::new(),
            defining_bzl: OnceLock::new(),
            repo_mapping: OnceLock::new(),
        })
    }
}
