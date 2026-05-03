//! `attr.*` namespace and the `AttrSpec` value it produces.
//!
//! Phase 3 only ships the attr kinds fixture #1 needs (`string`,
//! `string_list`). Bazel's full namespace also covers `int`, `bool`,
//! `label`, `label_list`, `string_dict`, `output`, `output_list`,
//! `license`, etc. — Phase 4 lands those alongside real type-checking
//! at rule-call time (when `Label`/`select`/`depset` become Starlark
//! values). v1 ruleset users will hit "no such attr kind" if they
//! reach for one of the unimplemented kinds; that's the trip-wire we
//! want at the Phase 3 stop gate.

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, StarlarkValue, starlark_value};

use crate::package::AttrValue;

/// Captured shape of one `attr.<kind>(...)` call.
#[derive(
    Debug, Clone, PartialEq, Eq, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display,
)]
#[display("attr.{kind:?}")]
pub struct AttrSpec {
    pub kind: AttrKind,
    pub default: Option<AttrValue>,
    pub mandatory: bool,
    pub doc: String,
}

/// Discriminant for the attr kind. Phase 4 will attach kind-specific
/// config (allow_files, providers, configurable, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Allocative)]
pub enum AttrKind {
    String,
    StringList,
}

starlark_simple_value!(AttrSpec);

#[starlark_value(type = "attr_spec")]
impl<'v> StarlarkValue<'v> for AttrSpec {}

impl AttrSpec {
    /// Default value applied when a rule is invoked without this attr.
    pub fn default_attr_value(&self) -> AttrValue {
        match &self.default {
            Some(v) => v.clone(),
            None => match self.kind {
                AttrKind::String => AttrValue::String(String::new()),
                AttrKind::StringList => AttrValue::List(Vec::new()),
            },
        }
    }
}

#[starlark_module]
pub fn attr_globals(b: &mut GlobalsBuilder) {
    fn string<'v>(
        #[starlark(require = named, default = "")] default: &str,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(default.to_string())),
            mandatory,
            doc: doc.to_string(),
        })
    }

    fn string_list<'v>(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        Ok(AttrSpec {
            kind: AttrKind::StringList,
            default: Some(AttrValue::List(Vec::new())),
            mandatory,
            doc: doc.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::values::Value;

    #[test]
    fn default_string_attr_falls_back_to_empty() {
        let s = AttrSpec {
            kind: AttrKind::String,
            default: None,
            mandatory: false,
            doc: String::new(),
        };
        assert_eq!(s.default_attr_value(), AttrValue::String(String::new()));
    }

    #[test]
    fn provided_default_round_trips() {
        let s = AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String("hi".into())),
            mandatory: false,
            doc: String::new(),
        };
        assert_eq!(s.default_attr_value(), AttrValue::String("hi".into()));
    }

    // Compile-check: macros generated `AttrSpec::from_value` on the
    // type itself (not on `&AttrSpec`). Don't lose that.
    #[test]
    fn attr_spec_unpacks_from_value() {
        fn _check<'v>(v: Value<'v>) -> Option<&'v AttrSpec> {
            AttrSpec::from_value(v)
        }
    }
}
