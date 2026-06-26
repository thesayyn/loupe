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
pub mod config;
pub mod configuration;
pub mod ctx;
pub mod depset;
pub mod exports;
pub mod file;
pub mod glob;
pub mod native_rules;
pub mod provider;
pub mod rule;
pub mod runfiles;
pub mod selector;
pub mod stub;
pub mod subrule;
pub mod target;
pub mod toolchains;

/// Register globals available inside `.bzl` files: `provider`, `rule`,
/// `aspect`, `attr.*`, `select`, `depset`, plus the `config.*` and
/// `attr.*` namespaces. `DefaultInfo`, `OutputGroupInfo`,
/// `platform_common`, and `coverage_common` are injected by
/// `crate::builtins` from the embedded `wrappers.bzl` / `exports.bzl`
/// tree. The `native.*` BUILD globals (`alias`, `label_flag`, …) are
/// registered separately in [`register_for_build`].
pub fn register_for_bzl(builder: &mut GlobalsBuilder) {
    provider::provider_globals(builder);
    rule::rule_globals(builder);
    aspect::aspect_globals(builder);
    selector::selector_globals(builder);
    depset::register_depset(builder);
    loupe_labels::starlark::register_label(builder);
    stub::compat_globals(builder);
    // Native `runfiles(...)` constructor — the canonical runfiles
    // provider (replaces the vendored `.bzl` one). exports.bzl's
    // `DefaultInfo` init and the `_runfiles` alias resolve to it.
    runfiles::runfiles_globals(builder);
    builder.namespace("attr", attr::attr_globals);
    builder.namespace("config", config::config_globals);
    // `.bzl` files reach for `native.<rule>` (the legacy macro-API
    // form); register the same alias-family + glob set there. BUILD
    // files use the bare-name forms registered in
    // [`register_for_build`].
    builder.namespace("native", native_rules::native_rules_globals);
}

/// Register globals available inside `BUILD`/`BUILD.bazel` files:
/// `select`, `depset`, `glob`, `exports_files`, plus the alias-family
/// native rules as bare globals.
pub fn register_for_build(builder: &mut GlobalsBuilder) {
    selector::selector_globals(builder);
    depset::register_depset(builder);
    loupe_labels::starlark::register_label(builder);
    glob::glob_globals(builder);
    exports::exports_globals(builder);
    stub::compat_globals(builder);
    native_rules::native_rules_globals(builder);
}

/// Register globals available inside rule implementation functions
/// during analysis: `depset` constructor (impls accumulate transitive
/// data into depsets) plus the native `DefaultInfo` constructor (so
/// impls can `return [DefaultInfo(files=[...])]` without an import).
/// Provider types from the rule's defining `.bzl` reach the impl
/// through the rule's frozen closure, not through Globals.
pub fn register_for_analysis(builder: &mut GlobalsBuilder) {
    depset::register_depset(builder);
    // `DefaultInfo`, `OutputGroupInfo`, `platform_common`, and
    // `coverage_common` are provided by the `crate::builtins`
    // wrappers.bzl injection.
}
