//! `OutputGroupInfo` — Bazel's named output groups.
//!
//! A native provider whose fields are arbitrary user-chosen group
//! names. Each field value is a `depset(File)` (or v1's flat-list
//! shortcut). Returned from rule impls that want to expose extra
//! output sets beyond `DefaultInfo.files`. `target.output_groups`
//! reads it; `bazel build //x --output_groups=foo` builds a specific
//! one.
//!
//! v1 surfaces it because rules_oci's `oci_load` returns one. The
//! field shape isn't enforced — any kwargs go through.

use std::sync::OnceLock;

use starlark::environment::GlobalsBuilder;

use crate::starlark_types::provider::{ProviderId, ProviderType};

pub const OUTPUT_GROUP_INFO_SYMBOL: &str = "OutputGroupInfo";

pub fn output_group_info_id() -> ProviderId {
    ProviderId {
        defining_bzl: None,
        symbol: OUTPUT_GROUP_INFO_SYMBOL.to_string(),
    }
}

/// Register `OutputGroupInfo(...)` as a callable global. Open-fields
/// — any kwargs become group names.
pub fn register_output_group_info(builder: &mut GlobalsBuilder) {
    let pt = ProviderType {
        name: OUTPUT_GROUP_INFO_SYMBOL.to_string(),
        fields: None, // open — group names are user-chosen
        doc: "Named output groups for a target.".to_string(),
        id: {
            let lock = OnceLock::new();
            let _ = lock.set(output_group_info_id());
            lock
        },
    };
    builder.set(OUTPUT_GROUP_INFO_SYMBOL, pt);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_group_info_id_displays_as_bare_symbol() {
        assert_eq!(output_group_info_id().to_string(), "OutputGroupInfo");
    }
}
