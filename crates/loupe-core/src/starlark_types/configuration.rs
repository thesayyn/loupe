//! Native `ctx.configuration` and `ctx.fragments.<name>` views.
//!
//! Bazel rules read config-level state two ways: `ctx.configuration` for
//! predicates that don't belong to any single fragment
//! (`is_tool_configuration()`, `host_path_separator`), and
//! `ctx.fragments.<name>` for a fragment's own knobs
//! (`java.use_ijars()`, `cpp.copts`, …).
//!
//! Loupe builds both **natively in Rust** off the analyzed target's
//! [`crate::analysis::Configuration`] — there is no
//! `@bazel_tools//fragments:*` target to analyse and no `FragmentInfo`
//! provider to read back. The engine owns this surface because the
//! values are derived from engine-side configuration state, not from
//! user `.bzl`.
//!
//! v1 models the subset of fields that analyzed rules actually reach
//! (the `rules_oci` / `rules_js` surface plus the wiring test). Anything
//! else is left undefined, so the access raises (`AttributeError`) per
//! DESIGN.md's "real or fail; never silent stubs" — surfacing the gap
//! rather than returning a structurally-valid-but-wrong default. The cc
//! / objc / python fragment knobs the vendored tree reads are not in v1
//! scope (those rules aren't analyzed); they raise here until a fixture
//! makes one of them unavoidable, exactly like the toolchain path did.

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{Heap, NoSerialize, StarlarkValue, Value, starlark_value};

/// Host path-list separator: `:` on Unix, `;` on Windows. Bazel's
/// `ctx.configuration.host_path_separator` reports the exec host's, not
/// a configured flag.
fn host_path_sep() -> &'static str {
    if cfg!(windows) { ";" } else { ":" }
}

/// `ctx.configuration` — config-level state not owned by a fragment.
/// Built fresh per access from the analyzed target's `Configuration`.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("configuration")]
pub struct CtxConfiguration {
    /// Whether this is the exec (tool) configuration. Drives
    /// `is_tool_configuration()`.
    pub is_exec: bool,
}

starlark_simple_value!(CtxConfiguration);

#[starlark_value(type = "configuration")]
impl<'v> StarlarkValue<'v> for CtxConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(configuration_methods)
    }
}

#[starlark_module]
fn configuration_methods(builder: &mut MethodsBuilder) {
    /// `ctx.configuration.is_tool_configuration()` — `True` in the exec
    /// configuration. v1 derives it from `Configuration::is_exec`.
    fn is_tool_configuration<'v>(
        #[starlark(this)] this: &CtxConfiguration,
    ) -> anyhow::Result<bool> {
        Ok(this.is_exec)
    }

    /// `ctx.configuration.host_path_separator` — `:` / `;` from the
    /// exec host OS. Honest default (matches Bazel with no override),
    /// not a stub.
    #[starlark(attribute)]
    fn host_path_separator<'v>(
        #[starlark(this)] this: &CtxConfiguration,
    ) -> anyhow::Result<String> {
        let _ = this;
        Ok(host_path_sep().to_string())
    }

    /// `ctx.configuration.coverage_enabled` — `False` unless
    /// `--collect_code_coverage`, which v1 doesn't model. Honest
    /// Bazel default.
    #[starlark(attribute)]
    fn coverage_enabled<'v>(#[starlark(this)] this: &CtxConfiguration) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// `ctx.configuration.default_shell_env` — empty dict. Bazel
    /// populates it from `--action_env`; none modelled, so the honest
    /// default is empty.
    #[starlark(attribute)]
    fn default_shell_env<'v>(
        #[starlark(this)] this: &CtxConfiguration,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(SmallMap::<Value<'v>, Value<'v>>::new()))
    }
}

/// `ctx.fragments.<name>` — one configuration fragment's knobs. Carries
/// the fragment name so method impls can key off it; the configuration
/// state each knob needs is read here, not from an analysed target.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("fragment({name})")]
pub struct CtxFragment {
    pub name: String,
}

starlark_simple_value!(CtxFragment);

#[starlark_value(type = "fragment")]
impl<'v> StarlarkValue<'v> for CtxFragment {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(fragment_methods)
    }
}

#[starlark_module]
fn fragment_methods(builder: &mut MethodsBuilder) {
    /// `ctx.fragments.java.use_ijars()` — whether interface jars are
    /// used. Bazel's `--use_ijars` defaults to `True`; v1 reports that
    /// honest default and raises for any non-java fragment (none of
    /// which expose `use_ijars`).
    fn use_ijars<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        if this.name == "java" {
            Ok(true)
        } else {
            Err(anyhow!(
                "ctx.fragments.{}.use_ijars() is not modeled in v1",
                this.name
            ))
        }
    }
}
