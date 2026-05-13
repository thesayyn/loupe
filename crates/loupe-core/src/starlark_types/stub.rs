//! Compatibility no-op stubs for Bazel/Starlark APIs Loupe doesn't
//! model but rules and macros call defensively (or for visibility /
//! diagnostics that v1 doesn't enforce).
//!
//! Each entry here is a pragmatic STUB. Adding one is preferable to
//! teaching the loader to skip the call, because Starlark's evaluator
//! reports the error at the call site (where the user can see it),
//! while a loader-level skip would silently drop semantics.

use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;

#[starlark_module]
pub fn compat_globals(b: &mut GlobalsBuilder) {
    /// `visibility(["//some:label", "public", ...])` — declares
    /// load-time visibility for the current `.bzl`. Loupe doesn't
    /// enforce visibility; accepted-and-ignored so vendored / user
    /// `.bzl` files top-of-module-call this without error. Registered
    /// here (not in `native_rules`) because `visibility(...)` is
    /// callable from `.bzl` as well as BUILD context.
    fn visibility<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `repository_rule(...)` — module-extension / WORKSPACE-time
    /// repo declarator. Loupe doesn't fetch external repos
    /// (workspaces are vendored), so the constructor is a no-op
    /// returning a marker the caller can pass around without us
    /// running the rule.
    fn repository_rule<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `module_extension(...)` — bzlmod extension declarator. Same
    /// no-op story as `repository_rule`: Loupe doesn't run
    /// extensions, just records `bazel_dep`s from `MODULE.bazel`.
    fn module_extension<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `tag_class(...)` — module-extension tag-class declarator.
    fn tag_class<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `subrule(...)` — Bazel 7+ subrule constructor. Loupe doesn't
    /// dispatch subrules yet; accepted-and-ignored.
    fn subrule<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `transition(implementation, inputs, outputs)` — user
    /// transition declarator. Loupe records the value but doesn't
    /// fire transitions at v1; the cfg path captures `inputs` /
    /// `outputs` directly via `_TransitionInfo` reads instead.
    /// Returning `None` here is the "transitions disabled" sentinel.
    fn transition<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `analysis_test_transition(...)` — test-time transition
    /// declarator. v1 doesn't run analysis tests.
    fn analysis_test_transition<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `exec_group(...)` — multi-exec-platform exec group
    /// declarator. v1 collapses exec onto target platform.
    fn exec_group<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }
}
