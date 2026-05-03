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
        #[starlark(require = named)] provides: Option<Value<'v>>,
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
            provides,
            doc,
        );
        Ok(Aspect)
    }
}
