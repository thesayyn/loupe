//! `alias`, `label_flag`, `label_setting` — the BUILD-level rule
//! kinds Loupe keeps Rust-side because they forward providers from a
//! target instead of running a rule impl.
//!
//! ## Why these three are native
//!
//! Bazel ships them as Java built-ins. Their semantics (forward every
//! provider from `actual` / `build_setting_default`) don't map cleanly
//! to a user-space `rule(...)` impl: Loupe's analysis driver
//! dereferences them via [`crate::dice_keys::synthesise_alias_analysis`]
//! before ever running a rule impl.
//!
//! `label_flag` and `label_setting` track Bazel's
//! [label-typed build settings](https://bazel.build/extending/config#label-typed-build-settings)
//! — they are *not* `rule(build_setting=…)` instances and do not
//! return `BuildSettingInfo`; they're a kind of alias whose
//! `build_setting_default` may be reconfigured by transitions.
//!
//! ## DO NOT ADD OTHER RULES HERE
//!
//! Every other BUILD-level rule (`filegroup`, `genrule`,
//! `config_setting`, `platform`, `constraint_setting`,
//! `constraint_value`, `toolchain`, `toolchain_type`, `test_suite`,
//! `starlark_doc_extract`, `bool_flag`/`string_flag`/…) lives in
//! vendored user-space `.bzl` files and is injected at module-bind
//! time by `crate::builtins`. Native recorders are reserved for the
//! handful that genuinely need engine semantics (alias-style
//! forwarding); everything else lives in user-space `rule(...)`.
//!
//! If a new BUILD-level rule needs to be supported:
//!
//! 1. Add (or reach for) its `rule(...)` definition in
//!    `crates/loupe-core/src/starlark/builtins_core/exports.bzl`.
//! 2. Add the symbol to the `exported_rules` allowlist in
//!    [`crate::builtins::inject_builtins_into`].
//! 3. If the vendored impl is a stub (`fail("TODO: implement")`),
//!    route the kind through a `synthesise_*_analysis` path in
//!    `crate::dice_keys::compute_analysis` (the kind-based dispatch
//!    fires before the rule impl runs).
//!
//! **Do not** add a new recorder function to this file unless it has
//! genuine alias-forwarding semantics that prevent a user-space impl.

use std::collections::BTreeMap;

use anyhow::anyhow;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::none::NoneType;

use crate::build_eval::BuildContext;
use crate::package::{AttrValue, UnconfiguredTarget};
use loupe_labels::{Label, LabelValue};

fn ctx_from_eval<'a, 'v>(eval: &'a Evaluator<'v, '_, '_>) -> anyhow::Result<&'a BuildContext> {
    eval.extra
        .ok_or_else(|| anyhow!("internal: native rule called outside a BuildContext"))?
        .downcast_ref::<BuildContext>()
        .ok_or_else(|| anyhow!("internal: extra is not a BuildContext"))
}

fn record_native<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    kind: &str,
    name: Option<&str>,
    extras: BTreeMap<String, AttrValue>,
) -> anyhow::Result<()> {
    let ctx = ctx_from_eval(eval)?;
    let name = name.ok_or_else(|| anyhow!("`{kind}` requires `name = ...`"))?;
    let label = Label::new(ctx.package().repo.clone(), ctx.package().path.clone(), name);
    let mut attrs = extras;
    attrs.insert("name".to_string(), AttrValue::String(name.to_string()));
    ctx.record_target(UnconfiguredTarget {
        label,
        kind: kind.to_string(),
        bzl_source: format!("native.{kind}"),
        bzl_label: None,
        source_symbol: None,
        attrs,
    })
}

#[starlark_module]
pub fn native_rules_globals(builder: &mut GlobalsBuilder) {
    /// `alias(name, actual, **common_attrs)`. `actual` is captured
    /// as a canonical `Label` so the analysis driver can forward all
    /// providers from the aliased target. Universal common attrs
    /// (`visibility`, `tags`, `testonly`, `deprecation`, …) are
    /// accepted via the variadic surface and silently absorbed —
    /// generated rule classes (rules_nodejs's toolchains repo) routinely
    /// pass `deprecation=…` on `alias`.
    ///
    /// Kept Rust-side because `alias` doesn't have a natural
    /// `rule(...)` impl: forwarding every provider from the aliased
    /// target requires walking analysis output, which sits below the
    /// rule-impl layer. The synchronous synthesis path
    /// (`synthesise_alias_analysis`) reads this UnconfiguredTarget's
    /// `actual` attr to find the dep, then relabels the dep's
    /// analyzed providers under the alias's own Label.
    fn alias<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let named = args.names_map().map_err(|e| anyhow!("alias: {e}"))?;
        let name = named.get("name").and_then(|v| v.unpack_str());
        let actual = named.get("actual").and_then(|v| v.unpack_str());
        let ctx = ctx_from_eval(eval)?;
        let mut extras = BTreeMap::new();
        if let Some(s) = actual
            && let Ok(label) = Label::resolve(s, ctx.package(), ctx.repo_mapping())
        {
            extras.insert("actual".to_string(), AttrValue::Label(label));
        }
        record_native(eval, "alias", name, extras)?;
        Ok(NoneType)
    }

    /// `label_flag(name, build_setting_default, **common_attrs)`.
    /// Mirrors Bazel's built-in. CLI / transitions may rebind the
    /// default to a different label; analysis follows whichever target
    /// the binding currently points at and forwards its providers.
    /// Loupe doesn't model CLI overrides yet — analysis always walks
    /// `build_setting_default` as written.
    fn label_flag<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        record_label_setting(eval, args, "label_flag")
    }

    /// `label_setting(name, build_setting_default, **common_attrs)`.
    /// Same shape as `label_flag` but not user-configurable from the
    /// CLI. Loupe treats them identically for analysis — both forward
    /// providers from `build_setting_default`.
    fn label_setting<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        record_label_setting(eval, args, "label_setting")
    }

    /// `native.package_name()` — slash-separated path of the package
    /// the calling macro is being invoked from. Reads
    /// [`BuildContext::package`]; only callable inside BUILD eval
    /// (`.bzl` top-level access errors with the standard
    /// "internal: native rule called outside a BuildContext").
    fn package_name<'v>(eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<String> {
        let ctx = ctx_from_eval(eval)?;
        Ok(ctx.package().path.as_str().to_string())
    }

    /// `native.repository_name()` — canonical repo name of the
    /// calling macro's package, prefixed with `@@` (Bazel's bzlmod
    /// canonical form). `@@` for the main repo.
    fn repository_name<'v>(eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<String> {
        let ctx = ctx_from_eval(eval)?;
        Ok(format!("@@{}", ctx.package().repo.as_str()))
    }

    /// `native.repo_name()` — Bazel 7+ replacement for
    /// `native.repository_name()`. Returns the canonical repo name
    /// without the leading `@@` (so the main repo returns the empty
    /// string). rules_js's `js_run_binary.bzl` calls this to build
    /// `RUNFILES_DIR` paths; without it the rule fails with
    /// "struct has no attribute repo_name" because Bazel exposes
    /// `repo_name` on `native` as a callable.
    fn repo_name<'v>(eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<String> {
        let ctx = ctx_from_eval(eval)?;
        Ok(ctx.package().repo.as_str().to_string())
    }

    /// `native.package_relative_label(input)` — resolve a label string
    /// (or pass a `Label` through) to a canonical `Label`, relative to
    /// the package being evaluated. bazel-skylib's `selects.bzl` keys a
    /// dict on it; rules_cc's `cc_shared_library` stringifies it.
    /// `None` passes through (Bazel allows it for optional attrs).
    fn package_relative_label<'v>(
        #[starlark(require = pos)] input: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        if input.is_none() {
            return Ok(Value::new_none());
        }
        let ctx = ctx_from_eval(eval)?;
        let raw = input
            .unpack_str()
            .map(|s| s.to_string())
            .or_else(|| input.downcast_ref::<LabelValue>().map(|lv| lv.raw.clone()))
            .ok_or_else(|| {
                anyhow!(
                    "package_relative_label: expected a string or Label, got {}",
                    input.get_type()
                )
            })?;
        let label = Label::resolve(&raw, ctx.package(), ctx.repo_mapping())
            .map_err(|e| anyhow!("package_relative_label: resolving `{raw}`: {e}"))?;
        Ok(eval.heap().alloc(LabelValue::parse(&label.to_string())))
    }

    /// `package(default_visibility = …, default_testonly = …, …)` —
    /// Bazel's BUILD-level metadata declarator. Loupe doesn't model
    /// any of its kwargs; accepted-and-ignored so vendored BUILD
    /// files evaluate.
    fn package<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `licenses([...])` — legacy Bazel licensing declarator.
    /// Accepted-and-ignored.
    fn licenses<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `genquery(name, expression, scope, …)` — Bazel native rule that
    /// runs a query at build time and writes the result. rules_js's
    /// root BUILD declares one. v1 doesn't execute queries; record an
    /// UnconfiguredTarget so the label resolves and the package loads.
    fn genquery<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let named = args.names_map().map_err(|e| anyhow!("genquery: {e}"))?;
        let name = named.get("name").and_then(|v| v.unpack_str());
        record_native(eval, "genquery", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `package_group(name, packages = [], includes = [])` — Bazel
    /// native that names a set of packages for visibility grouping.
    /// It is a built-in (not a `rule(...)`), so it can't live in
    /// user-space `.bzl`; exports.bzl already documents it as one of
    /// the natives meant to reach BUILD files from Rust. Loupe doesn't
    /// enforce visibility, so record an UnconfiguredTarget (so the
    /// label resolves and the package loads) — `@protobuf//upb`'s BUILD
    /// declares several, and without this its load fails with
    /// "Variable `package_group` not found".
    fn package_group<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let named = args
            .names_map()
            .map_err(|e| anyhow!("package_group: {e}"))?;
        let name = named.get("name").and_then(|v| v.unpack_str());
        record_native(eval, "package_group", name, BTreeMap::new())?;
        Ok(NoneType)
    }
}

/// Shared body for `label_flag` and `label_setting` — both record a
/// target whose `build_setting_default` is resolved against the
/// caller's repo mapping. `singleton_list` is the `platforms`-specific
/// quirk (vendored BUILD's `label_flag(..., singleton_list = True)`);
/// captured for future use, ignored by analysis today.
fn record_label_setting<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
    args: &starlark::eval::Arguments<'v, '_>,
    kind: &str,
) -> anyhow::Result<NoneType> {
    let named = args.names_map().map_err(|e| anyhow!("{kind}: {e}"))?;
    let name = named.get("name").and_then(|v| v.unpack_str());
    let default = named
        .get("build_setting_default")
        .and_then(|v| v.unpack_str());
    let ctx = ctx_from_eval(eval)?;
    let mut extras = BTreeMap::new();
    if let Some(s) = default {
        match Label::resolve(s, ctx.package(), ctx.repo_mapping()) {
            Ok(label) => {
                extras.insert("build_setting_default".to_string(), AttrValue::Label(label));
            }
            Err(_) => {
                extras.insert(
                    "build_setting_default".to_string(),
                    AttrValue::String(s.to_string()),
                );
            }
        }
    }
    if let Some(v) = named.get("singleton_list")
        && let Some(b) = v.unpack_bool()
    {
        extras.insert("singleton_list".to_string(), AttrValue::Bool(b));
    }
    record_native(eval, kind, name, extras)?;
    Ok(NoneType)
}
