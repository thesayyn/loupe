//! `rule()` and the BUILD-time `Rule` value that records `UnconfiguredTarget`s.
//!
//! Names follow Bazel: the schema returned by `rule(...)` is a
//! [`RuleClass`]; the BUILD-callable binding (e.g. what you get from
//! `load(":defs.bzl", "hello")` and then call as `hello(name=...)`)
//! is a [`Rule`].
//!
//! Two value types:
//!
//! - [`RuleClass`] is what `rule(implementation=…, attrs=…)` returns
//!   inside a `.bzl`. It captures the attribute schema and the
//!   implementation function (a real `FrozenValue` after the `.bzl`
//!   is frozen — Phase 5 analysis invokes it).
//!
//! - [`Rule`] wraps a [`RuleClass`] in the BUILD evaluator's heap,
//!   carrying the symbol name the BUILD file uses to refer to it (the
//!   import alias) — what Bazel calls the rule's *kind* — plus the
//!   resolved `.bzl` `Label` so analysis can re-find the matching
//!   `RuleClass` through `BzlLoadKey` without holding any Starlark
//!   pointers.
//!
//! ## Why a generic `RuleClassGen<V>`?
//!
//! `rule(implementation = _impl, ...)` runs while the `.bzl` is still
//! mutable. The implementation function is at that point a
//! `Value<'v>`, but analysis (Phase 5) needs a `FrozenValue` it can
//! hold across DICE recompute boundaries. starlark-rust's standard
//! complex-value pattern is to write the type generic over `V`,
//! implement [`Freeze`] to convert `Value<'v>` → `FrozenValue`, and let
//! the framework run the conversion at module-freeze time.
//!
//! The non-V fields (`attrs`) are pure Rust data — we tag them
//! `#[trace(static)]` (no-op trace) and `#[freeze(identity)]` (Frozen
//! type same as Value type) so the derive can ignore them.

use std::collections::BTreeMap;

use allocative::Allocative;
use anyhow::{Result, anyhow};
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_complex_values;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, NoSerialize, StarlarkValue, StringValue, Trace,
    Value, ValueLifetimeless, ValueLike, starlark_value,
};

use crate::label::Label;
use crate::package::AttrValue;

use super::attr::{AttrKind, AttrSpec};

/// What `rule()` returns inside a `.bzl`. The implementation function
/// is preserved as a `Value<'v>` while the `.bzl` evaluates and gets
/// frozen alongside the rest of the module — the resulting
/// `FrozenRuleClass` is what Phase 5 analysis invokes.
#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<rule_class {} attrs>", attrs.len())]
pub struct RuleClassGen<V: ValueLifetimeless> {
    /// Attribute schema (`name` is universal and not in this map).
    /// Pure Rust data — no Starlark pointers — so trace is a no-op and
    /// freeze is identity.
    #[trace(static)]
    pub attrs: BTreeMap<String, AttrSpec>,
    /// Rule implementation function. `Value<'v>` while the `.bzl` is
    /// being evaluated; `FrozenValue` after freeze. Phase 5 invokes
    /// this through `Evaluator::eval_function` with the synthesized
    /// `ctx` as the sole positional argument.
    pub implementation: V,
}

/// `RuleClass<'v>` is what `rule()` returns into a still-mutable
/// `.bzl` heap; `FrozenRuleClass` is what survives the freeze.
pub type RuleClass<'v> = RuleClassGen<Value<'v>>;
/// Frozen form of `RuleClass`. Analysis sees this.
pub type FrozenRuleClass = RuleClassGen<FrozenValue>;

starlark_complex_values!(RuleClass);

impl<'v> Freeze for RuleClass<'v> {
    type Frozen = FrozenRuleClass;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(RuleClassGen {
            attrs: self.attrs,
            implementation: self.implementation.freeze(freezer)?,
        })
    }
}

#[starlark_value(type = "rule_class")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RuleClassGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    /// Calling a frozen `RuleClass` directly (without going through the
    /// BUILD-time `Rule` wrap) is an error. BUILD evaluation always
    /// sees `Rule`s; if this fires, something bypassed the wrap step.
    fn invoke(
        &self,
        _me: Value<'v>,
        _args: &Arguments<'v, '_>,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Err(starlark::Error::new_other(anyhow!(
            "internal: bare `RuleClass` invoked from a BUILD/ .bzl context — \
             the BUILD evaluator should have wrapped it in a `Rule`"
        )))
    }
}

/// Helper that yields the attrs of a (possibly-frozen) `RuleClass`.
pub fn rule_class_attrs<'a, 'v>(rc: RuleClassEither<'a, 'v>) -> &'a BTreeMap<String, AttrSpec> {
    match rc {
        RuleClassEither::Left(r) => &r.attrs,
        RuleClassEither::Right(r) => &r.attrs,
    }
}

/// Borrow the implementation as a `Value<'v>` (frozen variants are
/// promoted to `Value<'v>` via `to_value`).
pub fn rule_class_impl<'a, 'v>(rc: RuleClassEither<'a, 'v>) -> Value<'v> {
    match rc {
        RuleClassEither::Left(r) => r.implementation,
        RuleClassEither::Right(r) => r.implementation.to_value(),
    }
}

/// Either form of `RuleClass` returned by `RuleClass::from_value`.
pub type RuleClassEither<'a, 'v> = either::Either<&'a RuleClass<'v>, &'a FrozenRuleClass>;

/// BUILD-callable binding around a `RuleClass`. Knows the BUILD-side
/// import alias (the rule's *kind*) and the canonical `.bzl` `Label`
/// for re-resolution at analysis time.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<rule {kind} from {bzl_source}>")]
pub struct Rule {
    pub kind: String,
    /// User-facing load string, e.g. `:defs.bzl`. For diagnostics only.
    pub bzl_source: String,
    /// Canonical Label of the `.bzl` defining the underlying
    /// `RuleClass`. Phase 5 analysis forces `BzlLoadKey(bzl_label)` to
    /// recover the impl function across DICE boundaries.
    pub bzl_label: Label,
    /// Source-side symbol name inside `bzl_label`. The wrap module
    /// loaded the rule under `kind` (the BUILD-side alias); the value
    /// itself is bound to `source_symbol` in the original `.bzl`.
    pub source_symbol: String,
    /// Frozen pointer to the underlying `RuleClass`. Stable across the
    /// lifetime of the BUILD evaluation — the wrap module references
    /// the loaded `FrozenModule`'s frozen heap via `add_reference`.
    /// Used at BUILD time for attr-schema validation; analysis goes
    /// through `bzl_label` instead.
    pub rule_class: FrozenValue,
}

starlark_simple_value!(Rule);

#[starlark_value(type = "rule")]
impl<'v> StarlarkValue<'v> for Rule {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let ctx: &crate::build_eval::BuildContext = eval
            .extra
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow!("internal: Rule invoked without a BuildContext"))
            })?
            .downcast_ref::<crate::build_eval::BuildContext>()
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow!(
                    "internal: BUILD evaluator extra is not a BuildContext"
                ))
            })?;

        let rule_class_value = self.rule_class.to_value();
        let rule_class = RuleClass::from_value(rule_class_value).ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "internal: Rule's underlying frozen value is not a RuleClass"
            ))
        })?;
        let attrs_schema = rule_class_attrs(rule_class);

        let named = args
            .names_map()
            .map_err(|e| starlark::Error::new_other(anyhow!("rule call kwargs: {e}")))?;

        let attrs = validate_and_resolve(&self.kind, attrs_schema, &named, ctx)
            .map_err(starlark::Error::new_other)?;

        let name = match attrs.get("name") {
            Some(AttrValue::String(s)) => s.clone(),
            _ => {
                return Err(starlark::Error::new_other(anyhow!(
                    "rule call `{}` is missing required `name = ...`",
                    self.kind
                )));
            }
        };

        let label = crate::label::Label::new(
            ctx.package().repo.clone(),
            ctx.package().path.clone(),
            name.clone(),
        );
        ctx.record_target(crate::package::UnconfiguredTarget {
            label,
            kind: self.kind.clone(),
            bzl_source: self.bzl_source.clone(),
            bzl_label: Some(self.bzl_label.clone()),
            source_symbol: Some(self.source_symbol.clone()),
            attrs,
        })
        .map_err(starlark::Error::new_other)?;
        Ok(Value::new_none())
    }
}

fn validate_and_resolve<'v>(
    kind: &str,
    attrs_schema: &BTreeMap<String, AttrSpec>,
    named: &SmallMap<StringValue<'v>, Value<'v>>,
    ctx: &crate::build_eval::BuildContext,
) -> Result<BTreeMap<String, AttrValue>> {
    let mut out = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();

    for (k, v) in named.iter() {
        let key = k.as_str();
        seen.insert(key.to_string());
        if key == "name" || key == "visibility" || key == "tags" {
            // Universal kwargs Bazel accepts on every rule. v1 records
            // them but doesn't enforce visibility (DESIGN: "everything
            // public") or interpret tags.
            out.insert(key.to_string(), crate::build_eval::unpack_attr(*v));
            continue;
        }
        let spec = attrs_schema.get(key).ok_or_else(|| {
            anyhow!(
                "rule `{kind}` has no attribute `{key}` \
                 (declared attrs: {:?})",
                attrs_schema.keys().collect::<Vec<_>>()
            )
        })?;
        out.insert(key.to_string(), unpack_typed_attr(spec, *v, key, ctx)?);
    }

    for (attr_name, spec) in attrs_schema {
        if seen.contains(attr_name) {
            continue;
        }
        if spec.mandatory {
            return Err(anyhow!(
                "rule `{kind}` is missing required attribute `{attr_name}`"
            ));
        }
        // Apply the schema's default. Label-typed defaults stored as
        // strings get resolved here too — same path as user-provided.
        let raw_default = spec.default_attr_value();
        out.insert(
            attr_name.clone(),
            resolve_default_label_attr(spec, raw_default, ctx)?,
        );
    }
    Ok(out)
}

/// Convert a user-provided Starlark value into our captured shape,
/// honoring the attr's declared `kind`. For `Label` / `LabelList`,
/// resolves strings against the active package's `repo_mapping` —
/// downstream analysis just sees fully-canonical `Label`s.
fn unpack_typed_attr<'v>(
    spec: &AttrSpec,
    v: Value<'v>,
    key: &str,
    ctx: &crate::build_eval::BuildContext,
) -> Result<AttrValue> {
    match spec.kind {
        AttrKind::Label => {
            if v.is_none() {
                return Ok(AttrValue::None);
            }
            let s = v.unpack_str().ok_or_else(|| {
                anyhow!(
                    "attr `{key}` is a label, expected a string label, got {}",
                    v.get_type()
                )
            })?;
            let label = Label::resolve(s, ctx.package(), ctx.repo_mapping())
                .map_err(|e| anyhow!("attr `{key}`: resolving label `{s}`: {e}"))?;
            Ok(AttrValue::Label(label))
        }
        AttrKind::LabelList => {
            let list = starlark::values::list::ListRef::from_value(v).ok_or_else(|| {
                anyhow!(
                    "attr `{key}` is a label_list, expected a list of label strings, got {}",
                    v.get_type()
                )
            })?;
            let mut out = Vec::with_capacity(list.len());
            for item in list.iter() {
                let s = item
                    .unpack_str()
                    .ok_or_else(|| anyhow!("attr `{key}`: label_list entries must be strings"))?;
                let label = Label::resolve(s, ctx.package(), ctx.repo_mapping())
                    .map_err(|e| anyhow!("attr `{key}`: resolving label `{s}`: {e}"))?;
                out.push(label);
            }
            Ok(AttrValue::LabelList(out))
        }
        _ => Ok(crate::build_eval::unpack_attr(v)),
    }
}

/// `attr.label(default = ":foo")` stores its default as a `String`
/// (it can't construct a `Label` at definition time without the
/// caller's package / repo_mapping). When the rule is invoked, we
/// resolve the schema-provided default the same way as a
/// user-provided string — one path, one set of error messages.
fn resolve_default_label_attr(
    spec: &AttrSpec,
    raw: AttrValue,
    ctx: &crate::build_eval::BuildContext,
) -> Result<AttrValue> {
    match (spec.kind, raw) {
        (AttrKind::Label, AttrValue::String(s)) => Ok(AttrValue::Label(
            Label::resolve(&s, ctx.package(), ctx.repo_mapping())
                .map_err(|e| anyhow!("default label `{s}`: {e}"))?,
        )),
        (AttrKind::LabelList, AttrValue::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                if let AttrValue::String(s) = item {
                    let label = Label::resolve(&s, ctx.package(), ctx.repo_mapping())
                        .map_err(|e| anyhow!("default label `{s}`: {e}"))?;
                    out.push(label);
                } else {
                    return Err(anyhow!("label_list default must be strings"));
                }
            }
            Ok(AttrValue::LabelList(out))
        }
        (_, v) => Ok(v),
    }
}

#[starlark_module]
#[allow(clippy::too_many_arguments)] // matches Bazel's `rule()` API surface
pub fn rule_globals(b: &mut GlobalsBuilder) {
    fn rule<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        // Bazel's full surface is much larger; v1 accepts but ignores
        // the rest. Phase 5+ will start consulting some of these
        // (executable, test, fragments).
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named, default = false)] test: bool,
        #[starlark(require = named)] toolchains: Option<Value<'v>>,
        #[starlark(require = named)] fragments: Option<Value<'v>>,
        #[starlark(require = named)] provides: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<RuleClass<'v>> {
        if implementation.is_none() {
            return Err(anyhow!("rule(implementation=…) must be a callable"));
        }
        let _ = (executable, test, toolchains, fragments, provides, doc);
        let attrs_map = unpack_attrs(attrs)?;
        Ok(RuleClassGen {
            attrs: attrs_map,
            implementation,
        })
    }
}

fn unpack_attrs<'v>(v: Option<Value<'v>>) -> anyhow::Result<BTreeMap<String, AttrSpec>> {
    let v = match v {
        Some(v) if !v.is_none() => v,
        _ => return Ok(BTreeMap::new()),
    };
    let dict = starlark::values::dict::DictRef::from_value(v)
        .ok_or_else(|| anyhow!("rule(attrs=…) expects a dict, got {}", v.get_type()))?;
    let mut out = BTreeMap::new();
    for (k, val) in dict.iter() {
        let key = k
            .unpack_str()
            .ok_or_else(|| anyhow!("rule(attrs=…) keys must be strings"))?
            .to_string();
        if key == "name" || key == "visibility" || key == "tags" {
            return Err(anyhow!(
                "rule(attrs=…) cannot redefine reserved attribute `{key}`"
            ));
        }
        let spec = AttrSpec::from_value(val).ok_or_else(|| {
            anyhow!(
                "rule(attrs={{`{key}`: …}}) value must be an attr.<kind>(…) call, got {}",
                val.get_type()
            )
        })?;
        out.insert(key, spec.clone());
    }
    Ok(out)
}
