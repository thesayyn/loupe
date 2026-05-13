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
use starlark::values::list::UnpackList;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, NoSerialize, StarlarkValue, StringValue, Trace,
    Value, ValueLifetimeless, ValueLike, starlark_value,
};

use crate::label::Label;
use crate::package::AttrValue;

use super::attr::{AttrCfgGen, AttrKind, AttrSpec, AttrSpecGen};

/// What `rule()` returns inside a `.bzl`. The implementation function
/// is preserved as a `Value<'v>` while the `.bzl` evaluates and gets
/// frozen alongside the rest of the module — the resulting
/// `FrozenRuleClass` is what Phase 5 analysis invokes.
#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<rule_class {} attrs>", attrs.len())]
pub struct RuleClassGen<V: ValueLifetimeless> {
    /// Attribute schema (`name` is universal and not in this map).
    /// Generic over V because `AttrSpecGen.cfg` may carry a transition
    /// function — a `Value<'v>` while the `.bzl` is being evaluated, a
    /// `FrozenValue` after freeze. Wrapped in [`AttrsMap`] so we can
    /// hand-roll the `Trace` impl `BTreeMap` lacks.
    pub attrs: AttrsMap<V>,
    /// `rule(executable=True)` — rule produces an executable. v1
    /// stores it; the auto-declared `ctx.outputs.executable` Bazel
    /// gives executable rules isn't surfaced (impls can still set
    /// `DefaultInfo(executable=...)` themselves, which is the
    /// modern style).
    #[trace(static)]
    pub executable: bool,
    /// `rule(toolchains=[...])` — toolchain type labels the rule
    /// declares. v1 plumbs the list to `ctx.toolchains` (a dict
    /// keyed by these strings, currently with empty-struct values
    /// — STUB; the synthetic registry that supplies real toolchain
    /// data lands with the rules_oci fixture).
    #[trace(static)]
    pub toolchains: Vec<String>,
    /// Stamped at .bzl-freeze time with the rule's binding symbol
    /// (e.g. `oci_image_rule` for `oci_image_rule = rule(...)`).
    /// Used as the target's `kind` when the rule class is invoked
    /// directly from inside a macro (bypassing the BUILD-time
    /// `Rule` wrap).
    #[trace(static)]
    pub kind: std::sync::OnceLock<String>,
    /// Stamped at .bzl-freeze time with the canonical Label of the
    /// `.bzl` that ran `rule(...)`. Analysis uses this to recover
    /// the FrozenRuleClass when an `UnconfiguredTarget` was recorded
    /// via direct `RuleClass::invoke`.
    #[trace(static)]
    pub defining_bzl: std::sync::OnceLock<crate::label::Label>,
    /// Effective repo mapping for the `.bzl`'s defining repo, stamped
    /// alongside `defining_bzl`. Resolves apparent names embedded in
    /// `attr.label(default = "@external_repo//...")` defaults — those
    /// strings are written from the .bzl's perspective, not the
    /// invoking BUILD's. Without this, defaults like rules_oci's
    /// `attr.label(default = "@oci_crane_toolchains//:current_toolchain")`
    /// fall through to `Label::resolve`'s `<apparent>+` fallback
    /// (which, on Bazel 7's `~`-separator workspaces, points at a
    /// non-existent canonical).
    #[trace(static)]
    pub repo_mapping:
        std::sync::OnceLock<std::collections::BTreeMap<String, crate::label::CanonicalRepo>>,
    /// Rule implementation function. `Value<'v>` while the `.bzl` is
    /// being evaluated; `FrozenValue` after freeze. Phase 5 invokes
    /// this through `Evaluator::eval_function` with the synthesized
    /// `ctx` as the sole positional argument.
    pub implementation: V,
    /// `rule(initializer = fn)` — kwargs-rewrite hook run when the
    /// rule is invoked from BUILD. `fn(**kwargs)` returns a dict that
    /// replaces the user's kwargs before attr validation. Vendored
    /// `config_setting` uses this to remap `values={"k": v}` into
    /// `flag_values={"@bazel_tools//command_line_option:k": v}`.
    /// `None` (or no-op) when not declared.
    pub initializer: Option<V>,
}

/// `RuleClass<'v>` is what `rule()` returns into a still-mutable
/// `.bzl` heap; `FrozenRuleClass` is what survives the freeze.
pub type RuleClass<'v> = RuleClassGen<Value<'v>>;
/// Frozen form of `RuleClass`. Analysis sees this.
pub type FrozenRuleClass = RuleClassGen<FrozenValue>;

/// Newtype around the attrs `BTreeMap` so we can attach a `Trace`
/// impl. starlark-rust's blanket trace impls don't cover `BTreeMap`,
/// but the values (`AttrSpecGen<V>`) carry a live `V` in
/// `AttrCfgGen::Transition(V)` and so do need traversal.
#[derive(Debug, Allocative)]
#[repr(C)]
pub struct AttrsMap<V: ValueLifetimeless>(pub BTreeMap<String, AttrSpecGen<V>>);

impl<V: ValueLifetimeless> Default for AttrsMap<V> {
    fn default() -> Self {
        AttrsMap(BTreeMap::new())
    }
}

impl<V: ValueLifetimeless> std::ops::Deref for AttrsMap<V> {
    type Target = BTreeMap<String, AttrSpecGen<V>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<V: ValueLifetimeless> std::ops::DerefMut for AttrsMap<V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

unsafe impl<'v, V: starlark::values::Trace<'v> + ValueLifetimeless> starlark::values::Trace<'v>
    for AttrsMap<V>
{
    fn trace(&mut self, tracer: &starlark::values::Tracer<'v>) {
        for spec in self.0.values_mut() {
            // Only `cfg: AttrCfgGen<V>` holds a live V; the other
            // AttrSpecGen fields are pure Rust.
            if let AttrCfgGen::Transition(v) = &mut spec.cfg {
                starlark::values::Trace::trace(v, tracer);
            }
        }
    }
}

starlark_complex_values!(RuleClass);

impl<'v> Freeze for RuleClass<'v> {
    type Frozen = FrozenRuleClass;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let mut attrs = BTreeMap::new();
        for (k, v) in self.attrs.0 {
            attrs.insert(k, v.freeze(freezer)?);
        }
        Ok(RuleClassGen {
            attrs: AttrsMap(attrs),
            executable: self.executable,
            toolchains: self.toolchains,
            kind: self.kind,
            defining_bzl: self.defining_bzl,
            repo_mapping: self.repo_mapping,
            implementation: self.implementation.freeze(freezer)?,
            initializer: match self.initializer {
                Some(v) => Some(v.freeze(freezer)?),
                None => None,
            },
        })
    }
}

#[starlark_value(type = "rule_class")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RuleClassGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    /// Bare `RuleClass(...)` from inside a `.bzl` macro. Bazel
    /// supports this — `oci_image = macro(...)` calls
    /// `oci_image_rule(...)` directly. The kind/defining_bzl get
    /// stamped on each `RuleClass` at .bzl freeze time
    /// ([`bzl_eval::stamp_rule_class_origin`]).
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let kind = self
            .kind
            .get()
            .cloned()
            .unwrap_or_else(|| "<unstamped_rule>".to_string());
        let defining_bzl = self.defining_bzl.get().cloned();
        let repo_mapping = self.repo_mapping.get();

        let ctx: &crate::build_eval::BuildContext = eval
            .extra
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow!(
                    "internal: bare RuleClass invoked without a BuildContext (\
                     macros must run during BUILD eval)"
                ))
            })?
            .downcast_ref::<crate::build_eval::BuildContext>()
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow!(
                    "internal: BUILD evaluator extra is not a BuildContext"
                ))
            })?;

        let attrs_schema = &self.attrs;
        let named = args
            .names_map()
            .map_err(|e| starlark::Error::new_other(anyhow!("rule call kwargs: {e}")))?;
        let named = apply_initializer(self.initializer.map(|v| v.to_value()), named, eval)
            .map_err(starlark::Error::new_other)?;
        let attrs = validate_and_resolve(
            &kind,
            attrs_schema,
            &named,
            ctx,
            defining_bzl.as_ref(),
            repo_mapping,
        )
        .map_err(starlark::Error::new_other)?;
        let name = match attrs.get("name") {
            Some(AttrValue::String(s)) => s.clone(),
            _ => {
                return Err(starlark::Error::new_other(anyhow!(
                    "rule call `{}` is missing required `name = ...`",
                    kind
                )));
            }
        };
        let label =
            crate::label::Label::new(ctx.package().repo.clone(), ctx.package().path.clone(), name);
        ctx.record_target(crate::package::UnconfiguredTarget {
            label,
            kind: kind.clone(),
            bzl_source: defining_bzl
                .as_ref()
                .map(|l| l.to_string())
                .unwrap_or_else(|| "<macro>".to_string()),
            bzl_label: defining_bzl,
            source_symbol: Some(kind),
            attrs,
        })
        .map_err(starlark::Error::new_other)?;
        Ok(Value::new_none())
    }
}

// Note: `rule_class_attrs` was removed when `AttrSpecGen<V>` became
// generic — live vs frozen attrs no longer have the same type, so a
// `Either`-style accessor returning a single borrow can't unify them.
// Each `invoke` path now dispatches via [`RuleClassEither`] and calls
// [`validate_and_resolve`] (generic over `V`) directly with the
// per-arm attrs reference.

/// Borrow the stamped repo mapping of a (possibly-frozen) `RuleClass`,
/// if any. `None` means the `.bzl` evaluator hadn't run
/// `stamp_rule_class_origin` yet — possible for inline rule
/// definitions in tests, never for a load-graph-resolved rule.
pub fn rule_class_repo_mapping<'a, 'v>(
    rc: RuleClassEither<'a, 'v>,
) -> Option<&'a BTreeMap<String, crate::label::CanonicalRepo>> {
    match rc {
        RuleClassEither::Left(r) => r.repo_mapping.get(),
        RuleClassEither::Right(r) => r.repo_mapping.get(),
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

/// Borrow the optional `initializer` as a `Value<'v>`.
pub fn rule_class_initializer<'a, 'v>(rc: RuleClassEither<'a, 'v>) -> Option<Value<'v>> {
    match rc {
        RuleClassEither::Left(r) => r.initializer,
        RuleClassEither::Right(r) => r.initializer.map(|v| v.to_value()),
    }
}

/// Apply a rule's `initializer = fn` hook to the user's kwargs.
/// Calls `fn(**named)` and returns the resulting dict's entries as a
/// new `SmallMap<StringValue<'v>, Value<'v>>`. Returns the input
/// untouched when no initializer is set.
///
/// Usage: `config_setting(values={"compilation_mode":
/// "opt"})` flows through an initializer that rewrites `values=` into
/// `flag_values={"@bazel_tools//command_line_option:compilation_mode":
/// "opt"}`. Without this, the BUILD-side `values=` attr is silently
/// dropped by the underlying rule's schema.
fn apply_initializer<'v>(
    initializer: Option<Value<'v>>,
    named: SmallMap<StringValue<'v>, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Result<SmallMap<StringValue<'v>, Value<'v>>> {
    let Some(initializer) = initializer else {
        return Ok(named);
    };
    // Pass user kwargs as **kwargs to the initializer.
    let named_pairs: Vec<(&str, Value<'v>)> = named.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    let returned = eval
        .eval_function(initializer, &[], &named_pairs)
        .map_err(|e| anyhow!("rule initializer: {e}"))?;
    if returned.is_none() {
        // Initializer returned None — keep original kwargs (matches
        // the "no rewrite needed" path).
        return Ok(named);
    }
    let Some(dict) = starlark::values::dict::DictRef::from_value(returned) else {
        return Err(anyhow!(
            "rule initializer must return a dict (or None), got {}",
            returned.get_type()
        ));
    };
    let mut out = SmallMap::with_capacity(dict.len());
    for (k, v) in dict.iter() {
        let Some(key) = starlark::values::StringValue::new(k) else {
            return Err(anyhow!(
                "rule initializer returned a dict with non-string key: {k}"
            ));
        };
        out.insert(key, v);
    }
    Ok(out)
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
        let repo_mapping = rule_class_repo_mapping(rule_class);
        let initializer = rule_class_initializer(rule_class);

        let named = args
            .names_map()
            .map_err(|e| starlark::Error::new_other(anyhow!("rule call kwargs: {e}")))?;
        let named =
            apply_initializer(initializer, named, eval).map_err(starlark::Error::new_other)?;

        let attrs = match rule_class {
            RuleClassEither::Left(r) => validate_and_resolve(
                &self.kind,
                &r.attrs,
                &named,
                ctx,
                Some(&self.bzl_label),
                repo_mapping,
            ),
            RuleClassEither::Right(r) => validate_and_resolve(
                &self.kind,
                &r.attrs,
                &named,
                ctx,
                Some(&self.bzl_label),
                repo_mapping,
            ),
        }
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

fn validate_and_resolve<'v, V: ValueLifetimeless>(
    kind: &str,
    attrs_schema: &BTreeMap<String, AttrSpecGen<V>>,
    named: &SmallMap<StringValue<'v>, Value<'v>>,
    ctx: &crate::build_eval::BuildContext,
    defining_bzl: Option<&Label>,
    repo_mapping: Option<&BTreeMap<String, crate::label::CanonicalRepo>>,
) -> Result<BTreeMap<String, AttrValue>> {
    let mut out = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();

    for (k, v) in named.iter() {
        let key = k.as_str();
        seen.insert(key.to_string());
        if matches!(
            key,
            "name"
                | "visibility"
                | "tags"
                | "compatible_with"
                | "exec_compatible_with"
                | "target_compatible_with"
                | "restricted_to"
                | "testonly"
                | "deprecation"
                | "features"
                | "licenses"
                | "package_metadata"
                | "applicable_licenses"
                | "toolchains"
        ) {
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
        let raw_default = spec.default_attr_value();
        out.insert(
            attr_name.clone(),
            resolve_default_label_attr(spec, raw_default, ctx, defining_bzl, repo_mapping)?,
        );
    }
    Ok(out)
}

/// Convert a user-provided Starlark value into our captured shape,
/// honoring the attr's declared `kind`. For `Label` / `LabelList`,
/// resolves strings against the active package's `repo_mapping` —
/// downstream analysis just sees fully-canonical `Label`s.
fn unpack_typed_attr<'v, V: ValueLifetimeless>(
    spec: &AttrSpecGen<V>,
    v: Value<'v>,
    key: &str,
    ctx: &crate::build_eval::BuildContext,
) -> Result<AttrValue> {
    match spec.kind {
        AttrKind::Label => {
            if v.is_none() {
                return Ok(AttrValue::None);
            }
            // `attr.label(default = select({...}))` — preserve the
            // selector with each branch's value coerced to a Label.
            if let Some(sl) = v.downcast_ref::<crate::starlark_types::selector::SelectorList>() {
                let coerced = coerce_selector(sl, AttrKind::Label, ctx, key)?;
                return Ok(AttrValue::Selector(coerced));
            }
            // Bazel rules accept either a string label or a `Label(...)`
            // value (`aspect_bazel_lib`'s `write_source_files` macro
            // forwards already-resolved Label values into `_diff_test`).
            // Both unpack to the canonical string representation.
            let s = label_arg_as_str(v).ok_or_else(|| {
                anyhow!(
                    "attr `{key}` is a label, expected a string label, got {}",
                    v.get_type()
                )
            })?;
            let label = Label::resolve(&s, ctx.package(), ctx.repo_mapping())
                .map_err(|e| anyhow!("attr `{key}`: resolving label `{s}`: {e}"))?;
            Ok(AttrValue::Label(label))
        }
        AttrKind::LabelList => {
            // `attr.label_list(default = select({...}))` — same shape
            // as `attr.label`, but each branch is a list of label
            // strings → `AttrValue::LabelList`.
            if let Some(sl) = v.downcast_ref::<crate::starlark_types::selector::SelectorList>() {
                let coerced = coerce_selector(sl, AttrKind::LabelList, ctx, key)?;
                return Ok(AttrValue::Selector(coerced));
            }
            let list = starlark::values::list::ListRef::from_value(v).ok_or_else(|| {
                anyhow!(
                    "attr `{key}` is a label_list, expected a list of label strings, got {}",
                    v.get_type()
                )
            })?;
            let mut out = Vec::with_capacity(list.len());
            for item in list.iter() {
                let s = label_arg_as_str(item).ok_or_else(|| {
                    anyhow!("attr `{key}`: label_list entries must be strings or Label values")
                })?;
                let label = Label::resolve(&s, ctx.package(), ctx.repo_mapping())
                    .map_err(|e| anyhow!("attr `{key}`: resolving label `{s}`: {e}"))?;
                out.push(label);
            }
            Ok(AttrValue::LabelList(out))
        }
        AttrKind::LabelKeyedStringDict => {
            // `attr.label_keyed_string_dict(default = select({...}))` —
            // selector branches each carry a dict; coerce the branch
            // values to `AttrValue::LabelKeyedStringDict` so analysis-
            // time resolution lands on the same canonical shape.
            if let Some(sl) = v.downcast_ref::<crate::starlark_types::selector::SelectorList>() {
                let coerced = coerce_selector(sl, AttrKind::LabelKeyedStringDict, ctx, key)?;
                return Ok(AttrValue::Selector(coerced));
            }
            let dict = starlark::values::dict::DictRef::from_value(v).ok_or_else(|| {
                anyhow!(
                    "attr `{key}` is a label_keyed_string_dict, expected a dict, got {}",
                    v.get_type()
                )
            })?;
            let mut out = std::collections::BTreeMap::new();
            for (k, val) in dict.iter() {
                let k_str = label_arg_as_str(k).ok_or_else(|| {
                    anyhow!(
                        "attr `{key}`: label_keyed_string_dict keys must be strings or Label values, got {}",
                        k.get_type()
                    )
                })?;
                let label = Label::resolve(&k_str, ctx.package(), ctx.repo_mapping())
                    .map_err(|e| anyhow!("attr `{key}`: resolving label `{k_str}`: {e}"))?;
                let v_str = val.unpack_str().ok_or_else(|| {
                    anyhow!(
                        "attr `{key}`: label_keyed_string_dict values must be strings, got {}",
                        val.get_type()
                    )
                })?;
                out.insert(label, v_str.to_string());
            }
            Ok(AttrValue::LabelKeyedStringDict(out))
        }
        _ => Ok(crate::build_eval::unpack_attr(v)),
    }
}

/// Walk each piece of a `SelectorList` for an `attr.label` /
/// `attr.label_list` attr and coerce branch values (currently
/// `String` / `List<String>` because `unpack_attr` is type-blind) to
/// canonical `Label` / `LabelList`. Selectors keyed on
/// `//conditions:default` are coerced too. Returns a
/// fully-canonicalised `SelectorList` ready for analysis-time
/// resolution to land on `AttrValue::Label{,List}`.
fn coerce_selector(
    sl: &crate::starlark_types::selector::SelectorList,
    kind: AttrKind,
    ctx: &crate::build_eval::BuildContext,
    key: &str,
) -> Result<crate::starlark_types::selector::SelectorList> {
    use crate::starlark_types::selector::{Selector, SelectorList, SelectorPiece};
    let mut pieces = Vec::with_capacity(sl.pieces.len());
    for piece in &sl.pieces {
        let coerced_piece = match piece {
            SelectorPiece::Concrete(v) => {
                SelectorPiece::Concrete(coerce_branch_value(v, kind, ctx, key)?)
            }
            SelectorPiece::Selector(sel) => {
                let mut branches = std::collections::BTreeMap::new();
                for (k, v) in &sel.branches {
                    branches.insert(k.clone(), coerce_branch_value(v, kind, ctx, key)?);
                }
                SelectorPiece::Selector(Selector {
                    branches,
                    no_match_error: sel.no_match_error.clone(),
                })
            }
        };
        pieces.push(coerced_piece);
    }
    Ok(SelectorList { pieces })
}

fn coerce_branch_value(
    v: &AttrValue,
    kind: AttrKind,
    ctx: &crate::build_eval::BuildContext,
    key: &str,
) -> Result<AttrValue> {
    match kind {
        AttrKind::Label => match v {
            AttrValue::String(s) => {
                let label = Label::resolve(s, ctx.package(), ctx.repo_mapping())
                    .map_err(|e| anyhow!("attr `{key}`: resolving label `{s}` in select(): {e}"))?;
                Ok(AttrValue::Label(label))
            }
            AttrValue::None => Ok(AttrValue::None),
            other => Err(anyhow!(
                "attr `{key}` (label) select() branch value not a string: {other:?}"
            )),
        },
        AttrKind::LabelList => match v {
            AttrValue::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        AttrValue::String(s) => {
                            let label = Label::resolve(s, ctx.package(), ctx.repo_mapping())
                                .map_err(|e| {
                                    anyhow!(
                                        "attr `{key}`: resolving label `{s}` in select() branch: {e}"
                                    )
                                })?;
                            out.push(label);
                        }
                        other => {
                            return Err(anyhow!(
                                "attr `{key}` (label_list) select() branch list \
                                 contains non-string entry: {other:?}"
                            ));
                        }
                    }
                }
                Ok(AttrValue::LabelList(out))
            }
            other => Err(anyhow!(
                "attr `{key}` (label_list) select() branch value not a list: {other:?}"
            )),
        },
        AttrKind::LabelKeyedStringDict => match v {
            AttrValue::Dict(items) => {
                let mut out = std::collections::BTreeMap::new();
                for (k, val) in items {
                    let label = Label::resolve(k, ctx.package(), ctx.repo_mapping())
                        .map_err(|e| {
                            anyhow!(
                                "attr `{key}`: resolving label `{k}` in select() branch: {e}"
                            )
                        })?;
                    let s = match val {
                        AttrValue::String(s) => s.clone(),
                        other => {
                            return Err(anyhow!(
                                "attr `{key}` (label_keyed_string_dict) select() branch \
                                 contains non-string value: {other:?}"
                            ));
                        }
                    };
                    out.insert(label, s);
                }
                Ok(AttrValue::LabelKeyedStringDict(out))
            }
            other => Err(anyhow!(
                "attr `{key}` (label_keyed_string_dict) select() branch value not a dict: {other:?}"
            )),
        },
        _ => Ok(v.clone()),
    }
}

/// `attr.label(default = "descriptor.sh")` stores its default as a
/// `String`. Bazel resolves attr defaults relative to the **rule's
/// defining `.bzl` package** — `"descriptor.sh"` from a rule in
/// `@@rules_oci+//oci/private` becomes `@@rules_oci+//oci/private:descriptor.sh`,
/// not `@@//:descriptor.sh` (the BUILD's package). v1 follows that
/// rule when the caller supplies `defining_bzl`; falling back to the
/// BUILD package preserves behaviour for rules the test suite
/// declares inline.
fn resolve_default_label_attr<V: ValueLifetimeless>(
    spec: &AttrSpecGen<V>,
    raw: AttrValue,
    ctx: &crate::build_eval::BuildContext,
    defining_bzl: Option<&Label>,
    repo_mapping: Option<&BTreeMap<String, crate::label::CanonicalRepo>>,
) -> Result<AttrValue> {
    let default_pkg = defining_bzl
        .map(|l| crate::label::PackageId::new(l.repo().clone(), l.package_path().clone()));
    let resolve_one = |s: &str| -> Result<Label> {
        let pkg = default_pkg.as_ref().unwrap_or_else(|| ctx.package());
        // The .bzl-side default is written from the .bzl's
        // perspective, so apparent names like `@oci_crane_toolchains`
        // must resolve through the .bzl's repo mapping — not the
        // invoking BUILD's. `repo_mapping` is the stamped
        // copy; fall back to the BUILD's mapping when this rule was
        // declared inline (no defining .bzl).
        let mapping = repo_mapping.unwrap_or_else(|| ctx.repo_mapping());
        Label::resolve(s, pkg, mapping).map_err(|e| anyhow!("default label `{s}`: {e}"))
    };
    match (spec.kind, raw) {
        (AttrKind::Label, AttrValue::String(s)) => Ok(AttrValue::Label(resolve_one(&s)?)),
        (AttrKind::LabelList, AttrValue::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                if let AttrValue::String(s) = item {
                    out.push(resolve_one(&s)?);
                } else {
                    return Err(anyhow!("label_list default must be strings"));
                }
            }
            Ok(AttrValue::LabelList(out))
        }
        (AttrKind::LabelKeyedStringDict, AttrValue::Dict(items)) => {
            // `default=` was captured as a `BTreeMap<String, String>`
            // (unresolved label-string → value). Resolve each key from
            // the defining `.bzl`'s perspective.
            let mut out = std::collections::BTreeMap::new();
            for (k, val) in items {
                let label = resolve_one(&k)?;
                let s = match val {
                    AttrValue::String(s) => s,
                    other => {
                        return Err(anyhow!(
                            "label_keyed_string_dict default values must be strings, got {other:?}"
                        ));
                    }
                };
                out.insert(label, s);
            }
            Ok(AttrValue::LabelKeyedStringDict(out))
        }
        (_, v) => Ok(v),
    }
}

#[starlark_module]
#[allow(clippy::too_many_arguments)] // matches Bazel's `rule()` API surface
pub fn rule_globals(b: &mut GlobalsBuilder) {
    fn rule<'v>(
        // `rule(impl, attrs=...)` (positional) and
        // `rule(implementation=impl, attrs=...)` (named) both reach
        // Bazel's surface; rules_shell's `sh_library` uses positional.
        #[starlark(require = pos)] implementation_pos: Option<Value<'v>>,
        #[starlark(require = named)] implementation: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named, default = false)] test: bool,
        // `toolchains=[...]` accepts both strings and `Label(...)` /
        // `config_common.toolchain_type(...)` values; coerce each to
        // its string form via `label_arg_as_str`.
        #[starlark(require = named)] toolchains: Option<UnpackList<Value<'v>>>,
        // Bazel's full surface is much larger; v1 accepts but ignores
        // these. Phase 5+ will surface fragments / provides / doc as
        // they get exercised.
        #[starlark(require = named)] fragments: Option<Value<'v>>,
        #[starlark(require = named)] provides: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
        // Legacy / migration-flag kwargs Bazel rules still pass for
        // backwards compatibility — accept but ignore.
        #[starlark(require = named)] incompatible_use_toolchain_transition: Option<Value<'v>>,
        #[starlark(require = named)] _skylark_testable: Option<Value<'v>>,
        #[starlark(require = named)] host_fragments: Option<Value<'v>>,
        #[starlark(require = named)] outputs: Option<Value<'v>>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named)] exec_groups: Option<Value<'v>>,
        #[starlark(require = named)] exec_compatible_with: Option<Value<'v>>,
        #[starlark(require = named)] analysis_test: Option<Value<'v>>,
        #[starlark(require = named)] build_setting: Option<Value<'v>>,
        #[starlark(require = named)] subrules: Option<Value<'v>>,
        // Vendored-upstream extras Loupe doesn't enforce but must
        // accept so the embedded tree freezes:
        //   - `needs = [...]` — capability list (configuration,
        //     default_exec_group, make_variables, targetPlatform).
        //   - `initializer = fn` — kwargs-rewrite hook run at rule
        //     instantiation; not invoked yet, but parse must accept.
        #[starlark(require = named)] needs: Option<Value<'v>>,
        #[starlark(require = named)] initializer: Option<Value<'v>>,
    ) -> anyhow::Result<RuleClass<'v>> {
        let _ = needs;
        let implementation = match (implementation_pos, implementation) {
            (Some(p), None) => p,
            (None, Some(n)) => n,
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "rule() got both positional and named implementation; pick one"
                ));
            }
            (None, None) => {
                return Err(anyhow!(
                    "rule(implementation=…) is required and must be a callable"
                ));
            }
        };
        if implementation.is_none() {
            return Err(anyhow!("rule(implementation=…) must be a callable"));
        }
        let _ = (
            test,
            fragments,
            provides,
            doc,
            incompatible_use_toolchain_transition,
            _skylark_testable,
            host_fragments,
            outputs,
            cfg,
            exec_groups,
            exec_compatible_with,
            analysis_test,
            build_setting,
            subrules,
        );
        let mut attrs_map = unpack_attrs(attrs)?;
        synthesize_implicit_attrs(&mut attrs_map, test, executable);
        if build_setting.is_some() && !build_setting.unwrap().is_none() {
            // `rule(build_setting = config.<kind>(...))` — Bazel auto-
            // adds a `build_setting_default` attr typed to match the
            // build_setting kind. Loupe doesn't track the kind through
            // the `config.<x>(…)` wildcard, so a permissive `String`
            // attr is enough to keep BUILD-side
            // `bool_flag(build_setting_default=True)` from rejecting.
            attrs_map
                .entry("build_setting_default".to_string())
                .or_insert_with(|| AttrSpecGen {
                    kind: AttrKind::String,
                    default: None,
                    mandatory: false,
                    doc: String::new(),
                    executable: false,
                    providers: Vec::new(),
                    cfg: AttrCfgGen::Inherited,
                });
        }
        let toolchains_list: Vec<String> = toolchains
            .map(|l| l.items.into_iter().filter_map(label_arg_as_str).collect())
            .unwrap_or_default();
        Ok(RuleClassGen {
            attrs: AttrsMap(attrs_map),
            executable,
            toolchains: toolchains_list,
            kind: std::sync::OnceLock::new(),
            defining_bzl: std::sync::OnceLock::new(),
            repo_mapping: std::sync::OnceLock::new(),
            implementation,
            initializer: initializer.filter(|v| !v.is_none()),
        })
    }
}

/// Inject Bazel's implicit rule attrs that the engine adds for free
/// when `rule(test=True)` or `rule(executable=True)`. Without these,
/// macros that pass `size=`, `args=`, etc. to a `_my_rule` call hit
/// "rule has no attribute `size`" — Bazel makes them part of the rule
/// schema implicitly, even when `_my_rule(attrs={...})` doesn't list
/// them.
///
/// STUB on type fidelity: Loupe only models String, StringList, Label,
/// LabelList, Output, OutputList. `flaky`/`local` (bool) and
/// `shard_count` (int) get stored as `String` here — analysis code
/// rarely reads them, and storing as string keeps the BUILD-side
/// kwargs from rejecting at the AttrSpec layer. `env` (string_dict)
/// and `env_inherit` (list-of-string) follow the same pragmatic shape.
/// Unpack a BUILD-side label kwarg as its canonical string form,
/// accepting either a bare string (`"//pkg:tgt"`) or a constructed
/// `Label(...)` value. Returns `None` for anything else.
fn label_arg_as_str<'v>(v: Value<'v>) -> Option<String> {
    if let Some(s) = v.unpack_str() {
        return Some(s.to_string());
    }
    if let Some(lv) = v.downcast_ref::<crate::starlark_types::label::LabelValue>() {
        return Some(lv.raw.clone());
    }
    None
}

fn synthesize_implicit_attrs<V: ValueLifetimeless>(
    attrs: &mut BTreeMap<String, AttrSpecGen<V>>,
    test: bool,
    executable: bool,
) {
    fn spec<V: ValueLifetimeless>(kind: AttrKind) -> AttrSpecGen<V> {
        AttrSpecGen {
            kind,
            default: None,
            mandatory: false,
            doc: String::new(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
        }
    }
    let mut add = |k: &str, kind: AttrKind| {
        attrs.entry(k.to_string()).or_insert_with(|| spec(kind));
    };
    // https://bazel.build/reference/be/common-definitions#common-attributes
    // Universal — every rule, including non-test/non-executable.
    // `name`, `visibility`, `tags` are reserved (handled before this
    // synthesis runs); the rest are real attrs the rule's impl can
    // read via `ctx.attr.<name>`.
    add("testonly", AttrKind::Bool);
    add("compatible_with", AttrKind::LabelList);
    add("deprecation", AttrKind::String);
    add("features", AttrKind::StringList);
    add("restricted_to", AttrKind::LabelList);
    add("target_compatible_with", AttrKind::LabelList);
    add("toolchains", AttrKind::LabelList);
    add("exec_compatible_with", AttrKind::LabelList);
    add("exec_properties", AttrKind::String); // STUB: actually string_dict
    if test {
        // https://bazel.build/reference/be/common-definitions#common-attributes-tests
        add("size", AttrKind::String);
        add("timeout", AttrKind::String);
        add("flaky", AttrKind::Bool);
        add("local", AttrKind::Bool);
        add("shard_count", AttrKind::Int);
        add("env", AttrKind::String); // STUB: actually string_dict
        add("env_inherit", AttrKind::StringList);
    }
    if test || executable {
        add("args", AttrKind::StringList);
    }
}

fn unpack_attrs<'v>(v: Option<Value<'v>>) -> anyhow::Result<BTreeMap<String, AttrSpec<'v>>> {
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
        // `AttrSpec::from_value` returns `Either<&AttrSpec<'v>,
        // &FrozenAttrSpec>` (the starlark_complex_values macro hands
        // back whichever variant the heap actually holds). At rule()
        // ctor time the .bzl module is still mutable, but a value
        // loaded from another already-frozen `.bzl` may already be
        // frozen — convert either branch into a live `AttrSpec<'v>`
        // by re-promoting frozen `FrozenValue`s into `Value<'v>`.
        let either = AttrSpec::from_value(val).ok_or_else(|| {
            anyhow!(
                "rule(attrs={{`{key}`: …}}) value must be an attr.<kind>(…) call, got {}",
                val.get_type()
            )
        })?;
        let live: AttrSpec<'v> = match either {
            either::Either::Left(live) => live.clone(),
            either::Either::Right(frozen) => AttrSpecGen {
                kind: frozen.kind,
                default: frozen.default.clone(),
                mandatory: frozen.mandatory,
                doc: frozen.doc.clone(),
                executable: frozen.executable,
                providers: frozen.providers.clone(),
                cfg: match &frozen.cfg {
                    AttrCfgGen::Inherited => AttrCfgGen::Inherited,
                    AttrCfgGen::Exec => AttrCfgGen::Exec,
                    AttrCfgGen::Target => AttrCfgGen::Target,
                    AttrCfgGen::NoneCfg => AttrCfgGen::NoneCfg,
                    AttrCfgGen::Unconfigured => AttrCfgGen::Unconfigured,
                    AttrCfgGen::Transition(fv) => AttrCfgGen::Transition(fv.to_value()),
                },
            },
        };
        out.insert(key, live);
    }
    Ok(out)
}
