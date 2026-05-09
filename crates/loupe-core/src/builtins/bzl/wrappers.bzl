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
etc. from the host directly. The `_builtins.{toplevel,internal}`
magic namespace IS visible during evaluation: it's injected from
the Rust loader as a wildcard-shaped struct (see
`crate::builtins::inject_builtins_magic`).
"""

load(
    "@_builtins//builtins_core/config.bzl",
    _config = "config",
)
load(
    "@_builtins//builtins_core/exports.bzl",
    _cc_library = "cc_library",
    _filegroup = "filegroup",
    _genrule = "genrule",
    _sh_binary = "sh_binary",
    _sh_library = "sh_library",
    _sh_test = "sh_test",
    _test_suite = "test_suite",
)
load(
    "@_builtins//builtins_core/wrappers.bzl",
    _invoke_rule = "invoke_rule",
)
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

# Engine ↔ rule-impl indirection. Re-exported so the Rust analysis
# driver can resolve it through the auto-injected globals on the
# user `.bzl`'s frozen module. See
# `crate::builtins::invoke_rule_callable` for how the engine fishes
# this out and dispatches.
invoke_rule = _invoke_rule

# Native Bazel rules that bonanza expresses as ordinary `rule(...)`
# calls (Phase 3 of the bonanza-pattern refactor). Re-exported so the
# auto-injection picks them up — user BUILD files see them as
# globals, just like Bazel exposes filegroup/genrule/etc. natively.
cc_library = _cc_library
filegroup = _filegroup
genrule = _genrule
sh_binary = _sh_binary
sh_library = _sh_library
sh_test = _sh_test
test_suite = _test_suite

# `config` — Bazel's build-setting namespace. Phase 4 of the bonanza-
# pattern refactor lifts this out of the wildcard fall-through and
# into a real Starlark struct in `builtins_core/config.bzl`. Mirrors
# bonanza's `pkg/model/starlark/builtins.go::config` block — same
# names, returning the same shapes.
config = _config

# `_builtins.toplevel.*` and `_builtins.internal.*` — Bazel's magic
# injection namespace. Loupe re-publishes the host primitives here
# under `builtins_namespace`; the Rust loader picks up this struct
# and binds the result as `_builtins` on every user `.bzl` / BUILD
# module. See `crate::builtins::inject_builtins_into`.
#
# Phase 4 of the bonanza-pattern refactor made the `toplevel` side
# strict: every name vendored Bazel reaches for through
# `_builtins.toplevel.X` must be present here or the access raises.
# Stubs for unmodelled host machinery (`exec_transition`,
# `proto_common_do_not_use`, …) are explicit Starlark values rather
# than wildcard fall-through.
#
# `internal` stays as a `WildcardStruct` until Phase 5 deletes the
# type — vendored files reach for `_builtins.internal.cc_common` /
# `cc_internal` / `py_builtins` / `apple_common` /
# `java_common_internal_do_not_use` and there's no clean stub story
# yet.

# Stubs for unmodelled host machinery the vendored Bazel
# `builtins_bzl/` tree reaches for at module-eval time.

def _exec_transition_stub(implementation = None, inputs = [], outputs = []):
    # `bazel-out/<config>/...` configuration transition. Loupe doesn't
    # model transitions; return `None` so vendored
    # `bazel_exec_transition = _builtins.toplevel.exec_transition(...)`
    # at top level evaluates to a captured but never-invoked stub.
    _ = (implementation, inputs, outputs)
    return None

def _proto_toolchain_resolution_stub():
    # Read once at top level by Bazel's `common/exports.bzl` to seed
    # the `proto_common_do_not_use.INCOMPATIBLE_*` flag. False matches
    # Bazel's pre-flag default.
    return False

_proto_common_do_not_use = struct(
    incompatible_enable_proto_toolchain_resolution = _proto_toolchain_resolution_stub,
)

# `_builtins.toplevel.native.filegroup` — accessed inside a lambda
# body in `bazel/exports.bzl` (`cc_toolchain_suite` macro forwards
# to `native.filegroup`). The lambda only runs if the macro is
# invoked; at v1 it isn't, but providing the binding keeps strict
# evaluation honest if a future ruleset reaches for it.
_native_namespace = struct(
    filegroup = _filegroup,
)

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
        # Stubs for unmodelled host machinery — see notes above.
        exec_transition = _exec_transition_stub,
        proto_common_do_not_use = _proto_common_do_not_use,
        # `PackageSpecificationInfo` is a real provider in Bazel
        # (carries package-set semantics for visibility checks).
        # Loupe doesn't enforce visibility, so a `None` placeholder
        # is enough for `py_internal.bzl`'s top-level binding to
        # store something — downstream visibility code never reaches
        # that path.
        PackageSpecificationInfo = None,
        native = _native_namespace,
        config = _config,
    ),
    internal = struct(),
)
