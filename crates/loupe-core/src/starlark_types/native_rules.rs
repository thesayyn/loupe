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
    fn constraint_setting<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] default_constraint_value: Option<&str>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (default_constraint_value, visibility);
        record_native(eval, "constraint_setting", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `constraint_value(name, constraint_setting, …)`. STUB.
    fn constraint_value<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] constraint_setting: Option<&str>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (constraint_setting, visibility);
        record_native(eval, "constraint_value", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `platform(name, constraint_values=[], …)`. STUB.
    fn platform<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] constraint_values: Option<Value<'v>>,
        #[starlark(require = named)] parents: Option<Value<'v>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (constraint_values, parents, visibility);
        record_native(eval, "platform", name, BTreeMap::new())?;
        Ok(NoneType)
    }

    /// `toolchain(name, toolchain, toolchain_type, …)`. STUB.
    fn toolchain<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] toolchain: Option<&str>,
        #[starlark(require = named)] toolchain_type: Option<&str>,
        #[starlark(require = named)] exec_compatible_with: Option<Value<'v>>,
        #[starlark(require = named)] target_compatible_with: Option<Value<'v>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
            toolchain,
            toolchain_type,
            exec_compatible_with,
            target_compatible_with,
            visibility,
        );
        record_native(eval, "toolchain", name, BTreeMap::new())?;
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

    /// `config_setting(name, values=…, define_values=…, flag_values=…)`.
    /// STUB. v1 select() resolution doesn't reach config_setting deps.
    fn config_setting<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] values: Option<Value<'v>>,
        #[starlark(require = named)] define_values: Option<Value<'v>>,
        #[starlark(require = named)] flag_values: Option<Value<'v>>,
        #[starlark(require = named)] constraint_values: Option<Value<'v>>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
            values,
            define_values,
            flag_values,
            constraint_values,
            visibility,
        );
        record_native(eval, "config_setting", name, BTreeMap::new())?;
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
