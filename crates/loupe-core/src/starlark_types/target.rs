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
//! Bazel's full magic-attr set on a `Target` is `actions`,
//! `data_runfiles`, `default_runfiles`, `files`, `files_to_run`,
//! `label`, `output_groups`. v1 surfaces all seven; some are real
//! (`label`, `files`, `actions`), some pull off `DefaultInfo`
//! (`default_runfiles`, `data_runfiles`, `files_to_run.executable`),
//! and `output_groups` is a STUB until ruleset code surfaces a need.
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
use starlark::values::{
    FrozenValue, Heap, NoSerialize, StarlarkValue, Value, ValueLike, starlark_value,
};

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
            // `target.actions` — Bazel's read-only list of recorded
            // actions. Each access allocates fresh `Action` clones into
            // the consumer's heap; mirrors how `target.files` clones
            // `File`s through.
            "actions" => {
                let xs: Vec<Value<'v>> = self
                    .inner
                    .actions
                    .iter()
                    .map(|a| heap.alloc(a.clone()))
                    .collect();
                Some(heap.alloc(xs))
            }
            // `target.files_to_run` — Bazel's `FilesToRunProvider`
            // shim with `executable`, `runfiles_manifest`,
            // `repo_mapping_manifest`. v1 reads them off DefaultInfo
            // and stubs `runfiles_manifest`/`repo_mapping_manifest` to
            // None — rulesets that build wrappers usually only touch
            // `.executable`.
            // STUB: not a real `FilesToRunProvider`; just a struct.
            "files_to_run" => {
                let exec = self.default_info_field(heap, "executable");
                let pairs: SmallMap<&str, Value<'v>> = [
                    ("executable", exec),
                    ("runfiles_manifest", Value::new_none()),
                    ("repo_mapping_manifest", Value::new_none()),
                ]
                .into_iter()
                .collect();
                Some(heap.alloc(AllocStruct(pairs)))
            }
            // `target.output_groups` — Bazel exposes these via the
            // `OutputGroupInfo` provider. If the impl returned one,
            // surface its fields as a struct; otherwise empty struct.
            "output_groups" => {
                if let Some(p) = self.inner.provider_by_symbol("OutputGroupInfo") {
                    let pairs: SmallMap<&str, Value<'v>> = p
                        .fields
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.to_value()))
                        .collect();
                    Some(heap.alloc(AllocStruct(pairs)))
                } else {
                    Some(heap.alloc(AllocStruct::EMPTY))
                }
            }
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        [
            "label",
            "files",
            "default_runfiles",
            "data_runfiles",
            "actions",
            "files_to_run",
            "output_groups",
        ]
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
        // STUB: when the target doesn't carry a provider, Bazel
        // raises an error. v1 returns a `WildcardStruct` instead so
        // ruleset code that introspects platform/toolchain providers
        // (e.g. rules_oci checking
        // `target[platform_common.ConstraintValueInfo].label`) doesn't
        // halt the analysis on synthetic-DefaultInfo-only targets.
        // Real provider resolution would force a recursive analysis
        // and either find the provider or error.
        let provider = match self.inner.provider(want) {
            Some(p) => Some(p),
            None => {
                // Fall through to symbol-based lookup for the universal
                // Bazel providers (`DefaultInfo`, `OutputGroupInfo`,
                // `InstrumentedFilesInfo`). The wrappers.bzl-defined
                // ProviderType has `defining_bzl =
                // Some(@_builtins//common/providers.bzl)`, but the
                // synthetic DefaultInfo emitted by
                // `synthesise_source_file_analysis` /
                // `synthesise_alias_analysis` / Loupe's auto-empty
                // fallback in `run_analysis` carries
                // `defining_bzl: None`. Strict ID match misses; symbol
                // match recognises both as the same provider — same
                // fall-through `provider_by_symbol` does for the rest
                // of Loupe's internals.
                match want.symbol.as_str() {
                    "DefaultInfo" | "OutputGroupInfo" | "InstrumentedFilesInfo" => {
                        self.inner.provider_by_symbol(&want.symbol)
                    }
                    _ => None,
                }
            }
        };
        let provider = match provider {
            Some(p) => p,
            None => {
                // Native constraint/platform/toolchain rules
                // (`constraint_value(...)`, `platform(...)`,
                // `toolchain_type(...)`) live in `native_rules.rs` as
                // record-only stubs — the toolchain resolver reads
                // their attrs straight off the UnconfiguredTarget,
                // and they never go through a real `rule(...)` impl
                // that returns a provider. When user code reaches for
                // the matching provider, Bazel-style code patterns
                // typically just feed the result into another stub
                // (e.g. `target_platform_has_constraint(...)`), so
                // surface an empty struct rather than raising.
                match want.symbol.as_str() {
                    "ConstraintValueInfo"
                    | "ConstraintSettingInfo"
                    | "PlatformInfo"
                    | "ToolchainInfo"
                    | "TemplateVariableInfo" => {
                        let empty: Vec<(&str, Value<'v>)> = Vec::new();
                        return Ok(heap.alloc(starlark::values::structs::AllocStruct(empty)));
                    }
                    _ => {}
                }
                return Err(starlark::Error::new_other(anyhow!(
                    "target {} doesn't have provider {}",
                    self.inner.label,
                    want.symbol,
                )));
            }
        };
        // Build a real `ProviderInstance` (not a struct) so rules
        // that re-emit the result in their own providers list — like
        // `aspect_bazel_lib`'s `_resolved_toolchain_impl` returning
        // `[toolchain_info, toolchain_info.default, ...]` — survive
        // the `analysis::collect_providers_from_frozen` type check.
        // Prefer the captured `type_ptr` (the impl's original
        // ProviderType identity) so identity-based provider lookups
        // downstream still work; fall back to the lookup key.
        let type_ptr: FrozenValue = provider.type_ptr.unwrap_or_else(|| {
            index
                .unpack_frozen()
                .expect("target[<key>] index is always frozen at this point")
        });
        let mut fields: SmallMap<String, Value<'v>> = provider
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), v.to_value()))
            .collect();
        if want.symbol == "DefaultInfo" {
            // Bazel synthesises `files_to_run` on every `DefaultInfo`:
            // a struct with `executable`, `runfiles_manifest`, and
            // `repo_mapping_manifest`. Derived field — never user-
            // supplied. v1 builds it on demand from the user-supplied
            // `executable` (or `None`); the manifest fields stay
            // `None` since v1 doesn't execute. Matches the falsy
            // fallback path in `aspect_bazel_lib`'s `mtree_spec`.
            let executable = fields
                .get("executable")
                .copied()
                .unwrap_or_else(Value::new_none);
            let ftr_pairs: [(&str, Value<'v>); 3] = [
                ("executable", executable),
                ("runfiles_manifest", Value::new_none()),
                ("repo_mapping_manifest", Value::new_none()),
            ];
            let ftr = heap.alloc(AllocStruct(ftr_pairs.into_iter()));
            fields.insert("files_to_run".to_string(), ftr);

            // Bazel exposes three runfiles fields on every
            // DefaultInfo: `runfiles` (the canonical name in newer
            // rules), `default_runfiles`, and `data_runfiles`. v1
            // accepts any of them as constructor input (the first
            // wins) — see `default_info.rs`. Surface all three on
            // read by aliasing whichever was supplied; default to
            // an empty Runfiles when none was. Without this, rules
            // that read `default_runfiles.files` (rules_oci's `tar`
            // does) error on a `default_runfiles`-not-set DefaultInfo
            // built by a ruleset that wrote `runfiles=...`.
            let runfiles_alias = fields
                .get("runfiles")
                .or_else(|| fields.get("default_runfiles"))
                .or_else(|| fields.get("data_runfiles"))
                .copied()
                .unwrap_or_else(|| heap.alloc(crate::starlark_types::runfiles::Runfiles::empty()));
            for k in ["runfiles", "default_runfiles", "data_runfiles"] {
                fields.entry(k.to_string()).or_insert(runfiles_alias);
            }
        }
        use crate::starlark_types::provider::ProviderInstanceGen;
        Ok(heap.alloc_complex(ProviderInstanceGen {
            type_ptr,
            type_name: pt.name.clone(),
            fields,
        }))
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
