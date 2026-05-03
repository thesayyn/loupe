//! `DefaultInfo` — Bazel's universal native provider.
//!
//! Every analyzed target carries a `DefaultInfo`, either constructed
//! explicitly by the rule's impl (`return [DefaultInfo(files=...)]`)
//! or synthesized as empty by the analysis driver if the impl
//! returned none. `DefaultInfo` is what `ctx.attr.<dep>` consumers
//! reach into for default outputs (`target.files`,
//! `ctx.files.<attr>`), the executable (`ctx.executable.<attr>`),
//! and runfiles (`target.default_runfiles`, `target.data_runfiles`).
//!
//! Unlike user-defined providers, `DefaultInfo` is a *native*
//! provider — its `ProviderId.defining_bzl` is `None`, and its
//! display form is the bare `"DefaultInfo"` string Bazel cquery
//! prints. The constructor is pre-bound as a global in both bzl-
//! and analysis-phase Globals so impls can write `DefaultInfo(...)`
//! anywhere without an import.
//!
//! ## Field shape (v1)
//!
//! - `files: list[File]` — the rule's default outputs. Bazel's API
//!   takes a `depset(File)`; v1 accepts either a flat list or a
//!   depset and treats them the same way. (Real depset-shaped
//!   `DefaultInfo.files` lands when a ruleset surfaces one big
//!   enough for the lazy tree to matter.)
//! - `executable: File | None` — set for executable rules
//!   (`rule(executable=True)`). Read via `ctx.executable.<attr>`.
//! - `runfiles: Runfiles | None` — Bazel collapses
//!   `default_runfiles` and `data_runfiles` into one default for
//!   most rules; v1 follows that convention with this single field.
//! - `default_runfiles` / `data_runfiles` — accepted as aliases for
//!   `runfiles` for compat with Bazel-style impls; the first one
//!   provided wins.

use std::sync::OnceLock;

use starlark::environment::GlobalsBuilder;

use crate::starlark_types::provider::{ProviderId, ProviderType};

/// Symbol name DefaultInfo is exposed under in Globals + cquery
/// output. Matches Bazel exactly.
pub const DEFAULT_INFO_SYMBOL: &str = "DefaultInfo";

/// The well-known `ProviderId` for `DefaultInfo`. Native — no
/// defining `.bzl`. Cheap to call; no allocation, no lock.
pub fn default_info_id() -> ProviderId {
    ProviderId {
        defining_bzl: None,
        symbol: DEFAULT_INFO_SYMBOL.to_string(),
    }
}

/// Field names `DefaultInfo(...)` accepts. Order is documentary
/// only — provider field iteration is BTreeMap-sorted.
pub const DEFAULT_INFO_FIELDS: &[&str] = &[
    "files",
    "executable",
    "runfiles",
    "default_runfiles",
    "data_runfiles",
];

/// Register `DefaultInfo` as a callable global. Call from both bzl-
/// and analysis-phase `register_for_*` functions so impls can
/// construct `DefaultInfo(...)` whether they're inside a `.bzl`
/// helper or directly in a rule impl.
pub fn register_default_info(builder: &mut GlobalsBuilder) {
    let id = default_info_id();
    let pt = ProviderType {
        name: DEFAULT_INFO_SYMBOL.to_string(),
        fields: Some(DEFAULT_INFO_FIELDS.iter().map(|s| s.to_string()).collect()),
        doc: "Bazel's universal default provider — files, executable, runfiles.".to_string(),
        id: {
            let lock = OnceLock::new();
            let _ = lock.set(id);
            lock
        },
    };
    builder.set(DEFAULT_INFO_SYMBOL, pt);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_info_id_displays_as_bare_symbol() {
        // Bazel cquery prints native providers without a `<bzl>%`
        // prefix — match that, since the harness compares output
        // strings.
        assert_eq!(default_info_id().to_string(), "DefaultInfo");
    }
}
