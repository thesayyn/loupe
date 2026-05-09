//! Native Bazel rule stubs (filegroup, alias, platform, etc.).
//!
//! These are rules Bazel ships as Java-side built-ins; they don't have
//! a Starlark implementation we can load. v1 records each call as an
//! `UnconfiguredTarget` with no `bzl_label` so the analysis driver
//! routes them through the source-file early-return path — every
//! native target ends up with an empty `DefaultInfo` and no recorded
//! actions. Right for `bazel cquery //:foo` shape on these rules,
//! wrong if anything reads `DefaultInfo.files` (we'd miss source files
//! a real `filegroup` exposes).

use std::collections::BTreeMap;

use anyhow::anyhow;
use starlark::environment::{GlobalsBuilder, MethodsBuilder};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;
use starlark::values::list::UnpackList;
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
#[allow(clippy::too_many_arguments)] // matches Bazel's surface
pub fn native_rules_globals(builder: &mut GlobalsBuilder) {
    // `filegroup` is a user-space rule in
    // `bzl/builtins_core/exports.bzl`; the wrappers.bzl re-export
    // injects it as a global so `filegroup(name=..., srcs=[...])`
    // from BUILD resolves there.

    /// `alias(name, actual, **common_attrs)`. `actual` is captured
    /// as a canonical `Label` so the analysis driver can forward all
    /// providers from the aliased target. Universal common attrs
    /// (`visibility`, `tags`, `testonly`, `deprecation`, …) are
    /// accepted via the variadic surface and silently absorbed —
    /// generated rule classes (rules_nodejs's toolchains repo) routinely
    /// pass `deprecation=…` on `alias`.
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

    /// `constraint_setting(name, default_constraint_value=None, …)`. STUB.
    /// `constraint_setting(name, default_constraint_value=None, …)`.
    /// Captures `default_constraint_value` so future versions can
    /// honour the "missing-from-platform falls back to default"
    /// rule. STUB at v1: captured but not consulted. The toolchain
    /// resolver and select() resolution still match purely on the
    /// platform's explicit `constraint_values`.
    fn constraint_setting<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] default_constraint_value: Option<&str>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = visibility;
        let mut extras = BTreeMap::new();
        if let Some(s) = default_constraint_value {
            extras.insert(
                "default_constraint_value".to_string(),
                AttrValue::String(s.to_string()),
            );
        }
        record_native(eval, "constraint_setting", name, extras)?;
        Ok(NoneType)
    }

    /// `constraint_value(name, constraint_setting, …)`. Captures
    /// `constraint_setting` so `select()` resolution can recognise
    /// constraint_value branches.
    fn constraint_value<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] constraint_setting: Option<&str>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = visibility;
        let mut extras = BTreeMap::new();
        if let Some(s) = constraint_setting {
            extras.insert(
                "constraint_setting".to_string(),
                AttrValue::String(s.to_string()),
            );
        }
        record_native(eval, "constraint_value", name, extras)?;
        Ok(NoneType)
    }

    /// `platform(name, constraint_values=[], parents=[], …)`. v1
    /// captures `constraint_values` and `parents` as label strings;
    /// the toolchain resolver walks the parent chain at lookup time
    /// to union the effective constraint set.
    fn platform<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] constraint_values: Option<UnpackList<String>>,
        #[starlark(require = named)] parents: Option<UnpackList<String>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = visibility;
        let mut extras = BTreeMap::new();
        if let Some(xs) = constraint_values {
            extras.insert(
                "constraint_values".to_string(),
                AttrValue::List(xs.items.into_iter().map(AttrValue::String).collect()),
            );
        }
        if let Some(xs) = parents {
            extras.insert(
                "parents".to_string(),
                AttrValue::List(xs.items.into_iter().map(AttrValue::String).collect()),
            );
        }
        record_native(eval, "platform", name, extras)?;
        Ok(NoneType)
    }

    /// `toolchain(name, toolchain, toolchain_type, …)`. v1 captures
    /// the four resolution-relevant attrs (`toolchain`, `toolchain_type`,
    /// `exec_compatible_with`, `target_compatible_with`) as raw label
    /// strings on the recorded target. The toolchain resolver (PR B)
    /// canonicalises and matches them; v1 doesn't enforce
    /// `*_compatible_with` — it captures only.
    fn toolchain<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] toolchain: Option<&str>,
        #[starlark(require = named)] toolchain_type: Option<&str>,
        #[starlark(require = named)] exec_compatible_with: Option<UnpackList<String>>,
        #[starlark(require = named)] target_compatible_with: Option<UnpackList<String>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = visibility;
        let mut extras = BTreeMap::new();
        if let Some(s) = toolchain {
            extras.insert("toolchain".to_string(), AttrValue::String(s.to_string()));
        }
        if let Some(s) = toolchain_type {
            extras.insert(
                "toolchain_type".to_string(),
                AttrValue::String(s.to_string()),
            );
        }
        if let Some(xs) = exec_compatible_with {
            extras.insert(
                "exec_compatible_with".to_string(),
                AttrValue::List(xs.items.into_iter().map(AttrValue::String).collect()),
            );
        }
        if let Some(xs) = target_compatible_with {
            extras.insert(
                "target_compatible_with".to_string(),
                AttrValue::List(xs.items.into_iter().map(AttrValue::String).collect()),
            );
        }
        record_native(eval, "toolchain", name, extras)?;
        Ok(NoneType)
    }

    /// `toolchain_type(name, …)`. STUB.
    fn toolchain_type<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = visibility;
        record_native(eval, "toolchain_type", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `config_setting(name, values=…, define_values=…, flag_values=…, constraint_values=[…])`.
    /// v1 captures `values=` (matched against
    /// [`Configuration::build_settings`]), `flag_values=` (matched
    /// against the referenced build_setting target's
    /// `build_setting_default`), and `constraint_values=` (matched
    /// against the configured target platform). `define_values=` is
    /// captured as a STUB — Loupe doesn't model `--define KEY=VALUE`
    /// pairs separately from the engine-exported build-settings map.
    fn config_setting<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] values: Option<Value<'v>>,
        #[starlark(require = named)] define_values: Option<Value<'v>>,
        #[starlark(require = named)] flag_values: Option<Value<'v>>,
        #[starlark(require = named)] constraint_values: Option<UnpackList<String>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = visibility;
        let mut extras = BTreeMap::new();
        if let Some(xs) = constraint_values {
            extras.insert(
                "constraint_values".to_string(),
                AttrValue::List(xs.items.into_iter().map(AttrValue::String).collect()),
            );
        }
        // `values = {"compilation_mode": "opt", ...}` — capture into a
        // String→String dict the resolver matches against
        // `Configuration::build_settings`.
        if let Some(v) = values
            && let Some(dict) = starlark::values::dict::DictRef::from_value(v)
        {
            let mut entries = BTreeMap::new();
            for (k, val) in dict.iter() {
                if let (Some(k), Some(val)) = (k.unpack_str(), val.unpack_str()) {
                    entries.insert(k.to_string(), AttrValue::String(val.to_string()));
                }
            }
            extras.insert("values".to_string(), AttrValue::Dict(entries));
        }
        // `flag_values = {"//path:flag": "value", ...}` — capture
        // verbatim. Resolution analyzes each flag target and compares
        // its `build_setting_default` to the requested value.
        if let Some(v) = flag_values
            && let Some(dict) = starlark::values::dict::DictRef::from_value(v)
        {
            let mut entries = BTreeMap::new();
            for (k, val) in dict.iter() {
                if let (Some(k), Some(val)) = (k.unpack_str(), val.unpack_str()) {
                    entries.insert(k.to_string(), AttrValue::String(val.to_string()));
                }
            }
            extras.insert("flag_values".to_string(), AttrValue::Dict(entries));
        }
        // `define_values` — STUB. Captured as a marker so the resolver
        // can skip the branch rather than matching on a partial view.
        // A v2 that splits `--define` semantics out of build_settings
        // would handle these.
        if define_values.is_some() {
            extras.insert(
                "_loupe_unmodelled_setting".to_string(),
                AttrValue::Bool(true),
            );
        }
        record_native(eval, "config_setting", name, extras)?;
        Ok(NoneType)
    }

    // `sh_library` / `sh_binary` / `sh_test` / `cc_library` /
    // `genrule` / `test_suite` / `filegroup` are user-space `rule(...)`
    // calls in `bzl/builtins_core/exports.bzl`. Their invocations
    // carry `bzl_label = Some(@_builtins//builtins_core/exports.bzl)`
    // and reach the `(Some, Some)` arm in `compute_analysis`.
}

/// Used in test contexts where the rule signatures need to live on
/// some namespace; not currently bound. Kept separate so `native.*`
/// doesn't ship them under that namespace yet.
#[allow(dead_code)]
fn _native_rules_methods(_b: &mut MethodsBuilder) {}
