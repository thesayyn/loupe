//! Loupe-defined Starlark value types and the globals that build them.
//!
//! Phase 3 introduces:
//! - `provider()` → [`provider::ProviderType`] (callable in `.bzl` and
//!   in rule impls; instances are built by Phase 5).
//! - `attr.string`/`attr.int`/… → [`attr::AttrSpec`].
//! - `rule()` → [`rule::RuleClass`] (the schema). The BUILD-callable
//!   binding [`rule::Rule`] is the wrapper allocated at load time —
//!   it carries the BUILD-side import alias so the recorded `kind`
//!   matches the name the BUILD file used. Calling a `Rule` from BUILD
//!   records an `UnconfiguredTarget`.
//! - `aspect()` → [`aspect::Aspect`] (no-op accept; v1 doesn't
//!   propagate, see DESIGN.md).

use starlark::environment::GlobalsBuilder;

pub mod aspect;
pub mod attr;
pub mod provider;
pub mod rule;

/// Register all Loupe-defined globals — `provider`, `rule`, `aspect`,
/// and the `attr` namespace. Pass to `GlobalsBuilder::with`.
pub fn register(builder: &mut GlobalsBuilder) {
    provider::provider_globals(builder);
    rule::rule_globals(builder);
    aspect::aspect_globals(builder);
    builder.namespace("attr", attr::attr_globals);
}
