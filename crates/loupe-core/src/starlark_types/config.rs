//! `config.*` namespace and the identity-only values it produces.
//!
//! Bazel exposes two unrelated families under `config`:
//!
//! - `config.none()` / `config.unconfigured()` are *attr-cfg sentinels*.
//!   Rules pass them as `attr.label(cfg = config.none())` to mark a dep
//!   as not-yet-configured / configuration-stripped.
//! - `config.bool(...)`, `config.int(...)`, `config.string(...)`,
//!   `config.string_list(...)` are *build-setting kind declarators*
//!   rules pass to `rule(build_setting = config.bool(flag = True))`.
//!
//! Neither family carries semantic state Loupe currently uses — both
//! are identity-typed sentinels the rule/attr classifier matches on by
//! type. Modelling them as plain Rust enums keeps the API surface
//! truthful (`<config.none>` / `<build_setting_kind.bool>` print) and
//! lets `attr.label`'s `cfg=` decoder downcast without inventing a
//! `WildcardStruct`-shaped escape hatch.

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, StarlarkValue, starlark_value};

/// The attr-cfg sentinel family: `config.none()`,
/// `config.unconfigured()`, and `config.exec(exec_group=…)`.
/// Identity-typed — `attr.label(cfg=…)` downcasts by enum variant.
///
/// `Exec` carries an optional exec-group name. Loupe doesn't model
/// exec groups separately yet (the group name is captured but the
/// classifier returns the same `AttrCfgKind::Exec` regardless), so
/// `config.exec()` and `config.exec(exec_group="test")` are
/// indistinguishable downstream. The field is preserved so future
/// exec-group-aware resolution doesn't need an API change.
#[derive(
    Debug, Clone, PartialEq, Eq, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display,
)]
pub enum Cfg {
    #[display("<config.none>")]
    None_,
    #[display("<config.unconfigured>")]
    Unconfigured,
    #[display("<config.exec exec_group={_0:?}>")]
    Exec(Option<String>),
}
starlark_simple_value!(Cfg);

#[starlark_value(type = "config_cfg")]
impl<'v> StarlarkValue<'v> for Cfg {}

/// `rule(build_setting=…)` kind. Identity-typed; Loupe's `rule()`
/// only checks whether the argument is non-None today.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Allocative,
    NoSerialize,
    ProvidesStaticType,
    derive_more::Display,
)]
pub enum BuildSettingKind {
    #[display("<build_setting_kind.bool>")]
    Bool,
    #[display("<build_setting_kind.int>")]
    Int,
    #[display("<build_setting_kind.string>")]
    String,
    #[display("<build_setting_kind.string_list>")]
    StringList,
    #[display("<build_setting_kind.label_list>")]
    LabelList,
}
starlark_simple_value!(BuildSettingKind);

#[starlark_value(type = "build_setting_kind")]
impl<'v> StarlarkValue<'v> for BuildSettingKind {}

/// Register the `config.*` namespace.
#[starlark_module]
pub fn config_globals(b: &mut GlobalsBuilder) {
    fn none() -> anyhow::Result<Cfg> {
        Ok(Cfg::None_)
    }
    fn unconfigured() -> anyhow::Result<Cfg> {
        Ok(Cfg::Unconfigured)
    }
    /// `config.exec(exec_group=None)` — exec-platform transition.
    /// `exec_group` selects a non-default exec group when the rule
    /// declared multiple via `rule(exec_groups=…)`. Loupe captures
    /// the name but doesn't dispatch on it yet.
    fn exec<'v>(#[starlark(require = named)] exec_group: Option<&str>) -> anyhow::Result<Cfg> {
        Ok(Cfg::Exec(exec_group.map(|s| s.to_string())))
    }
    fn bool<'v>(
        #[starlark(require = named)] flag: Option<bool>,
    ) -> anyhow::Result<BuildSettingKind> {
        let _ = flag;
        Ok(BuildSettingKind::Bool)
    }
    fn int<'v>(
        #[starlark(require = named)] flag: Option<bool>,
    ) -> anyhow::Result<BuildSettingKind> {
        let _ = flag;
        Ok(BuildSettingKind::Int)
    }
    fn string<'v>(
        #[starlark(require = named)] flag: Option<bool>,
    ) -> anyhow::Result<BuildSettingKind> {
        let _ = flag;
        Ok(BuildSettingKind::String)
    }
    fn string_list<'v>(
        #[starlark(require = named)] flag: Option<bool>,
        #[starlark(require = named)] repeatable: Option<bool>,
    ) -> anyhow::Result<BuildSettingKind> {
        let _ = (flag, repeatable);
        Ok(BuildSettingKind::StringList)
    }
    /// `config.label_list(flag=…)` — declares
    /// label-list build settings (`--extra_execution_platforms`). The
    /// vendored `bazel_tools/command_line_option/settings.bzl` uses
    /// this for `label_list_flag`.
    fn label_list<'v>(
        #[starlark(require = named)] flag: Option<bool>,
    ) -> anyhow::Result<BuildSettingKind> {
        let _ = flag;
        Ok(BuildSettingKind::LabelList)
    }
}
