//! `aspect()` — accepted but never propagated in v1.
//!
//! DESIGN.md: "Aspects accepted syntactically but not propagated."
//! Phase 3 only needs `aspect()` to evaluate without erroring; analysis
//! (Phase 5) ignores aspect-typed attrs entirely.

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, StarlarkValue, Value, starlark_value};

#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<aspect>")]
pub struct Aspect;

starlark_simple_value!(Aspect);

#[starlark_value(type = "aspect")]
impl<'v> StarlarkValue<'v> for Aspect {}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
pub fn aspect_globals(b: &mut GlobalsBuilder) {
    /// Defines an aspect. v1 does not propagate aspects across edges;
    /// the call returns a sentinel that satisfies attr-typing but does
    /// nothing during analysis.
    fn aspect<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(require = named)] attr_aspects: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] required_providers: Option<Value<'v>>,
        // Vendored Bazel `aspect()` accepts a broader keyword surface
        // than Loupe models. v1 doesn't propagate aspects, so each
        // kwarg is accepted-and-discarded — including the additional
        // names cc_shared_library.bzl and friends pass:
        // `required_aspect_providers`, `toolchains`, `fragments`,
        // `host_fragments`, `apply_to_generating_rules`, `exec_groups`,
        // `subrules`. Adding them as explicit `Option<Value>` instead
        // of `**kwargs` keeps the typed surface visible for the day
        // aspect propagation lands.
        #[starlark(require = named)] required_aspect_providers: Option<Value<'v>>,
        #[starlark(require = named)] provides: Option<Value<'v>>,
        #[starlark(require = named)] toolchains: Option<Value<'v>>,
        #[starlark(require = named)] fragments: Option<Value<'v>>,
        #[starlark(require = named)] host_fragments: Option<Value<'v>>,
        #[starlark(require = named)] apply_to_generating_rules: Option<Value<'v>>,
        #[starlark(require = named)] exec_groups: Option<Value<'v>>,
        #[starlark(require = named)] subrules: Option<Value<'v>>,
        // `requires = [other_aspect, …]` declares aspect-dependency
        // ordering. rules_js's `js_proto_aspect_test.bzl` uses it to
        // sequence its testing aspect after `js_proto_aspect`. v1
        // doesn't propagate aspects, so accepted-and-discarded.
        #[starlark(require = named)] requires: Option<Value<'v>>,
        #[starlark(require = named, default = "")] doc: &str,
    ) -> anyhow::Result<Aspect> {
        // STUB: the implementation function and edge propagation rules
        // are accepted but discarded — Phase 5+ revisits when aspects
        // become real.
        let _ = (
            implementation,
            attr_aspects,
            attrs,
            required_providers,
            required_aspect_providers,
            provides,
            toolchains,
            fragments,
            host_fragments,
            apply_to_generating_rules,
            exec_groups,
            subrules,
            requires,
            doc,
        );
        Ok(Aspect)
    }
}
