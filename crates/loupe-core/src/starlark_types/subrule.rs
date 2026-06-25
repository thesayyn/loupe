//! `subrule()` — Bazel 7+ subrules. A subrule is a reusable analysis
//! helper a rule (or another subrule) calls like a function; Bazel
//! injects a restricted subrule context as the first argument and
//! forwards the subrule's declared `attrs`. Loupe runs no cc/objc
//! actions, so it builds a *minimal* subrule context carrying only the
//! declared `fragments` — enough for the read-only cc subrule paths
//! (`create_fdo_context` reads `ctx.fragments.cpp.compilation_mode()`
//! and early-returns when it isn't `"opt"`) — and passes each declared
//! attr as `None` (their late-bound config defaults aren't read on that
//! path, but the impl signature requires them).
//!
//! This replaces the old accept-and-ignore stub that returned `None`,
//! which made `create_fdo_context = subrule(...)` fail with
//! "Operation `call()` not supported on type `NoneType`".

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_complex_values;
use starlark::values::structs::AllocStruct;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

use crate::starlark_types::configuration::CtxFragment;

/// What `subrule()` returns: a callable holding the implementation plus
/// the declared attr names and fragments. Generic over `V` so the
/// implementation function freezes with its defining `.bzl`.
#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<subrule>")]
pub struct SubruleGen<V: ValueLifetimeless> {
    /// The subrule's `implementation` function.
    pub implementation: V,
    /// Names of the subrule's declared late-bound `attrs`. Passed to the
    /// impl as `None` (unread on the read-only path, but required by the
    /// impl's keyword-only signature).
    #[trace(static)]
    pub attr_names: Vec<String>,
    /// Configuration fragments the subrule declared; surfaced on the
    /// minimal subrule ctx as `ctx.fragments.<name>`.
    #[trace(static)]
    pub fragments: Vec<String>,
}

pub type Subrule<'v> = SubruleGen<Value<'v>>;
pub type FrozenSubrule = SubruleGen<FrozenValue>;

starlark_complex_values!(Subrule);

impl<'v> Freeze for Subrule<'v> {
    type Frozen = FrozenSubrule;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(SubruleGen {
            implementation: self.implementation.freeze(freezer)?,
            attr_names: self.attr_names,
            fragments: self.fragments,
        })
    }
}

#[starlark_value(type = "subrule")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for SubruleGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        // Minimal subrule ctx: just the declared fragments (each a
        // `CtxFragment`). The read-only cc subrule paths only read
        // `ctx.fragments.<f>.compilation_mode()` before early-returning.
        let frag_pairs: Vec<(String, Value<'v>)> = self
            .fragments
            .iter()
            .map(|f| {
                (
                    f.clone(),
                    eval.heap().alloc(CtxFragment { name: f.clone() }),
                )
            })
            .collect();
        let fragments_struct = eval.heap().alloc(AllocStruct(frag_pairs));
        let subrule_ctx = eval
            .heap()
            .alloc(AllocStruct([("fragments", fragments_struct)]));

        // Positional args: the subrule ctx, then the caller's positionals.
        let caller_pos: Vec<Value<'v>> = args
            .positions(eval.heap())
            .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("subrule call: {e}")))?
            .collect();
        let mut positional: Vec<Value<'v>> = Vec::with_capacity(caller_pos.len() + 1);
        positional.push(subrule_ctx);
        positional.extend(caller_pos);

        // Named args: the caller's kwargs, plus each declared attr as
        // `None` (impl signature requires them; unread on this path).
        let names_map = args
            .names_map()
            .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("subrule kwargs: {e}")))?;
        let mut named: Vec<(&str, Value<'v>)> = Vec::with_capacity(names_map.len());
        for (k, v) in names_map.iter() {
            named.push((k.as_str(), *v));
        }
        for name in &self.attr_names {
            if !named.iter().any(|(n, _)| *n == name.as_str()) {
                named.push((name.as_str(), Value::new_none()));
            }
        }

        eval.eval_function(self.implementation.to_value(), &positional, &named)
    }
}
