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
use starlark::starlark_complex_values;
use starlark::starlark_module;
use starlark::values::list::UnpackList;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

use crate::package::AttrValue;

/// Captured shape of one `attr.<kind>(...)` call.
///
/// Generic over `V` so the `cfg` field can carry a transition
/// function — a `Value<'v>` during `.bzl` eval, a `FrozenValue` after
/// freeze. Type aliases below give the canonical names:
///
/// - [`FrozenAttrSpec`] — frozen form (V = `FrozenValue`); the
///   schema analysis reads off `FrozenRuleClass`.
/// - [`AttrSpec`] — live form (V = `Value<'v>`); what `attr.label(...)`
///   returns into a still-mutable `.bzl` heap.
#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("attr.{:?}", kind)]
pub struct AttrSpecGen<V: ValueLifetimeless> {
    #[trace(static)]
    pub kind: AttrKind,
    #[trace(static)]
    pub default: Option<AttrValue>,
    #[trace(static)]
    pub mandatory: bool,
    #[trace(static)]
    pub doc: String,
    /// `attr.label(executable=True)` / `attr.label_list(executable=…)`
    /// — the dep must surface an executable `File` (Bazel enforces at
    /// rule-call time; v1 enforces lazily on `ctx.executable.<attr>`).
    #[trace(static)]
    pub executable: bool,
    /// `attr.label(providers=[…])` — typed-dep contract. Captured but
    /// not enforced in v1; the door is open for a future phase.
    #[trace(static)]
    pub providers: Vec<String>,
    /// `attr.label(cfg = …)` — classified into one of the [`AttrCfg`]
    /// variants. [`AttrCfg::Transition`] carries the transition value
    /// (a function or a `_TransitionInfo` provider instance) so
    /// analysis can fire it when resolving the attr's deps.
    pub cfg: AttrCfgGen<V>,
}

/// Live form — what `attr.label(...)` returns into a `.bzl` module
/// before freeze. Matches starlark-rust's `starlark_complex_values!`
/// naming convention: the unprefixed name is the live form.
pub type AttrSpec<'v> = AttrSpecGen<Value<'v>>;
/// Frozen form — what `FrozenRuleClass.attrs` holds and what analysis
/// reads.
pub type FrozenAttrSpec = AttrSpecGen<FrozenValue>;

starlark_complex_values!(AttrSpec);

impl<'v> Freeze for AttrSpec<'v> {
    type Frozen = FrozenAttrSpec;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(AttrSpecGen {
            kind: self.kind,
            default: self.default,
            mandatory: self.mandatory,
            doc: self.doc,
            executable: self.executable,
            providers: self.providers,
            cfg: self.cfg.freeze(freezer)?,
        })
    }
}

impl<V: ValueLifetimeless> Clone for AttrSpecGen<V> {
    fn clone(&self) -> Self {
        AttrSpecGen {
            kind: self.kind,
            default: self.default.clone(),
            mandatory: self.mandatory,
            doc: self.doc.clone(),
            executable: self.executable,
            providers: self.providers.clone(),
            cfg: self.cfg.clone(),
        }
    }
}

/// Classifier for `attr.label(cfg = …)`.
///
/// String forms `"target"` / `"exec"` / `"host"` (legacy alias for
/// exec) and the `config.none()` / `config.unconfigured()` sentinels
/// each map to a variant. Anything else (a function value, a
/// `_TransitionInfo` provider instance, etc.) classifies as
/// [`AttrCfgGen::Transition`] and the transition value is preserved
/// so analysis can fire it.
#[derive(Debug, Trace, Allocative)]
pub enum AttrCfgGen<V: ValueLifetimeless> {
    /// No explicit `cfg=` argument.
    Inherited,
    /// `cfg="exec"` (or legacy `cfg="host"`).
    Exec,
    /// `cfg="target"` — explicit no-op classifier matching Bazel.
    Target,
    /// `cfg=config.none()` — Bazel's identity-typed "no configuration".
    NoneCfg,
    /// `cfg=config.unconfigured()` — drop build-settings overlay.
    Unconfigured,
    /// User transition. Carries the transition value the user passed —
    /// either a plain function (older-style) or a `_TransitionInfo`
    /// provider instance. Analysis-time code unpacks
    /// the value to extract `implementation`, `inputs`, `outputs`.
    Transition(V),
}

/// Live `AttrCfg` — what's constructed inside a `.bzl` module.
pub type AttrCfg<'v> = AttrCfgGen<Value<'v>>;
/// Frozen `AttrCfg` — what's stored on `FrozenAttrSpec`.
pub type FrozenAttrCfg = AttrCfgGen<FrozenValue>;

impl<V: ValueLifetimeless> Clone for AttrCfgGen<V> {
    fn clone(&self) -> Self {
        match self {
            AttrCfgGen::Inherited => AttrCfgGen::Inherited,
            AttrCfgGen::Exec => AttrCfgGen::Exec,
            AttrCfgGen::Target => AttrCfgGen::Target,
            AttrCfgGen::NoneCfg => AttrCfgGen::NoneCfg,
            AttrCfgGen::Unconfigured => AttrCfgGen::Unconfigured,
            AttrCfgGen::Transition(v) => AttrCfgGen::Transition(*v),
        }
    }
}

impl<V: ValueLifetimeless> Default for AttrCfgGen<V> {
    fn default() -> Self {
        AttrCfgGen::Inherited
    }
}

impl<'v> Freeze for AttrCfg<'v> {
    type Frozen = FrozenAttrCfg;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(match self {
            AttrCfgGen::Inherited => AttrCfgGen::Inherited,
            AttrCfgGen::Exec => AttrCfgGen::Exec,
            AttrCfgGen::Target => AttrCfgGen::Target,
            AttrCfgGen::NoneCfg => AttrCfgGen::NoneCfg,
            AttrCfgGen::Unconfigured => AttrCfgGen::Unconfigured,
            AttrCfgGen::Transition(v) => AttrCfgGen::Transition(v.freeze(freezer)?),
        })
    }
}

impl<V: ValueLifetimeless> AttrCfgGen<V> {
    /// True iff the dep should surface as `[Target]` (list-of-one)
    /// rather than a bare `Target`. Matches Bazel's split-transition
    /// shape: `cfg=transition(...)` yields a list, the string forms
    /// stay scalar.
    pub fn surfaces_as_list(&self) -> bool {
        matches!(self, AttrCfgGen::Transition(_))
    }

    /// Discriminant-only view: returns the `()` variant ignoring the
    /// `Transition` payload. Useful for matching when the payload V
    /// isn't reachable in the caller's lifetime.
    pub fn kind(&self) -> AttrCfgKind {
        match self {
            AttrCfgGen::Inherited => AttrCfgKind::Inherited,
            AttrCfgGen::Exec => AttrCfgKind::Exec,
            AttrCfgGen::Target => AttrCfgKind::Target,
            AttrCfgGen::NoneCfg => AttrCfgKind::NoneCfg,
            AttrCfgGen::Unconfigured => AttrCfgKind::Unconfigured,
            AttrCfgGen::Transition(_) => AttrCfgKind::Transition,
        }
    }
}

/// Payload-free view of [`AttrCfgGen`]. Used by code paths that only
/// need the variant tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrCfgKind {
    Inherited,
    Exec,
    Target,
    NoneCfg,
    Unconfigured,
    Transition,
}

/// Discriminant for the attr kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Allocative)]
pub enum AttrKind {
    String,
    StringList,
    /// `attr.string_dict(...)` — string→string dict. Stored as
    /// `AttrValue::Dict(<String>)`; surfaces through `ctx.attr.<name>`
    /// as a Starlark dict so impls can `.items()` / `[k]` it.
    StringDict,
    /// `attr.string_list_dict(...)` — string→[string] dict.
    StringListDict,
    /// `attr.int(...)` — declared int. Stored as `AttrValue::Int`;
    /// surfaces as a Starlark int through `ctx.attr.<name>`.
    Int,
    /// `attr.bool(...)` — declared bool. Stored as `AttrValue::Bool`;
    /// surfaces as a Starlark bool.
    Bool,
    Label,
    LabelList,
    /// `attr.output(...)` — an implicit output declared on the rule.
    /// The BUILD-side value (when supplied) is a label string naming a
    /// file to declare in the target's package. v1 stores it as
    /// `AttrValue::String` (empty when unsupplied) and surfaces it
    /// through `ctx.outputs.<name>` as a `File` (or `None`).
    Output,
    /// `attr.output_list(...)` — list-of-output sibling of [`Output`].
    /// Stored as `AttrValue::List(Vec<String>)`; surfaces as a list of
    /// `File`s through `ctx.outputs.<name>`.
    OutputList,
}

impl AttrKind {
    /// Whether this kind carries label-typed values that analysis
    /// must recursively force `AnalysisKey` for.
    pub fn is_label_typed(&self) -> bool {
        matches!(self, AttrKind::Label | AttrKind::LabelList)
    }

    /// Whether this kind declares implicit outputs that surface
    /// through `ctx.outputs.<name>`.
    pub fn is_output_typed(&self) -> bool {
        matches!(self, AttrKind::Output | AttrKind::OutputList)
    }
}

#[starlark_value(type = "attr_spec")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for AttrSpecGen<V> where Self: ProvidesStaticType<'v> {}

impl<V: ValueLifetimeless> AttrSpecGen<V> {
    /// Default value applied when a rule is invoked without this attr.
    pub fn default_attr_value(&self) -> AttrValue {
        match &self.default {
            Some(v) => v.clone(),
            None => match self.kind {
                AttrKind::String | AttrKind::Output => AttrValue::String(String::new()),
                AttrKind::StringList | AttrKind::OutputList => AttrValue::List(Vec::new()),
                AttrKind::StringDict | AttrKind::StringListDict => {
                    AttrValue::Dict(std::collections::BTreeMap::new())
                }
                AttrKind::Int => AttrValue::Int(0),
                AttrKind::Bool => AttrValue::Bool(false),
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
    ) -> anyhow::Result<AttrSpec<'v>> {
        let _ = values;
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(default.to_string())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
        })
    }

    fn string_list<'v>(
        #[starlark(require = named)] default: Option<UnpackList<String>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = false)] allow_empty: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec<'v>> {
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
            cfg: AttrCfgGen::Inherited,
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
    ) -> anyhow::Result<AttrSpec<'v>> {
        let _ = default;
        Ok(AttrSpec {
            kind: AttrKind::StringDict,
            default: Some(AttrValue::Dict(std::collections::BTreeMap::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
        })
    }

    /// `attr.string_list_dict(default={}, mandatory=False, doc="")`.
    fn string_list_dict<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec<'v>> {
        let _ = default;
        Ok(AttrSpec {
            kind: AttrKind::StringListDict,
            default: Some(AttrValue::Dict(std::collections::BTreeMap::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
        })
    }

    /// `attr.label_keyed_string_dict(...)`. STUB.
    #[allow(clippy::too_many_arguments)]
    fn label_keyed_string_dict<'v>(
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
        #[starlark(require = named)] providers: Option<Value<'v>>,
        #[starlark(require = named)] allow_files: Option<Value<'v>>,
        #[starlark(require = named)] allow_empty: Option<bool>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named)] aspects: Option<Value<'v>>,
        #[starlark(require = named)] flags: Option<Value<'v>>,
    ) -> anyhow::Result<AttrSpec<'v>> {
        let _ = (
            default,
            providers,
            allow_files,
            allow_empty,
            cfg,
            aspects,
            flags,
        );
        Ok(AttrSpec {
            kind: AttrKind::String,
            default: Some(AttrValue::String(String::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
        })
    }

    /// `attr.bool(default=False, mandatory=False, doc="")`.
    fn bool<'v>(
        #[starlark(require = named, default = false)] default: bool,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec<'v>> {
        Ok(AttrSpec {
            kind: AttrKind::Bool,
            default: Some(AttrValue::Bool(default)),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
        })
    }

    /// `attr.int(default=0, mandatory=False, doc="", values=[])`.
    fn int<'v>(
        #[starlark(require = named, default = 0i32)] default: i32,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
        #[starlark(require = named)] values: Option<UnpackList<i32>>,
    ) -> anyhow::Result<AttrSpec<'v>> {
        let _ = values;
        Ok(AttrSpec {
            kind: AttrKind::Int,
            default: Some(AttrValue::Int(default as i64)),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
        })
    }

    /// `attr.int_list(default=[], mandatory=False, doc="")`. STUB.
    fn int_list<'v>(
        #[starlark(require = named)] default: Option<UnpackList<i32>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec<'v>> {
        let _ = default;
        Ok(AttrSpec {
            kind: AttrKind::StringList,
            default: Some(AttrValue::List(Vec::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
        })
    }

    /// `attr.output(...)`. Records the attr as an implicit output;
    /// the BUILD-side value (a label string) is captured as
    /// `AttrValue::String` and surfaces through `ctx.outputs.<name>`
    /// as a `File` (or `None` when the user didn't supply one).
    fn output<'v>(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec<'v>> {
        Ok(AttrSpec {
            kind: AttrKind::Output,
            default: Some(AttrValue::String(String::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
        })
    }

    /// `attr.output_list(...)`. List-of-output sibling of `output`.
    fn output_list<'v>(
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec<'v>> {
        Ok(AttrSpec {
            kind: AttrKind::OutputList,
            default: Some(AttrValue::List(Vec::new())),
            mandatory,
            doc: doc.to_string(),
            executable: false,
            providers: Vec::new(),
            cfg: AttrCfgGen::Inherited,
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
        #[starlark(require = named)] default: Option<Value<'v>>,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named)] providers: Option<UnpackList<Value<'v>>>,
        #[starlark(require = named)] allow_files: Option<Value<'v>>,
        #[starlark(require = named)] allow_single_file: Option<Value<'v>>,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named)] aspects: Option<Value<'v>>,
        #[starlark(require = named)] flags: Option<Value<'v>>,
        #[starlark(require = named)] for_dependency_resolution: Option<bool>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec<'v>> {
        // STUB: `allow_files` / `allow_single_file` accept either a
        // bool or a list of extensions; v1 doesn't enforce file-vs-rule
        // dep typing, so we don't decode the shape.
        // `aspects`/`flags`/`for_dependency_resolution` accepted
        // and ignored at v1.
        let attr_cfg = classify_cfg_arg(cfg)?;
        let _ = (
            allow_files,
            allow_single_file,
            aspects,
            flags,
            for_dependency_resolution,
        );
        // `default` accepts either a label string or a `Label(...)`
        // value — Bazel rules use both forms. Both unpack to the
        // canonical string representation for storage.
        let default_str = default.and_then(|v| {
            v.unpack_str().map(|s| s.to_string()).or_else(|| {
                v.downcast_ref::<crate::starlark_types::label::LabelValue>()
                    .map(|lv| lv.raw.clone())
            })
        });
        Ok(AttrSpec {
            kind: AttrKind::Label,
            default: default_str.map(AttrValue::String),
            mandatory,
            doc: doc.to_string(),
            executable,
            providers: collect_provider_ids(providers),
            cfg: attr_cfg,
        })
    }

    /// `attr.label_list(default=[], executable=False, providers=[],
    /// allow_files=…, allow_rules=…, mandatory=False, doc="")`.
    /// `allow_rules` is the rule-kind allowlist Bazel enforces at
    /// dep-binding time; v1 captures the spec but doesn't enforce
    /// kind-typing on label-typed attrs (DESIGN.md "what not to
    /// implement").
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn label_list<'v>(
        #[starlark(require = named)] default: Option<UnpackList<Value<'v>>>,
        #[starlark(require = named, default = false)] executable: bool,
        #[starlark(require = named)] providers: Option<UnpackList<Value<'v>>>,
        #[starlark(require = named)] allow_files: Option<Value<'v>>,
        #[starlark(require = named)] allow_rules: Option<UnpackList<String>>,
        #[starlark(require = named, default = true)] allow_empty: bool,
        #[starlark(require = named)] cfg: Option<Value<'v>>,
        #[starlark(require = named)] aspects: Option<Value<'v>>,
        #[starlark(require = named)] flags: Option<Value<'v>>,
        #[starlark(require = named, default = false)] mandatory: bool,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<AttrSpec<'v>> {
        let _ = (allow_files, allow_rules, allow_empty, aspects, flags);
        let attr_cfg = classify_cfg_arg(cfg)?;
        // Each `default` entry accepts either a label string or a
        // `Label(...)` value — Bazel rules use both forms. Unpack to
        // the canonical string representation for storage.
        let default_attr = default.map(|l| {
            AttrValue::List(
                l.items
                    .into_iter()
                    .map(|v| {
                        if let Some(s) = v.unpack_str() {
                            AttrValue::String(s.to_string())
                        } else if let Some(lv) =
                            v.downcast_ref::<crate::starlark_types::label::LabelValue>()
                        {
                            AttrValue::String(lv.raw.clone())
                        } else {
                            AttrValue::String(v.to_string())
                        }
                    })
                    .collect(),
            )
        });
        Ok(AttrSpec {
            kind: AttrKind::LabelList,
            default: default_attr,
            mandatory,
            doc: doc.to_string(),
            executable,
            providers: collect_provider_ids(providers),
            cfg: attr_cfg,
        })
    }
}

/// Capture the provider list as `<bzl>%<symbol>` strings. v1 only
/// supports the flat-list form (`[A, B]` — must provide A AND B);
/// the nested form (`[[A, B], [C]]` — any-of-all-of) lands when a
/// ruleset surfaces a need.
///
/// STUB: providers are captured for documentation only — no
/// enforcement at rule-call or analysis time. Accepts either
/// `ProviderType<'v>` (defined in the same `.bzl`) or
/// `FrozenProviderType` (loaded from another `.bzl`).
/// Classify `attr.label(cfg = …)` into an [`AttrCfg`] variant.
///
/// Accepts: `None`, `"target"`/`"exec"`/`"host"` strings, the
/// `config.none()`/`config.unconfigured()` sentinels (downcast to
/// [`crate::starlark_types::config::Cfg`]), or anything else (function
/// value, `_TransitionInfo` provider instance, …) — the catch-all is
/// [`AttrCfg::Transition`].
fn classify_cfg_arg<'v>(cfg: Option<Value<'v>>) -> anyhow::Result<AttrCfg<'v>> {
    use crate::starlark_types::config::Cfg;
    let Some(v) = cfg else {
        return Ok(AttrCfgGen::Inherited);
    };
    if let Some(s) = v.unpack_str() {
        return Ok(match s {
            "target" => AttrCfgGen::Target,
            // "host" is the pre-Bazel-6 alias for "exec"; vendored
            // rulesets still ship it. Treat the two identically.
            "exec" | "host" => AttrCfgGen::Exec,
            other => {
                return Err(anyhow::anyhow!(
                    "attr.label(cfg = {other:?}) — unknown string form; \
                     expected \"target\", \"exec\", or \"host\""
                ));
            }
        });
    }
    if let Some(c) = v.downcast_ref::<Cfg>() {
        return Ok(match c {
            Cfg::None_ => AttrCfgGen::NoneCfg,
            Cfg::Unconfigured => AttrCfgGen::Unconfigured,
            // `config.exec(exec_group=…)` — exec transition. Loupe
            // doesn't distinguish exec from target yet
            // (`derive_dep_config` collapses them), so the group name
            // is dropped on the floor here.
            Cfg::Exec(_) => AttrCfgGen::Exec,
        });
    }
    Ok(AttrCfgGen::Transition(v))
}

fn collect_provider_ids(providers: Option<UnpackList<Value<'_>>>) -> Vec<String> {
    use crate::starlark_types::provider::{FrozenProviderType, ProviderType};
    providers
        .map(|l| {
            l.items
                .into_iter()
                .map(|v| {
                    if let Some(pt) = v.downcast_ref::<ProviderType>() {
                        pt.id.get().map(|id| id.to_string()).unwrap_or_default()
                    } else if let Some(pt) = v.downcast_ref::<FrozenProviderType>() {
                        pt.id.get().map(|id| id.to_string()).unwrap_or_default()
                    } else {
                        String::new()
                    }
                })
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
            cfg: AttrCfgGen::Inherited,
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
            cfg: AttrCfgGen::Inherited,
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
            cfg: AttrCfgGen::Inherited,
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

    // Compile-check: `starlark_complex_values!` generated
    // `AttrSpec::from_value` on the type itself, returning the
    // `Either<&AttrSpec<'v>, &FrozenAttrSpec>` shape callers
    // dispatch on. Don't lose that.
    #[test]
    fn attr_spec_unpacks_from_value() {
        fn _check<'v>(
            v: Value<'v>,
        ) -> Option<either::Either<&'v AttrSpec<'v>, &'v FrozenAttrSpec>> {
            AttrSpec::from_value(v)
        }
    }
}
