//! `ctx.toolchains` — the resolved-toolchain map a rule impl reads.
//!
//! Replaces the v1-WildcardStruct STUB with real lookups. Built once
//! per analysis in `compute_analysis`: the resolver walks
//! `ModuleGraph::registered_toolchains`, finds the first `toolchain`
//! target whose `toolchain_type` attr matches each type the rule
//! declared via `rule(toolchains=[...])`, force-analyzes the
//! `toolchain.toolchain` impl, and stores its `AnalyzedTarget` here
//! keyed by the canonical toolchain-type label.
//!
//! `ctx.toolchains[T]`:
//! - Hit: returns a struct of the impl's `ToolchainInfo` provider
//!   fields (no copy — `FrozenValue`s alias the dep heap, anchored on
//!   the consumer's analysis module by `run_analysis`).
//! - Miss: returns a [`WildcardStruct`] so introspection chains like
//!   `ctx.toolchains[T].some.field` don't halt analysis. Bazel raises
//!   here; v1 chooses survival to keep ruleset code that probes for
//!   optional toolchains running. Documented divergence.
//!
//! The lookup canonicalizes the string key against the main repo's
//! package + repo mapping. Same caveat as everywhere else in v1: if a
//! ruleset's `toolchains=` list is written with apparent names that
//! aren't in the main module's mapping, we fall through to
//! WildcardStruct rather than guess.

use std::collections::BTreeMap;
use std::sync::Arc;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::starlark_simple_value;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, NoSerialize, StarlarkValue, Value, ValueLike, starlark_value};

use crate::analysis::AnalyzedTarget;
use crate::label::{CanonicalRepo, Label, PackageId, PackagePath};

/// Resolved toolchain map handed to the rule impl through `ctx`.
///
/// Cheap to clone (entries are `Arc<AnalyzedTarget>`); cloning happens
/// at most once when the impl reads `ctx.toolchains`.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<toolchains {} resolved>", by_type.len())]
pub struct Toolchains {
    /// Canonical toolchain-type label → resolved toolchain impl
    /// target. Misses fall through to [`WildcardStruct`] so this map
    /// only holds successful resolutions.
    pub by_type: BTreeMap<Label, Arc<AnalyzedTarget>>,
    /// Apparent → canonical mapping needed at lookup time when the
    /// impl writes the toolchain-type as a string. Same map the
    /// engine threaded through analysis (main module's view).
    pub repo_mapping: Arc<BTreeMap<String, CanonicalRepo>>,
}

impl Toolchains {
    /// Canonicalize a string toolchain-type key as it'd be written in
    /// `rule(toolchains=[...])`: `@apparent//pkg:tt`, `//pkg:tt`,
    /// `@@canonical//pkg:tt`, or `:tt` (relative to the main repo's
    /// root package).
    fn canonicalize_key(&self, s: &str) -> Option<Label> {
        let main_pkg = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());
        Label::resolve(s, &main_pkg, &self.repo_mapping).ok()
    }
}

starlark_simple_value!(Toolchains);

#[starlark_value(type = "toolchains")]
impl<'v> StarlarkValue<'v> for Toolchains {
    /// `ctx.toolchains[<type>]` — by string label or already-canonical
    /// `LabelValue`. STUB: `ProviderType` keys aren't supported (Bazel
    /// allows passing the toolchain_type rule's instance directly; we
    /// only model string keys). Most ruleset code uses strings.
    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let label = if let Some(s) = index.unpack_str() {
            self.canonicalize_key(s)
        } else if let Some(lv) = index.downcast_ref::<crate::starlark_types::compat::LabelValue>() {
            Label::parse_canonical(&lv.raw)
                .ok()
                .or_else(|| self.canonicalize_key(&lv.raw))
        } else {
            None
        };

        let resolved = label.as_ref().and_then(|l| self.by_type.get(l));
        let Some(target) = resolved else {
            return Ok(heap.alloc(crate::starlark_types::compat::WildcardStruct));
        };

        // The resolver guarantees a hit only when the toolchain impl
        // returned a `ToolchainInfo` provider; surface its fields
        // directly as a struct so `ctx.toolchains[T].field` reads
        // through. STUB if the impl returned a `platform_common`-typed
        // provider Loupe doesn't recognise — we still hand back its
        // fields so chained reads work.
        let ti_id = crate::starlark_types::platform_common::toolchain_info_id();
        let provider = target.provider(&ti_id).or_else(|| {
            // Fall back to whatever first provider isn't DefaultInfo —
            // some toolchain rules return their own typed provider
            // (e.g. `RegctlInfo`) instead of `ToolchainInfo`. v1
            // surfaces fields-of-first-non-DefaultInfo so ruleset
            // code that reads `ctx.toolchains[T].some_attr` still
            // resolves. STUB: real Bazel requires `ToolchainInfo`.
            let default_id = crate::starlark_types::default_info::default_info_id();
            target.providers.iter().find(|p| p.id != default_id)
        });
        let Some(provider) = provider else {
            return Ok(heap.alloc(crate::starlark_types::compat::WildcardStruct));
        };
        let pairs: SmallMap<&str, Value<'v>> = provider
            .fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.to_value()))
            .collect();
        Ok(heap.alloc(AllocStruct(pairs)))
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        let label = if let Some(s) = other.unpack_str() {
            self.canonicalize_key(s)
        } else if let Some(lv) = other.downcast_ref::<crate::starlark_types::compat::LabelValue>() {
            Label::parse_canonical(&lv.raw)
                .ok()
                .or_else(|| self.canonicalize_key(&lv.raw))
        } else {
            None
        };
        Ok(label.as_ref().is_some_and(|l| self.by_type.contains_key(l)))
    }

    fn length(&self) -> starlark::Result<i32> {
        Ok(self.by_type.len() as i32)
    }
}
