//! Analysis-phase `ctx` value.
//!
//! The synthesized argument every rule impl receives. Phase 5 ships the
//! minimum surface fixture #1 needs:
//!
//! - `ctx.attr` — a struct of resolved attribute values.
//! - `ctx.label` — the configured target's label, as a string for now
//!   (Phase 5+ will replace with a real Starlark `Label` value).
//! - `ctx.workspace_name` — the main module name from `MODULE.bazel`.
//!
//! Rule impls return a list of provider instances; analysis machinery
//! reads them off the returned `Value`. `ctx` itself is a complex
//! value because the resolved `attr` struct lives on a Starlark heap.
//!
//! The full Bazel `ctx` surface (`ctx.actions.*`, `ctx.files`,
//! `ctx.executable`, `ctx.runfiles`, `ctx.toolchains`, …) lands after
//! the Phase 5 stop gate per `PLAN.md`.

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::starlark_complex_values;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<ctx for {label_str}>")]
pub struct CtxGen<V: ValueLifetimeless> {
    /// Configured target label as a string. Phase 5 stop-gate scope:
    /// keep it a string so the impl can stringify/compare; Phase 5+
    /// replaces with a real `Label` Starlark value.
    #[trace(static)]
    pub label_str: String,
    /// Main module name from `MODULE.bazel`. Bazel's
    /// `ctx.workspace_name`.
    #[trace(static)]
    pub workspace_name: String,
    /// Struct whose fields are the resolved attrs: `ctx.attr.<name>`
    /// returns `attr.get_attr(name)`. Built in the analysis heap via
    /// `AllocStruct`.
    pub attr: V,
}

pub type Ctx<'v> = CtxGen<Value<'v>>;
pub type FrozenCtx = CtxGen<FrozenValue>;

starlark_complex_values!(Ctx);

impl<'v> Freeze for Ctx<'v> {
    type Frozen = FrozenCtx;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(CtxGen {
            label_str: self.label_str,
            workspace_name: self.workspace_name,
            attr: self.attr.freeze(freezer)?,
        })
    }
}

#[starlark_value(type = "ctx")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for CtxGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "attr" => Some(self.attr.to_value()),
            "label" => Some(heap.alloc(self.label_str.as_str())),
            "workspace_name" => Some(heap.alloc(self.workspace_name.as_str())),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["attr".into(), "label".into(), "workspace_name".into()]
    }
}
