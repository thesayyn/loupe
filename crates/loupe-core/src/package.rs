//! Unconfigured packages and the targets they declare.
//!
//! A `Package` is the result of evaluating a single `BUILD` (or
//! `BUILD.bazel`) file. Each rule call in that file becomes one
//! `UnconfiguredTarget`. v1 captures only attribute *shapes*; the real
//! attribute typing (Label, select, depset) lands in Phase 4.

use std::collections::BTreeMap;

use crate::label::{Label, PackageId};

/// A target as recorded during BUILD evaluation, before configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnconfiguredTarget {
    pub label: Label,
    /// The rule kind, e.g. `"hello"`. For Phase 2 this is the loaded
    /// symbol name (we don't have real `rule()` definitions yet).
    pub kind: String,
    /// The `.bzl` the rule was loaded from, or `"<native>"` if it came
    /// from the BUILD-file globals.
    pub bzl_source: String,
    /// Captured kwargs in stable iteration order.
    pub attrs: BTreeMap<String, AttrValue>,
}

/// A captured BUILD-time attribute value. Phase 2 records primitives
/// and homogeneous containers; Phase 4 will introduce `Label`,
/// `Selector`, `depset`, `glob` results, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrValue {
    None,
    String(String),
    Int(i64),
    Bool(bool),
    List(Vec<AttrValue>),
    Dict(BTreeMap<String, AttrValue>),
    /// Anything we couldn't unpack — kept as a Display string so callers
    /// at least see *something*. Phase 4 narrows this.
    Opaque(String),
}

#[derive(Debug, Clone)]
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
