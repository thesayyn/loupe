//! `transition()` — user-defined configuration transitions.
//!
//! A transition is a callable `(settings, attr) -> dict` (or, for splits,
//! `-> list[dict]` / `-> dict[str, dict]`) that rewrites build settings.
//! Rules attach one at the attr level (`attr.label(cfg = my_transition)`)
//! or the rule level (`rule(cfg = my_transition)`). The value records the
//! `implementation` plus the declared `inputs` / `outputs` so analysis
//! can pass the right `settings` dict in and validate the result.
//!
//! `get_attr` exposes `implementation` / `inputs` / `outputs` so the
//! analysis-time applier (`dice_keys::apply_user_transition`) reads them
//! the same way it reads a `_TransitionInfo`-style struct.

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_complex_values;
use starlark::starlark_module;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

/// What `transition()` returns. Generic over `V` so the implementation
/// function freezes with its defining `.bzl`.
#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<transition>")]
pub struct TransitionGen<V: ValueLifetimeless> {
    /// The transition's `implementation` function `(settings, attr)`.
    pub implementation: V,
    /// Build-setting keys the impl reads (passed in as `settings`).
    #[trace(static)]
    pub inputs: Vec<String>,
    /// Build-setting keys the impl is allowed to write (its result is
    /// validated against this set).
    #[trace(static)]
    pub outputs: Vec<String>,
    /// Builtin fixed-output transitions (`analysis_test_transition`)
    /// carry their `(label, value)` overlay here and have no real
    /// `implementation` to evaluate — `apply_user_transition` applies
    /// these verbatim. `None` for user `transition()` values.
    #[trace(static)]
    pub fixed_outputs: Option<Vec<(String, String)>>,
}

pub type Transition<'v> = TransitionGen<Value<'v>>;
pub type FrozenTransition = TransitionGen<FrozenValue>;

starlark_complex_values!(Transition);

impl<'v> Freeze for Transition<'v> {
    type Frozen = FrozenTransition;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(TransitionGen {
            implementation: self.implementation.freeze(freezer)?,
            inputs: self.inputs,
            outputs: self.outputs,
            fixed_outputs: self.fixed_outputs,
        })
    }
}

#[starlark_value(type = "transition")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TransitionGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "implementation" => Some(self.implementation.to_value()),
            "inputs" => Some(heap.alloc(self.inputs.clone())),
            "outputs" => Some(heap.alloc(self.outputs.clone())),
            _ => None,
        }
    }

    fn has_attr(&self, attribute: &str, _heap: Heap<'v>) -> bool {
        matches!(attribute, "implementation" | "inputs" | "outputs")
    }

    fn dir_attr(&self) -> Vec<String> {
        ["implementation", "inputs", "outputs"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}

/// Register the `transition()` global.
#[starlark_module]
pub fn transition_globals(b: &mut GlobalsBuilder) {
    /// `transition(implementation, inputs, outputs)` — declare a
    /// configuration transition. `inputs`/`outputs` are lists of
    /// build-setting labels (e.g. `"//command_line_option:cpu"`).
    fn transition<'v>(
        implementation: Value<'v>,
        #[starlark(require = named)] inputs: Option<UnpackListOrTuple<String>>,
        #[starlark(require = named)] outputs: Option<UnpackListOrTuple<String>>,
    ) -> anyhow::Result<Transition<'v>> {
        Ok(Transition {
            implementation,
            inputs: inputs.map(|l| l.items).unwrap_or_default(),
            outputs: outputs.map(|l| l.items).unwrap_or_default(),
            fixed_outputs: None,
        })
    }
}
