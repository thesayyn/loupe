//! `Target` — Starlark view of a configured target's analysis result.
//!
//! What rule impls receive when they read `ctx.attr.<dep>`. Each
//! `Target` wraps an `Arc<AnalyzedTarget>` and exposes:
//!
//! - `target.label` — the configured label as a string.
//! - `target[ProviderType]` — the captured provider with that
//!   `ProviderId`. Errors if the target doesn't carry it.
//! - `provider_type in target` — membership test, same identity rule.
//!
//! Bazel also surfaces `target.files`, `target.default_runfiles`,
//! `target.data_runfiles`, and per-provider attribute access on
//! the implicit `DefaultInfo`. Those land alongside `DefaultInfo`
//! itself — separate piece of work.
//!
//! ## Provider field access — no double allocation
//!
//! `target[Provider]` returns a struct whose fields are the original
//! `FrozenValue`s the impl produced — no copy. The dep's
//! `analysis_heap` is anchored on the consumer's analysis module
//! (in `run_analysis`), so the FrozenValue references stay valid
//! as long as the consumer's heap exists. The struct wrapper itself
//! is allocated once per `target[Provider]` lookup, but its
//! contents are aliased.

use std::sync::Arc;

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::starlark_simple_value;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, NoSerialize, StarlarkValue, Value, ValueLike, starlark_value};

use crate::analysis::AnalyzedTarget;
use crate::starlark_types::provider::ProviderType;

/// What `ctx.attr.<dep>` returns for a label-typed attr (or one
/// element of a label_list).
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<target {}>", inner.label)]
pub struct Target {
    pub inner: Arc<AnalyzedTarget>,
}

impl Target {
    pub fn new(inner: Arc<AnalyzedTarget>) -> Self {
        Target { inner }
    }

    /// Read a field off the target's `DefaultInfo` provider, lifting
    /// the stored `FrozenValue` back into the consumer's heap. Returns
    /// `None`-as-Value when the impl didn't set that field — Bazel's
    /// behavior for `target.files` on a target with no
    /// `DefaultInfo.files=`.
    fn default_info_field<'v>(&self, _heap: Heap<'v>, name: &str) -> Value<'v> {
        let p = self.inner.default_info();
        match p.fields.get(name) {
            Some(v) => v.to_value(),
            None => Value::new_none(),
        }
    }
}

starlark_simple_value!(Target);

#[starlark_value(type = "Target")]
impl<'v> StarlarkValue<'v> for Target {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "label" => Some(heap.alloc(self.inner.label.to_string())),
            // `target.files` reads `DefaultInfo.files` directly.
            // STUB: returns a flat list, not a `depset(File)`. The
            // depset shape lands when v1 surfaces real depset
            // construction in `DefaultInfo.files=`.
            "files" => Some(self.default_info_field(heap, "files")),
            // `target.default_runfiles` / `target.data_runfiles` —
            // both fall back to `DefaultInfo.runfiles` if the impl
            // didn't separate them. v1 collapses them onto one
            // `runfiles` field per the doc note in default_info.rs.
            "default_runfiles" | "data_runfiles" => Some(self.default_info_field(heap, "runfiles")),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["label", "files", "default_runfiles", "data_runfiles"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// `target[MyProvider]` — Bazel's provider lookup by type
    /// identity. Identity is the `(defining_bzl, exported_symbol)`
    /// pair stamped on `ProviderType` at `.bzl` freeze time.
    ///
    /// The returned struct's fields are the original FrozenValues the
    /// impl produced — no copying. That works because
    /// `run_analysis` anchors the dep's `analysis_heap` on the
    /// consumer's module (see `analysis::run_analysis`), keeping the
    /// FrozenValue refs valid for as long as the consumer reads them.
    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let pt = index.downcast_ref::<ProviderType>().ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "target[<key>]: key must be a provider type, got {}",
                index.get_type()
            ))
        })?;
        let want = pt.id.get().ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "internal: ProviderType is missing its ProviderId stamp"
            ))
        })?;
        let provider = self.inner.provider(want).ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "target {} does not provide {}",
                self.inner.label,
                want
            ))
        })?;
        // STUB: returns a struct, not a fresh ProviderInstance.
        // Ruleset code typically does `target[Foo].field`, which
        // works on either; if a ruleset reflects on `type(...)` of
        // the result we'll need to switch to a real ProviderInstance.
        let pairs: SmallMap<&str, Value<'v>> = provider
            .fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.to_value()))
            .collect();
        Ok(heap.alloc(AllocStruct(pairs)))
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        // `MyProvider in target` — same identity rule as `target[…]`.
        let id = match other
            .downcast_ref::<ProviderType>()
            .and_then(|pt| pt.id.get())
        {
            Some(id) => id,
            None => return Ok(false),
        };
        Ok(self.inner.provider(id).is_some())
    }
}
