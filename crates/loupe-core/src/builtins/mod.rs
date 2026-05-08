//! Bazel `builtins.bzl`-style compat surface, in user-space Starlark.
//!
//! Bazel ships a private ["builtins
//! zip"](https://github.com/bazelbuild/bazel/blob/master/src/main/starlark/builtins_bzl/README.md)
//! whose `.bzl` files define the rule constructors, provider types,
//! and helper namespaces (`coverage_common`, `platform_common`, …)
//! that user `.bzl`s reference as if they were native globals. Loupe
//! mirrors that pattern: keep the bottom primitives (`provider`,
//! `attr.*`, `rule`, `ctx.actions.*`, `File`, `depset`) in the host
//! (Rust), and layer Bazel-compatibility behaviour on top in
//! [`wrappers.bzl`]. New compat surface lands as a `.bzl` change, not
//! a Rust change.
//!
//! ## Lifecycle
//!
//! 1. [`embedded_builtins`] parses + evaluates [`wrappers.bzl`]
//!    against the *base* `bzl_globals` once (cached for the process
//!    lifetime). The result is a [`FrozenModule`] whose top-level
//!    bindings are the names we want every user `.bzl` / BUILD to
//!    see — `coverage_common`, `InstrumentedFilesInfo`, etc.
//!
//! 2. [`inject_builtins_into`] is called from each user-`.bzl` and
//!    BUILD evaluator before user code runs. It anchors the cached
//!    builtins frozen heap on the user module's frozen heap (so the
//!    `FrozenValue`s remain valid through the user module's freeze)
//!    and copies each top-level name into the user module via
//!    [`Module::set`]. The user code then sees those names as if
//!    they were native globals.
//!
//! ## Constraints on `wrappers.bzl`
//!
//! - May only reference names available in the base `bzl_globals`
//!   (no auto-injected names — that would create an evaluation-order
//!   cycle, since wrappers.bzl is itself the source of those).
//! - Symbols defined here become part of every `.bzl`'s frozen
//!   identity. Two `.bzl`s referencing `coverage_common` see the
//!   *same* `FrozenValue`, which is the correct shape for provider
//!   identity.

use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
/// Embedded source of [`wrappers.bzl`]. Compiled into the binary so
/// no on-disk file is required to get the builtins running.
const WRAPPERS_BZL: &str = include_str!("wrappers.bzl");

/// Filename used for diagnostics emitted while evaluating
/// [`WRAPPERS_BZL`]. Keeping it unique (no path) makes Starlark error
/// messages unambiguous about where the failing line lives.
const WRAPPERS_BZL_FILENAME: &str = "@@<loupe-builtins>//:wrappers.bzl";

/// Lazily-initialised cache for the frozen `wrappers.bzl` module.
/// `Arc` is used so the cached frozen heap can be referenced from
/// many user `.bzl` modules without copying.
static CACHED: OnceLock<std::sync::Arc<FrozenModule>> = OnceLock::new();

/// Build the globals `wrappers.bzl` is evaluated against. This is
/// the same surface user `.bzl` files see (`provider`, `attr.*`,
/// `struct`, etc.) — wrappers.bzl is itself a `.bzl` file, just one
/// we ship inside loupe-core.
///
/// Kept private so the cache always uses the same globals identity:
/// builtins evaluation is deterministic regardless of which call
/// site (BUILD vs `.bzl`) first triggers it.
fn wrappers_globals() -> Globals {
    use starlark::environment::{GlobalsBuilder, LibraryExtension::*};
    GlobalsBuilder::extended_by(&[
        StructType,
        RecordType,
        EnumType,
        NamespaceType,
        Map,
        Filter,
        Partial,
        Debug,
        Print,
        Pprint,
        Pstr,
        Prepr,
        Json,
        Typing,
        SetType,
    ])
    .with(crate::starlark_types::register_for_bzl)
    .build()
}

/// Parse + evaluate [`WRAPPERS_BZL`] once, returning the cached
/// frozen module. Builtins are evaluated against
/// [`wrappers_globals`] regardless of which call site first triggers
/// the load — same identity for provider equality across all
/// consumers.
pub fn embedded_builtins() -> Result<std::sync::Arc<FrozenModule>> {
    if let Some(fm) = CACHED.get() {
        return Ok(std::sync::Arc::clone(fm));
    }
    let globals = wrappers_globals();
    let ast = AstModule::parse(
        WRAPPERS_BZL_FILENAME,
        WRAPPERS_BZL.to_string(),
        &Dialect::Standard,
    )
    .map_err(|e| anyhow!("parsing builtins wrappers.bzl: {e}"))?;
    let frozen = Module::with_temp_heap(|module| -> Result<FrozenModule> {
        {
            let mut eval = Evaluator::new(&module);
            eval.eval_module(ast, &globals)
                .map_err(|e| anyhow!("evaluating builtins wrappers.bzl: {e}"))?;
        }
        module
            .freeze()
            .map_err(|e| anyhow!("freezing builtins wrappers.bzl: {e:?}"))
    })?;
    let arc = std::sync::Arc::new(frozen);
    let _ = CACHED.set(std::sync::Arc::clone(&arc));
    // Race-safe: another thread may have populated the cell first.
    // Either Arc points at an observably-equivalent FrozenModule.
    Ok(CACHED.get().cloned().unwrap_or(arc))
}

/// Anchor [`embedded_builtins`]'s frozen heap on `module` and inject
/// every top-level name from the cached builtins module as a
/// pre-bound name in `module`. Call this *before* the evaluator runs
/// user code on `module`.
pub fn inject_builtins_into(module: &Module) -> Result<()> {
    let builtins = embedded_builtins()?;
    module.frozen_heap().add_reference(builtins.frozen_heap());
    for name in builtins.names() {
        let symbol = name.as_str();
        let owned = match builtins.get_option(symbol) {
            Ok(Some(v)) => v,
            _ => continue,
        };
        // SAFETY: anchored above via add_reference; the FrozenValue
        // we promote remains valid for the lifetime of `module`'s
        // frozen heap.
        let frozen_value = unsafe { owned.unchecked_frozen_value() };
        module.set(symbol, frozen_value.to_value());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_builtins_exposes_coverage_common() {
        let builtins = embedded_builtins().unwrap();
        let names: std::collections::BTreeSet<String> =
            builtins.names().map(|n| n.as_str().to_string()).collect();
        assert!(
            names.contains("coverage_common"),
            "expected coverage_common in wrappers.bzl exports, got {names:?}"
        );
        assert!(
            names.contains("InstrumentedFilesInfo"),
            "expected InstrumentedFilesInfo in wrappers.bzl exports, got {names:?}"
        );
    }

    #[test]
    fn inject_builtins_makes_coverage_common_visible_in_user_module() {
        let globals = wrappers_globals();
        Module::with_temp_heap(|module| {
            inject_builtins_into(&module).unwrap();
            // The injected name should now resolve as a module-local
            // binding. Evaluate a tiny snippet that reads it.
            let ast = AstModule::parse(
                "test.bzl",
                "result = type(coverage_common)".to_string(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(ast, &globals).unwrap();
            }
            let result = module.get("result").unwrap();
            assert_eq!(result.unpack_str(), Some("struct"));
        });
    }
}
