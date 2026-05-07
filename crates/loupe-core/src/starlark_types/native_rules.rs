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
    /// `filegroup(name, srcs=[], data=[], visibility=[], …)`. v1
    /// records an empty target — analysis returns synthetic DefaultInfo.
    /// STUB: doesn't surface the source files as DefaultInfo.files.
    fn filegroup<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] srcs: Option<UnpackList<String>>,
        #[starlark(require = named)] data: Option<UnpackList<String>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        #[starlark(require = named)] tags: Option<Value<'v>>,
        #[starlark(require = named)] testonly: Option<bool>,
        #[starlark(require = named)] compatible_with: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (srcs, data, visibility, tags, testonly, compatible_with);
        record_native(eval, "filegroup", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `alias(name, actual, visibility=[], …)`. STUB: doesn't follow
    /// the alias at analysis time.
    fn alias<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] actual: Option<&str>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        #[starlark(require = named)] tags: Option<Value<'v>>,
        #[starlark(require = named)] testonly: Option<bool>,
        #[starlark(require = named)] compatible_with: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (visibility, tags, testonly, compatible_with);
        let mut extras = BTreeMap::new();
        if let Some(s) = actual {
            extras.insert("actual".to_string(), AttrValue::String(s.to_string()));
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
    /// v1 captures `constraint_values` for `select()` resolution; the
    /// other three (`values=`, `define_values=`, `flag_values=`) are
    /// accepted but not honoured because Loupe doesn't model
    /// `--define`, build settings, or feature flags. A select branch
    /// keyed on a `config_setting` that uses any of those silently
    /// fails to match (falls back to `//conditions:default`). STUB.
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
        // Stamp a marker on the target when any of the unmodelled
        // attrs (`values`, `define_values`, `flag_values`) is set.
        // The select() resolver checks this and skips the branch
        // (vs. matching purely on `constraint_values`) — a
        // config_setting that depends on `--define` semantics can't
        // legitimately match a v1 platform-only resolution. Surfacing
        // the flag in attrs makes the divergence introspectable from
        // tests and tooling.
        if values.is_some() || define_values.is_some() || flag_values.is_some() {
            extras.insert(
                "_loupe_unmodelled_setting".to_string(),
                AttrValue::Bool(true),
            );
        }
        record_native(eval, "config_setting", name, extras)?;
        Ok(NoneType)
    }

    /// `sh_library(name, srcs=[], deps=[], …)`. STUB.
    fn sh_library<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] srcs: Option<Value<'v>>,
        #[starlark(require = named)] deps: Option<Value<'v>>,
        #[starlark(require = named)] data: Option<Value<'v>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (srcs, deps, data, visibility);
        record_native(eval, "sh_library", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `sh_binary(name, …)`. STUB.
    fn sh_binary<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] srcs: Option<Value<'v>>,
        #[starlark(require = named)] deps: Option<Value<'v>>,
        #[starlark(require = named)] data: Option<Value<'v>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (srcs, deps, data, visibility);
        record_native(eval, "sh_binary", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `sh_test(name, …)`. STUB.
    fn sh_test<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] srcs: Option<Value<'v>>,
        #[starlark(require = named)] deps: Option<Value<'v>>,
        #[starlark(require = named)] data: Option<Value<'v>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (srcs, deps, data, visibility);
        record_native(eval, "sh_test", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `cc_library(name, …)`. STUB.
    fn cc_library<'v>(
        #[starlark(require = named)] name: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        record_native(eval, "cc_library", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `genrule(name, srcs, outs, cmd, …)`. STUB.
    fn genrule<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] srcs: Option<Value<'v>>,
        #[starlark(require = named)] outs: Option<Value<'v>>,
        #[starlark(require = named)] cmd: Option<Value<'v>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] executable: Option<bool>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (srcs, outs, cmd, tools, executable, visibility);
        record_native(eval, "genrule", name, BTreeMap::new())?;
        Ok(NoneType)
    }
}

/// Used in test contexts where the rule signatures need to live on
/// some namespace; not currently bound. Kept separate so `native.*`
/// doesn't ship them under that namespace yet.
#[allow(dead_code)]
fn _native_rules_methods(_b: &mut MethodsBuilder) {}
