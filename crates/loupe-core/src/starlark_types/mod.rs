//! Loupe-defined Starlark value types and the globals that build them.
//!
//! Phase 3 introduced `provider()`, `attr.*`, `rule()`, and `aspect()`.
//! Phase 4 adds the core value types: `select()`, `depset()`,
//! `glob()`, `exports_files()`, and `File` (currently passive — a
//! Starlark value users can receive but not construct directly).
//!
//! ## Layered globals
//!
//! From DETAILS.md:
//! > Globals are layered. Build separate `Globals` for loading-phase
//! > (no `ctx`) vs analysis-phase (no `glob`) so APIs aren't
//! > accidentally available in the wrong phase.
//!
//! Phase 4 splits along loading-vs-build lines:
//!
//! - [`register_for_bzl`] — `.bzl` globals: `provider`, `rule`,
//!   `aspect`, `attr.*`, `select`, `depset`. Definition-shaped APIs.
//! - [`register_for_build`] — BUILD globals: `select`, `depset`,
//!   `glob`, `exports_files`. Plus loaded rule constructors are bound
//!   by `load()` itself, so BUILD authors can call them as if they
//!   were native.
//!
//! Analysis-phase globals (with `ctx`) arrive in Phase 5 and live in
//! their own module.

use starlark::environment::GlobalsBuilder;

pub mod actions;
pub mod aspect;
pub mod attr;
pub mod ctx;
pub mod depset;
pub mod exports;
pub mod file;
pub mod glob;
pub mod provider;
pub mod rule;
pub mod runfiles;
pub mod selector;

/// Register globals available inside `.bzl` files: `provider`, `rule`,
/// `aspect`, `attr.*`, `select`, `depset`.
pub fn register_for_bzl(builder: &mut GlobalsBuilder) {
    provider::provider_globals(builder);
    rule::rule_globals(builder);
    aspect::aspect_globals(builder);
    selector::selector_globals(builder);
    builder.namespace("attr", attr::attr_globals);
}

/// Register globals available inside `BUILD`/`BUILD.bazel` files:
/// `select`, `depset`, `glob`, `exports_files`.
pub fn register_for_build(builder: &mut GlobalsBuilder) {
    selector::selector_globals(builder);
    glob::glob_globals(builder);
    exports::exports_globals(builder);
}

/// Register globals available inside rule implementation functions
/// during analysis. Phase 5 keeps this very narrow: provider
/// constructors are reachable through the `ctx`-passed Starlark
/// values (or the rule's own closure), but the impl itself only
/// touches the standard Starlark library plus `select` (resolved at
/// call boundaries, not invokable here — but a pre-resolved selector
/// might pass through unchanged).
pub fn register_for_analysis(_builder: &mut GlobalsBuilder) {
    // Intentionally empty for v1: rule impls already see
    // their `.bzl`'s loaded symbols (provider types, helpers) through
    // the FrozenValue closures. There's no analysis-only global API
    // surface fixture #1 needs. Phase 5+ adds depset/select/etc.
    // here as analysis grows.
}
