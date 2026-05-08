//! `coverage_common.*` — Bazel's coverage-instrumentation namespace.
//!
//! Bazel exposes `coverage_common.instrumented_files_info(ctx, ...)` as
//! a constructor for `InstrumentedFilesInfo`, the provider that carries
//! per-target source / dependency attribute hints for `bazel coverage`.
//! Loupe's "no execution" stance puts coverage out of v1 scope, but
//! `rules_shell`'s `sh_library` (pulled in by `oci_load`) returns
//! `coverage_common.instrumented_files_info(...)` in its providers
//! list — without registering the symbol the load fails with
//! `Variable 'coverage_common' not found`.
//!
//! The fix is to register `coverage_common.instrumented_files_info` as
//! a native provider type. Calling it constructs a real
//! `ProviderInstance` with `InstrumentedFilesInfo` as its identity and
//! the supplied kwargs as open fields. The leading positional `ctx`
//! arg is silently dropped — `ProviderType::invoke` only consumes
//! kwargs, which is what we want here. The result satisfies the
//! provider-list type-check in `analysis::collect_providers_from_frozen`,
//! so the impl's `return [DefaultInfo(...), instrumented_files_info]`
//! shape goes through cleanly.

use std::sync::OnceLock;

use starlark::environment::GlobalsBuilder;

use crate::starlark_types::provider::{ProviderId, ProviderType};

pub const INSTRUMENTED_FILES_INFO_SYMBOL: &str = "InstrumentedFilesInfo";

pub fn instrumented_files_info_id() -> ProviderId {
    ProviderId {
        defining_bzl: None,
        symbol: INSTRUMENTED_FILES_INFO_SYMBOL.to_string(),
    }
}

fn make_native_provider(name: &str, id: ProviderId, doc: &str) -> ProviderType {
    ProviderType {
        name: name.to_string(),
        fields: None,
        doc: doc.to_string(),
        id: {
            let lock = OnceLock::new();
            let _ = lock.set(id);
            lock
        },
    }
}

/// Register `coverage_common.instrumented_files_info` as a native
/// provider type. Called via
/// `builder.namespace("coverage_common", ...)` so the resulting
/// Starlark identifier is `coverage_common.instrumented_files_info`.
pub fn coverage_common_globals(builder: &mut GlobalsBuilder) {
    builder.set(
        "instrumented_files_info",
        make_native_provider(
            INSTRUMENTED_FILES_INFO_SYMBOL,
            instrumented_files_info_id(),
            "Bazel coverage_common.instrumented_files_info. STUB at v1: \
             open-fields constructor; the leading positional `ctx` is \
             dropped, kwargs become provider fields.",
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instrumented_files_info_id_displays_as_bare_symbol() {
        assert_eq!(
            instrumented_files_info_id().to_string(),
            "InstrumentedFilesInfo"
        );
    }
}
