"""Loupe `builtins_core/wrappers.bzl` — engine ↔ rule-impl indirection.

Mirrors bonanza's
[`starlark/builtins_core/wrappers.bzl`](https://github.com/buildbarn/bonanza/blob/main/starlark/builtins_core/wrappers.bzl).
The engine no longer calls a rule's `implementation` callback
directly. It calls `invoke_rule(impl, ctx)`, which in turn calls
`impl(_wrap_rule_ctx(ctx))`. That single layer of indirection lets
us add Starlark-side `ctx`-derived fields (`expand_make_variables`,
`runfiles` wrappers, `build_file_path`, …) without touching Rust:
they get added in `_wrap_rule_ctx` rather than as host stubs.

Phase 1 (this file at first cut) is *just* the indirection.
`_wrap_rule_ctx` is the identity — it returns `ctx` unmodified — so
the call shape rules see is unchanged. Subsequent phases populate
the wrapper struct.

The Rust side calls this through
[`crate::builtins::invoke_rule_callable`], which fishes the
`invoke_rule` `FrozenValue` out of the cached embedded module and
hands it to `Evaluator::eval_function`.
"""

def _wrap_rule_ctx(ctx):
    """Phase 1: identity passthrough.

    Phase 2 replaces this with a `struct(**fields)` that adds the
    Starlark-derived ctx fields bonanza's wrappers.bzl synthesises.
    """
    return ctx

def invoke_rule(fn, ctx):
    """Engine entry point. Replaces `fn(ctx)` for every rule impl.

    Args:
        fn: The rule's `implementation` callable, captured at
            `rule(...)` definition time. A `FrozenValue` from the
            defining `.bzl`'s frozen module.
        ctx: The Rust-side `Ctx` value, freshly allocated for this
            target's analysis run. Phase 1 passes through; future
            phases wrap.

    Returns:
        Whatever the impl returns — typically a list of provider
        instances, but Loupe also tolerates a single provider
        (matching Bazel).
    """
    return fn(_wrap_rule_ctx(ctx))
