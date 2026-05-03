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
//! Identity invariant: the inner `AnalyzedTarget` is `Arc`'d so
//! handing out a `Target` value across analyses is cheap. The
//! `Arc` ptr is the equality (a recompute produces a fresh
//! analysis result; downstream sees a different `Target`).

use std::sync::Arc;

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::starlark_simple_value;
use starlark::values::dict::AllocDict;
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
}

starlark_simple_value!(Target);

#[starlark_value(type = "Target")]
impl<'v> StarlarkValue<'v> for Target {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "label" => Some(heap.alloc(self.inner.label.to_string())),
            // STUB: `target.files` / `target.default_runfiles` /
            // `target.data_runfiles` need `DefaultInfo` plumbed, which
            // lands separately. Returning None here surfaces as a
            // Starlark `AttributeError` — preferable to silently
            // returning empty depsets.
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["label".into()]
    }

    /// `target[MyProvider]` — Bazel's provider lookup by type
    /// identity. Identity here is the `(defining_bzl, exported_symbol)`
    /// pair stamped on `ProviderType` at `.bzl` freeze time.
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
        // Build a struct-like dict so the impl can read fields off it.
        // STUB: returns a dict, not a struct or a fresh
        // ProviderInstance. For ruleset code that does
        // `target[Foo].field`, dict-style access works. A real fix
        // re-allocates a `ProviderInstance` carrying the captured
        // fields.
        let mut entries: SmallMap<&str, Value<'v>> = SmallMap::new();
        for (k, v) in &provider.fields {
            entries.insert(k.as_str(), alloc_attr(heap, v));
        }
        Ok(heap.alloc(AllocDict(entries)))
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

fn alloc_attr<'v>(heap: Heap<'v>, v: &crate::package::AttrValue) -> Value<'v> {
    use crate::package::AttrValue;
    match v {
        AttrValue::None => Value::new_none(),
        AttrValue::Bool(b) => Value::new_bool(*b),
        AttrValue::Int(i) => heap.alloc(*i),
        AttrValue::String(s) => heap.alloc(s.as_str()),
        AttrValue::List(items) => {
            let xs: Vec<Value<'v>> = items.iter().map(|x| alloc_attr(heap, x)).collect();
            heap.alloc(xs)
        }
        AttrValue::Dict(map) => {
            let mut entries: SmallMap<&str, Value<'v>> = SmallMap::new();
            for (k, v) in map {
                entries.insert(k.as_str(), alloc_attr(heap, v));
            }
            heap.alloc(AllocDict(entries))
        }
        AttrValue::Label(l) => heap.alloc(l.to_string()),
        AttrValue::LabelList(ls) => {
            let xs: Vec<Value<'v>> = ls.iter().map(|l| heap.alloc(l.to_string())).collect();
            heap.alloc(xs)
        }
        AttrValue::Opaque(s) => heap.alloc(s.as_str()),
    }
}
