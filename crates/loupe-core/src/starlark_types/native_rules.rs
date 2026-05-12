//! `alias` — the one BUILD-level rule kind Loupe keeps Rust-side.
//!
//! ## DO NOT ADD MORE RULES HERE
//!
//! Bazel ships `alias` as a Java built-in; its semantics (forward
//! every provider from `actual`) don't map cleanly to a user-space
//! `rule(...)` impl because Loupe's analysis driver dereferences
//! aliases via the bespoke `synthesise_alias_analysis` path before
//! ever running a rule impl.
//!
//! Every other BUILD-level rule (`filegroup`, `genrule`,
//! `config_setting`, `platform`, `constraint_setting`,
//! `constraint_value`, `toolchain`, `toolchain_type`, `test_suite`,
//! `starlark_doc_extract`, …) lives in vendored user-space
//! `builtins_core/exports.bzl` and is injected at module-bind time
//! by `crate::builtins`. Per PLAN.md's bonanza migration (Phase 3):
//! native recorders go, user-space `rule(...)` is the source of
//! truth.
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
//! **Do not** add a new recorder function to this file. It's
//! reserved for `alias` and the small set of helpers it needs.

use std::collections::BTreeMap;

use anyhow::anyhow;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::none::NoneType;

use crate::build_eval::BuildContext;
use crate::label::Label;
use crate::package::{AttrValue, UnconfiguredTarget};

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
}
