//! Pass-through "any field works" struct. Narrowly scoped: only
//! used as the runtime shape of `_builtins.internal` during
//! embedded-tree evaluation. Vendored Bazel `builtins_bzl/`
//! files reach for `_builtins.internal.cc_common`, `cc_internal`,
//! `py_builtins`, `apple_common`, `java_common_internal_do_not_use`
//! at module-eval time and call methods on the resulting nested
//! structs from inside function bodies. None of those host
//! machinery values are modelled in Loupe; the wildcard lets the
//! embedded files freeze without a per-name stub avalanche.
//!
//! **Scope:** `pub(crate)` — never reachable from user `.bzl` /
//! BUILD code. `target[<provider>]` misses, `ctx.toolchains[T]`
//! misses, and depset entry-type errors raise instead.

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    allocative::Allocative,
    starlark::any::ProvidesStaticType,
    derive_more::Display,
    starlark::values::NoSerialize,
)]
#[display("<wildcard-stub>")]
pub(crate) struct WildcardStruct;

starlark::starlark_simple_value!(WildcardStruct);

use starlark::values::ValueLike as _;

#[starlark::values::starlark_value(type = "wildcard_struct")]
impl<'v> starlark::values::StarlarkValue<'v> for WildcardStruct {
    fn get_attr(
        &self,
        _attribute: &str,
        heap: starlark::values::Heap<'v>,
    ) -> Option<starlark::values::Value<'v>> {
        Some(heap.alloc(WildcardStruct))
    }

    fn dir_attr(&self) -> Vec<String> {
        Vec::new()
    }

    fn at(
        &self,
        _index: starlark::values::Value<'v>,
        heap: starlark::values::Heap<'v>,
    ) -> starlark::Result<starlark::values::Value<'v>> {
        Ok(heap.alloc(WildcardStruct))
    }

    fn length(&self) -> starlark::Result<i32> {
        Ok(0)
    }

    fn to_bool(&self) -> bool {
        false
    }

    fn equals(&self, other: starlark::values::Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<WildcardStruct>().is_some())
    }

    fn invoke(
        &self,
        _me: starlark::values::Value<'v>,
        _args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<starlark::values::Value<'v>> {
        Ok(eval.heap().alloc(WildcardStruct))
    }

    fn iterate_collect(
        &self,
        _heap: starlark::values::Heap<'v>,
    ) -> starlark::Result<Vec<starlark::values::Value<'v>>> {
        Ok(Vec::new())
    }
}
