//! `ctx.toolchains` — the resolved-toolchain map a rule impl reads.
//!
//! Built once per analysis in `compute_analysis`: the resolver walks
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
//! - Miss: raises (matches Bazel).
//!
//! The lookup canonicalizes the string key against the main repo's
//! package + repo mapping.

use std::collections::BTreeMap;
use std::sync::Arc;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::starlark_simple_value;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, NoSerialize, StarlarkValue, Value, ValueLike, starlark_value};

use crate::analysis::AnalyzedTarget;
use loupe_labels::{CanonicalRepo, Label, PackageId, PackagePath, RepoMapping};

/// Resolved toolchain map handed to the rule impl through `ctx`.
///
/// Cheap to clone (entries are `Arc<AnalyzedTarget>`); cloning happens
/// at most once when the impl reads `ctx.toolchains`.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<toolchains {} resolved>", by_type.len())]
pub struct Toolchains {
    /// Canonical toolchain-type label → resolved toolchain impl
    /// target. Misses raise; this map only holds successful
    /// resolutions.
    pub by_type: BTreeMap<Label, Arc<AnalyzedTarget>>,
    /// Apparent → canonical mapping needed at lookup time when the
    /// impl writes the toolchain-type as a string. The mapping's
    /// `owner` is the rule's defining `.bzl` repo so self-refs
    /// (`@//pkg:tt`) resolve from that perspective.
    pub repo_mapping: Arc<RepoMapping>,
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
        } else if let Some(lv) = index.downcast_ref::<loupe_labels::LabelValue>() {
            Label::parse_canonical(&lv.raw)
                .ok()
                .or_else(|| self.canonicalize_key(&lv.raw))
        } else {
            None
        };

        let resolved = label.as_ref().and_then(|l| self.by_type.get(l));
        let Some(target) = resolved else {
            let _ = heap;
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "ctx.toolchains[<type>]: no resolved toolchain for `{}`",
                label
                    .as_ref()
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| index.to_string()),
            )));
        };

        // The resolver guarantees a hit only when the toolchain impl
        // returned a `ToolchainInfo` provider; surface its fields
        // directly as a struct so `ctx.toolchains[T].field` reads
        // through. Look up by symbol name so a `ToolchainInfo`
        // defined natively *or* in
        // `crate::builtins::wrappers.bzl` matches identically.
        let provider = target.provider_by_symbol("ToolchainInfo").or_else(|| {
            // Fall back to whatever first provider isn't DefaultInfo —
            // some toolchain rules return their own typed provider
            // (e.g. `RegctlInfo`) instead of `ToolchainInfo`. v1
            // surfaces fields-of-first-non-DefaultInfo so ruleset
            // code that reads `ctx.toolchains[T].some_attr` still
            // resolves. STUB: real Bazel requires `ToolchainInfo`.
            target
                .providers
                .iter()
                .find(|p| p.id.symbol != "DefaultInfo")
        });
        let Some(provider) = provider else {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "ctx.toolchains[<type>]: resolved toolchain `{}` returned no providers",
                target.label,
            )));
        };
        // Re-emit a real `ProviderInstance` (not a struct) when the
        // captured provider has a known `type_ptr` — the resolved-
        // toolchain rule pattern (`return [toolchain_info, ti.default,
        // ...]`) re-puts this value into a providers list, which only
        // works if it carries a real `ProviderInstance` shape that
        // `analysis::collect_providers_from_frozen` can downcast.
        if let Some(type_ptr) = provider.type_ptr {
            use crate::starlark_types::provider::ProviderInstanceGen;
            let fields: SmallMap<String, Value<'v>> = provider
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), v.to_value()))
                .collect();
            return Ok(heap.alloc_complex(ProviderInstanceGen {
                type_ptr: type_ptr.to_value(),
                type_name: provider.id.symbol.clone(),
                fields,
                computed: std::sync::OnceLock::new(),
            }));
        }
        // Fallback: provider with no type_ptr (synthetic-source-file
        // path; toolchains never go through that path, but keep the
        // shape symmetric with `Target::at`).
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
        } else if let Some(lv) = other.downcast_ref::<loupe_labels::LabelValue>() {
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
