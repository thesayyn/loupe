//! `attr.*` namespace and the `AttrSpec` value it produces.
//!
//! v1 surface: `string`, `string_list`, `label`, `label_list`. Bazel's
//! full surface (`int`, `bool`, `string_dict`, `output`, `output_list`,
//! `license`, …) lands when a ruleset reaches for one — until then,
//! a "no such attr kind" trip-wire surfaces the gap.
//!
//! `attr.label` / `attr.label_list` carry the dep-resolution kwargs
//! that matter for analysis: `executable` (Bazel: must point at an
//! executable target) and `providers` (typed-dep contract). v1
//! captures both but only enforces `executable` lazily when
//! `ctx.executable.<attr>` is read.

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::list::UnpackList;
use starlark::values::{NoSerialize, StarlarkValue, Value, starlark_value};

use crate::package::AttrValue;
use crate::starlark_types::provider::ProviderType;

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
    /// `attr.label(executable=True)` / `attr.label_list(executable=…)`
    /// — the dep must surface an executable `File` (Bazel enforces at
    /// rule-call time; v1 enforces lazily on `ctx.executable.<attr>`).
    pub executable: bool,
    /// `attr.label(providers=[…])` — typed-dep contract. Captured but
    /// not enforced in v1; the door is open for a future phase.
    pub providers: Vec<String>,
}

/// Discriminant for the attr kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Allocative)]
pub enum AttrKind {
    String,
    StringList,
    Label,
    LabelList,
}

impl AttrKind {
    /// Whether this kind carries label-typed values that analysis
    /// must recursively force `AnalysisKey` for.
    pub fn is_label_typed(&self) -> bool {
        matches!(self, AttrKind::Label | AttrKind::LabelList)
    }
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
                AttrKind::Label => AttrValue::None,
                AttrKind::LabelList => AttrValue::LabelList(Vec::new()),
            },
        }
    }
}

#[starlark_module]
#[allow(clippy::too_many_arguments)] // Bazel surface; matches upstream
pub fn attr_globals(b: &mut GlobalsBuilder) {
    fn string<'v>(
        #[starlark(require = named, default = "")] default: &str,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
        // STUB: `values=[...]` is the enumerated-allowed-values
        // constraint Bazel enforces at attr-binding time. v1 accepts
        // it for compat with rules_oci's `format = attr.string(values=
        // ["docker","oci"])` and similar; we don't enforce.
        #[starlark(require = named)] values: Option<UnpackList<String>>,
    ) -> anyhow::Result<AttrSpec> {
        let _ = values;
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(default.to_string())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    fn string_list<'v>(
        #[starlark(require = named)] default: Option<UnpackList<String>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] allow_empty: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        let _ = allow_empty;
        let default_list = default
            .map(|l| AttrValue::List(l.items.into_iter().map(AttrValue::String).collect()))
            .unwrap_or_else(|| AttrValue::List(Vec::new()));
        Ok(AttrSpec {
            kind: AttrKind::StringList,
            default: Some(default_list),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    /// `attr.string_dict(default={}, mandatory=False, doc="")` —
    /// string-keyed dict of strings. STUB: stored as a String-kind
    /// AttrSpec for v1; the repository rules that actually exercise
    /// this never read the attr from a configured target (they use it
    /// at fetch time, which Loupe doesn't model).
    fn string_dict<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        let _ = default;
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(String::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    /// `attr.string_list_dict(default={}, mandatory=False, doc="")`.
    /// STUB: same shape as `string_dict`.
    fn string_list_dict<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        let _ = default;
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(String::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    /// `attr.label_keyed_string_dict(...)`. STUB.
    fn label_keyed_string_dict<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        let _ = default;
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(String::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    /// `attr.bool(default=False, mandatory=False, doc="")`.
    /// STUB-ish: stored as a String-kind AttrSpec because v1 doesn't
    /// model bool-typed attrs end-to-end. Sufficient for repo-rule
    /// definitions that don't get analyzed.
    fn bool<'v>(
        #[starlark(require = named, default = false)] default: bool,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        let _ = default;
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(String::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    /// `attr.int(default=0, mandatory=False, doc="", values=[])`. STUB.
    fn int<'v>(
        #[starlark(require = named, default = 0i32)] default: i32,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
        #[starlark(require = named)] values: Option<UnpackList<i32>>,
    ) -> anyhow::Result<AttrSpec> {
        let _ = (default, values);
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(String::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    /// `attr.int_list(default=[], mandatory=False, doc="")`. STUB.
    fn int_list<'v>(
        #[starlark(require = named)] default: Option<UnpackList<i32>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        let _ = default;
        Ok(AttrSpec {
            kind: AttrKind::StringList,
            default: Some(AttrValue::List(Vec::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    /// `attr.output(...)`. STUB: stored as String. Output attrs
    /// declare implicit outputs at rule-definition time; v1 records
    /// the spec but doesn't synthesise the outputs.
    fn output<'v>(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(String::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    /// `attr.output_list(...)`. STUB.
    fn output_list<'v>(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        Ok(AttrSpec {
            kind: AttrKind::StringList,
            default: Some(AttrValue::List(Vec::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
        })
    }

    /// `attr.label(default=None, executable=False, providers=[],
    /// allow_files=…, allow_single_file=…, mandatory=False, doc="")`.
    /// v1 captures `default` (as a string label — analysis resolves
    /// it through the package's repo_mapping at first read),
    /// `executable`, `providers`, and `mandatory`. The `allow_*`
    /// kwargs are accepted but ignored — v1 doesn't enforce file-
    /// vs-rule dep typing.
    fn label<'v>(
        #[starlark(require = named)] default: Option<&str>,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named)] providers: Option<UnpackList<&'v ProviderType>>,
        #[starlark(require = named)] allow_files: Option<Value<'v>>,
        #[starlark(require = named)] allow_single_file: Option<Value<'v>>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named)] aspects: Option<Value<'v>>,
        #[starlark(require = named)] flags: Option<Value<'v>>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        // STUB: `allow_files` / `allow_single_file` accept either a
        // bool or a list of extensions; v1 doesn't enforce file-vs-rule
        // dep typing, so we don't decode the shape.
        // `cfg`/`aspects`/`flags`/`for_dependency_resolution` accepted
        // and ignored at v1.
        let _ = (
            allow_files,
            allow_single_file,
            cfg,
            aspects,
            flags,
            for_dependency_resolution,
        );
        Ok(AttrSpec {
            kind: AttrKind::Label,
            default: default.map(|s| AttrValue::String(s.to_string())),
            mandatory,
            doc: doc.to_string(),
            executable,
            providers: collect_provider_ids(providers),
        })
    }

    /// `attr.label_list(default=[], executable=False, providers=[],
    /// allow_files=…, mandatory=False, doc="")`.
    fn label_list<'v>(
        #[starlark(require = named)] default: Option<UnpackList<String>>,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named)] providers: Option<UnpackList<&'v ProviderType>>,
        #[starlark(require = named)] allow_files: Option<Value<'v>>,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named)] aspects: Option<Value<'v>>,
        #[starlark(require = named)] flags: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec> {
        let _ = (allow_files, allow_empty, cfg, aspects, flags);
        let default_attr =
            default.map(|l| AttrValue::List(l.items.into_iter().map(AttrValue::String).collect()));
        Ok(AttrSpec {
            kind: AttrKind::LabelList,
            default: default_attr,
            mandatory,
            doc: doc.to_string(),
            executable,
            providers: collect_provider_ids(providers),
        })
    }
}

/// Capture the provider list as `<bzl>%<symbol>` strings. v1 only
/// supports the flat-list form (`[A, B]` — must provide A AND B);
/// the nested form (`[[A, B], [C]]` — any-of-all-of) lands when a
/// ruleset surfaces a need.
///
/// STUB: providers are captured for documentation only — no
/// enforcement at rule-call or analysis time.
fn collect_provider_ids(providers: Option<UnpackList<&ProviderType>>) -> Vec<String> {
    providers
        .map(|l| {
            l.items
                .into_iter()
                .map(|pt| pt.id.get().map(|id| id.to_string()).unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_string_attr_falls_back_to_empty() {
        let s = AttrSpec {
            kind: AttrKind::String,
            default: None,
            mandatory: false,
            doc: String::new(),
            executable: false,
            providers: Vec::new(),
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
            executable: false,
            providers: Vec::new(),
        };
        assert_eq!(s.default_attr_value(), AttrValue::String("hi".into()));
    }

    #[test]
    fn label_list_default_is_empty_label_list() {
        // No `default=` → empty `LabelList`. The kind matters: a
        // future analysis pass distinguishes "ran out of label
        // attrs" from "list-of-strings attr".
        let s = AttrSpec {
            kind: AttrKind::LabelList,
            default: None,
            mandatory: false,
            doc: String::new(),
            executable: false,
            providers: Vec::new(),
        };
        assert_eq!(s.default_attr_value(), AttrValue::LabelList(Vec::new()));
    }

    #[test]
    fn label_typed_kinds_self_identify() {
        assert!(AttrKind::Label.is_label_typed());
        assert!(AttrKind::LabelList.is_label_typed());
        assert!(!AttrKind::String.is_label_typed());
        assert!(!AttrKind::StringList.is_label_typed());
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
