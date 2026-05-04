//! `platform_common.*` — symbol stubs for Bazel's platform/toolchain
//! providers.
//!
//! Bazel exposes `platform_common.PlatformInfo` and
//! `platform_common.ToolchainInfo` as namespaced provider types that
//! rules read off targets in `platforms`/`toolchains` attrs. Loupe v1
//! doesn't model real toolchain resolution (DESIGN.md: "Toolchain
//! resolution (stub it)"), so these are **name-resolution stubs**:
//! the symbols exist as native `ProviderType`s so Starlark code that
//! references them at parse / load time doesn't crash, but rules_oci
//! reads through `ctx.toolchains[<type>]` rather than constructing
//! these directly. The synthetic toolchain registry under
//! `ctx.toolchains[]` carries the actual data shape.

use std::sync::OnceLock;

use starlark::environment::GlobalsBuilder;

use crate::starlark_types::provider::{ProviderId, ProviderType};

pub const PLATFORM_INFO_SYMBOL: &str = "PlatformInfo";
pub const TOOLCHAIN_INFO_SYMBOL: &str = "ToolchainInfo";

pub fn platform_info_id() -> ProviderId {
    ProviderId {
        defining_bzl: None,
        symbol: PLATFORM_INFO_SYMBOL.to_string(),
    }
}

pub fn toolchain_info_id() -> ProviderId {
    ProviderId {
        defining_bzl: None,
        symbol: TOOLCHAIN_INFO_SYMBOL.to_string(),
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

/// Register `platform_common.PlatformInfo` /
/// `platform_common.ToolchainInfo` as native provider types.
/// Called via `builder.namespace("platform_common", ...)` from the
/// caller, so the resulting Starlark identifier is
/// `platform_common.PlatformInfo` (and friends).
pub fn platform_common_globals(builder: &mut GlobalsBuilder) {
    builder.set(
        PLATFORM_INFO_SYMBOL,
        make_native_provider(
            PLATFORM_INFO_SYMBOL,
            platform_info_id(),
            "Bazel platform_common.PlatformInfo. STUB at v1.",
        ),
    );
    builder.set(
        TOOLCHAIN_INFO_SYMBOL,
        make_native_provider(
            TOOLCHAIN_INFO_SYMBOL,
            toolchain_info_id(),
            "Bazel platform_common.ToolchainInfo. STUB at v1.",
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_common_ids_display_as_bare_symbols() {
        assert_eq!(platform_info_id().to_string(), "PlatformInfo");
        assert_eq!(toolchain_info_id().to_string(), "ToolchainInfo");
    }
}
