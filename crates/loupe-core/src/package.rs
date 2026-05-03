//! Unconfigured packages and the targets they declare.
//!
//! A `Package` is the result of evaluating a single `BUILD` (or
//! `BUILD.bazel`) file. Each rule call in that file becomes one
//! `UnconfiguredTarget`. v1 captures only attribute *shapes*; the real
//! attribute typing (Label, select, depset) lands in Phase 4.

use std::collections::BTreeMap;

use crate::label::{Label, PackageId};

/// A target as recorded during BUILD evaluation, before configuration.
#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct UnconfiguredTarget {
    pub label: Label,
    /// The rule kind, e.g. `"hello"` — the BUILD-side import alias (the
    /// `local` in `load(":defs.bzl", "hello")`).
    pub kind: String,
    /// The `.bzl` the rule was loaded from as it appeared in the BUILD's
    /// `load()` (e.g. `":defs.bzl"`), or `"<native>"` for built-in rules
    /// like the synthetic `source_file` from `exports_files`.
    pub bzl_source: String,
    /// Canonical Label of the `.bzl` that defined the rule. `None` for
    /// native-only kinds. Phase 5 analysis uses this to force the
    /// matching `BzlLoadKey` and recover the `RuleClass`.
    pub bzl_label: Option<Label>,
    /// Source-side symbol name in `bzl_label` (the `source` in
    /// `load(":defs.bzl", "hello_alias" = "hello")`). `None` for native.
    pub source_symbol: Option<String>,
    /// Captured kwargs in stable iteration order.
    pub attrs: BTreeMap<String, AttrValue>,
}

/// A captured BUILD-time attribute value. Phase 2 records primitives
/// and homogeneous containers; Phase 5+ adds `Label` / `LabelList` so
/// analysis can recover dep edges without re-parsing strings.
#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub enum AttrValue {
    None,
    String(String),
    Int(i64),
    Bool(bool),
    List(Vec<AttrValue>),
    Dict(BTreeMap<String, AttrValue>),
    /// Resolved label dep — analysis forces `AnalysisKey` for it.
    Label(Label),
    /// Resolved list of label deps.
    LabelList(Vec<Label>),
    /// Anything we couldn't unpack — kept as a Display string so callers
    /// at least see *something*. Phase 4 narrows this.
    Opaque(String),
}

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct Package {
    pub id: PackageId,
    pub targets: BTreeMap<String, UnconfiguredTarget>,
}

impl Package {
    pub fn new(id: PackageId) -> Self {
        Package {
            id,
            targets: BTreeMap::new(),
        }
    }

    /// Targets in stable (sorted-by-name) order.
    pub fn labels(&self) -> Vec<Label> {
        self.targets.values().map(|t| t.label.clone()).collect()
    }
}
