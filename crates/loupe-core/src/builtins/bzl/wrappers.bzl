"""Loupe-side `wrappers.bzl` — root of the Bazel-compatibility tree.

This file's top-level bindings are auto-injected as globals into
every user `.bzl` and BUILD evaluation, so consumer code (rules_oci,
rules_shell, aspect_bazel_lib, …) sees them as native — no
`load(…)` required.

The pattern matches what `bonanza` does in
`starlark/builtins_core/wrappers.bzl`: keep the bottom primitives
(`provider`, `attr.*`, `rule`, `ctx.actions.*`, `File`, `depset`)
in the host (Rust), and layer Bazel-compatibility behaviour on top
in user-space starlark. New compat surface lands as a `.bzl` change.

`wrappers.bzl` itself is a thin aggregator — almost everything lives
in sub-files under `@_builtins//common/`. Adding a new file means:

  1. Drop the `.bzl` under `bzl/<area>/<name>.bzl`.
  2. Register it in `crate::builtins::EMBEDDED_FILES` (in dependency
     order — files later in the list may `load()` from earlier
     entries).
  3. `load()` it from `wrappers.bzl` and re-export the public names
     so the auto-injection picks them up.

This shape is what makes vendoring Bazel's actual `builtins_bzl/`
tree (or a curated subset) mechanical: drop the files in, append to
`EMBEDDED_FILES`, re-export from `wrappers.bzl`.

CONSTRAINT: every embedded file is evaluated against the *base*
`bzl_globals` only — no auto-injected names are visible during
builtins evaluation. Files reach for `provider()`, `struct(...)`,
etc. from the host directly.
"""

load(
    "@_builtins//common/coverage.bzl",
    _InstrumentedFilesInfo = "InstrumentedFilesInfo",
    _coverage_common = "coverage_common",
)
load(
    "@_builtins//common/platform.bzl",
    _ConstraintSettingInfo = "ConstraintSettingInfo",
    _ConstraintValueInfo = "ConstraintValueInfo",
    _PlatformInfo = "PlatformInfo",
    _TemplateVariableInfo = "TemplateVariableInfo",
    _ToolchainInfo = "ToolchainInfo",
    _platform_common = "platform_common",
)
load(
    "@_builtins//common/providers.bzl",
    _DefaultInfo = "DefaultInfo",
    _OutputGroupInfo = "OutputGroupInfo",
)

# Re-export every public name as a top-level binding so the Rust-
# side `inject_builtins_into` picks it up. Aliases use the `_X = X`
# import / `X = _X` re-export pattern because Starlark forbids
# top-level rebinding of an imported name.

InstrumentedFilesInfo = _InstrumentedFilesInfo
coverage_common = _coverage_common

PlatformInfo = _PlatformInfo
ToolchainInfo = _ToolchainInfo
ConstraintSettingInfo = _ConstraintSettingInfo
ConstraintValueInfo = _ConstraintValueInfo
TemplateVariableInfo = _TemplateVariableInfo
platform_common = _platform_common

DefaultInfo = _DefaultInfo
OutputGroupInfo = _OutputGroupInfo

# `_builtins.toplevel.*` and `_builtins.internal.*` — the magic
# namespace Bazel's actual `builtins_bzl/*.bzl` files reach for. Real
# Bazel populates `_builtins.toplevel` from C++/Java-side registered
# values (provider, rule, attr, etc.); Loupe re-publishes its
# Rust-side globals here so vendored Bazel files that say e.g.
# `_builtins.toplevel.provider(...)` resolve to the same primitive
# user `.bzl`s see directly.
#
# Lives as a regular Starlark struct here, then gets re-published
# under the `_builtins` symbol via the Rust loader (the underscore
# prefix would otherwise mark it private to the wrappers module).
# Consumed by `crate::builtins::extract_builtins_namespace` which
# pulls this struct out of the frozen module and registers it as a
# real global so user `.bzl`s that say `_builtins.toplevel.provider`
# resolve.
#
# `internal` is the back-channel Bazel uses for non-public registered
# names (`cc_internal`, `proto_internal`, …). Loupe doesn't currently
# expose any internals — empty struct keeps the access shape valid
# until specific names are needed.
builtins_namespace = struct(
    toplevel = struct(
        provider = provider,
        rule = rule,
        aspect = aspect,
        attr = attr,
        struct = struct,
        depset = depset,
        select = select,
        DefaultInfo = _DefaultInfo,
        OutputGroupInfo = _OutputGroupInfo,
        InstrumentedFilesInfo = _InstrumentedFilesInfo,
        coverage_common = _coverage_common,
        platform_common = _platform_common,
        PlatformInfo = _PlatformInfo,
        ToolchainInfo = _ToolchainInfo,
        ConstraintSettingInfo = _ConstraintSettingInfo,
        ConstraintValueInfo = _ConstraintValueInfo,
        TemplateVariableInfo = _TemplateVariableInfo,
    ),
    internal = struct(),
)
