"""Loupe-side `wrappers.bzl` — root of the Bazel-compatibility tree.

This file's top-level bindings are auto-injected as globals into
every user `.bzl` and BUILD evaluation, so consumer code (rules_oci,
rules_shell, aspect_bazel_lib, …) sees them as native — no
`load(…)` required.

Loupe keeps the bottom primitives (`provider`, `attr.*`, `rule`,
`ctx.actions.*`, `File`, `depset`) in the host (Rust), and layers
Bazel-compatibility behaviour on top in user-space starlark. New
compat surface lands as a `.bzl` change.

`wrappers.bzl` itself is a thin aggregator — almost everything lives
in sub-files under `@_builtins//common/` and `@_builtins//builtins_core/`.
Adding a new file means:

  1. Drop the `.bzl` under `bzl/<area>/<name>.bzl`.
  2. Register it in `crate::builtins::EMBEDDED_FILES` (in dependency
     order — files later in the list may `load()` from earlier
     entries).
  3. `load()` it from `wrappers.bzl` and re-export the public names
     so the auto-injection picks them up.

CONSTRAINT: every embedded file is evaluated against the *base*
`bzl_globals` only — no auto-injected names are visible during
builtins evaluation. Files reach for `provider()`, `struct(...)`,
etc. from the host directly. The `_builtins.{toplevel,internal}`
magic namespace IS visible during evaluation: it's a snapshot of
`builtins_namespace` below, materialised by the Rust loader (see
`crate::builtins::inject_builtins_magic`).
"""

load(
    "@_builtins//builtins_core/config.bzl",
    _config = "config",
)
load(
    "@_builtins//builtins_core/exports.bzl",
    _BuildSettingInfo = "BuildSettingInfo",
    _ConfigSettingInfo = "ConfigSettingInfo",
    _cc_library = "cc_library",
    _config_setting = "config_setting",
    _filegroup = "filegroup",
    _genrule = "genrule",
    _sh_binary = "sh_binary",
    _sh_library = "sh_library",
    _sh_test = "sh_test",
    _test_suite = "test_suite",
)
load(
    "@_builtins//builtins_core/stubs.bzl",
    _CcInfo = "CcInfo",
    _CcLauncherInfo = "CcLauncherInfo",
    _CcSharedLibraryHintInfo = "CcSharedLibraryHintInfo",
    _CcSharedLibraryInfo = "CcSharedLibraryInfo",
    _CcToolchainConfigInfo = "CcToolchainConfigInfo",
    _CcToolchainInfo = "CcToolchainInfo",
    _DebugPackageInfo = "DebugPackageInfo",
    _JavaInfo = "JavaInfo",
    _JavaPluginInfo = "JavaPluginInfo",
    _PackageSpecificationInfo = "PackageSpecificationInfo",
    _ProtoInfo = "ProtoInfo",
    _PyCcLinkParamsProvider = "PyCcLinkParamsProvider",
    _PyInfo = "PyInfo",
    _PyRuntimeInfo = "PyRuntimeInfo",
    _RunEnvironmentInfo = "RunEnvironmentInfo",
    _analysis_test_transition = "analysis_test_transition",
    _cc_proto_aspect = "cc_proto_aspect",
    _exec_group = "exec_group",
    _licenses = "licenses",
    _macro = "macro",
    _module_extension = "module_extension",
    _package = "package",
    _package_group = "package_group",
    _repository_rule = "repository_rule",
    _subrule = "subrule",
    _tag_class = "tag_class",
    _transition = "transition",
    _visibility = "visibility",
)
load(
    "@_builtins//builtins_core/wrappers.bzl",
    _invoke_rule = "invoke_rule",
)
load(
    "@_builtins//builtins_core/native_compat.bzl",
    _native = "native",
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
load(
    "@_builtins//common/cc/cc_common_bazel.bzl",
    _cc_common = "cc_common",
)
load(
    "@_builtins//common/config_common.bzl",
    _config_common = "config_common",
)
load(
    "@_builtins//common/objc/apple_common.bzl",
    _apple_common = "apple_common_bazel",
)
load(
    "@_builtins//common/java/java_common.bzl",
    _java_common = "java_common_export_for_bazel",
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

# C++/Objective-C/Java host-built-in helper namespaces. Bazel ships
# these as top-level globals; vendored Bazel-provided files
# (`bazel_tools/tools/cpp/toolchain_utils.bzl`) and rules_cc reach
# for them by bare name. Underlying calls bottom out in
# `_builtins.internal.{cc_common,apple_common,java_common_internal_do_not_use}`,
# both of which Loupe stubs as `WildcardStruct`.
cc_common = _cc_common
apple_common = _apple_common
java_common = _java_common
config_common = _config_common

# Engine ↔ rule-impl indirection. The Rust analysis driver dispatches
# every rule's `implementation` callback through `invoke_rule(impl,
# ctx)` instead of calling `impl(ctx)` directly — see
# `crate::builtins::invoke_rule_callable` and `_wrap_rule_ctx` in
# `builtins_core/wrappers.bzl`.
invoke_rule = _invoke_rule

# `native` — Tier-2 user-space namespace. Shadows the Rust-side
# `compat::native_namespace` at user-.bzl injection time. Vendored
# `@cc_compatibility_proxy//:proxy.bzl` does
# `cc_binary = native.cc_binary` at module top level; this struct
# carries the stubs that line resolves against.
native = _native

# Native Bazel rules expressed as ordinary `rule(...)` calls.
cc_library = _cc_library
config_setting = _config_setting
filegroup = _filegroup
genrule = _genrule
sh_binary = _sh_binary
sh_library = _sh_library
sh_test = _sh_test
test_suite = _test_suite

# Provider types user code reads off `target[ConfigSettingInfo]` /
# `target[BuildSettingInfo]`.
ConfigSettingInfo = _ConfigSettingInfo
BuildSettingInfo = _BuildSettingInfo

# Bazel's `config.*` build-setting namespace.
config = _config

# Loupe-doesn't-model surface — accept-and-ignore stubs and native
# provider placeholders.
analysis_test_transition = _analysis_test_transition
exec_group = _exec_group
licenses = _licenses
macro = _macro
module_extension = _module_extension
package = _package
package_group = _package_group
repository_rule = _repository_rule
subrule = _subrule
tag_class = _tag_class
transition = _transition
visibility = _visibility

CcInfo = _CcInfo
CcLauncherInfo = _CcLauncherInfo
CcSharedLibraryHintInfo = _CcSharedLibraryHintInfo
CcSharedLibraryInfo = _CcSharedLibraryInfo
CcToolchainConfigInfo = _CcToolchainConfigInfo
CcToolchainInfo = _CcToolchainInfo
DebugPackageInfo = _DebugPackageInfo
JavaInfo = _JavaInfo
JavaPluginInfo = _JavaPluginInfo
PackageSpecificationInfo = _PackageSpecificationInfo
ProtoInfo = _ProtoInfo
PyCcLinkParamsProvider = _PyCcLinkParamsProvider
PyInfo = _PyInfo
PyRuntimeInfo = _PyRuntimeInfo
RunEnvironmentInfo = _RunEnvironmentInfo
cc_proto_aspect = _cc_proto_aspect

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
# to `native.filegroup`). Provide the binding so strict evaluation
# stays honest even though the lambda is unreachable in v1.
_native_namespace = struct(
    filegroup = _filegroup,
)

# `_builtins.{toplevel,internal}.*` — Bazel's magic injection
# namespace. Loupe re-publishes the host primitives here under
# `builtins_namespace`; the Rust loader snapshots this struct's
# fields and binds the result as `_builtins` on every user `.bzl` /
# BUILD module. See `crate::builtins::inject_builtins_into`.
#
# `toplevel` is strict: every name vendored Bazel reaches for through
# `_builtins.toplevel.X` must be present here or the access raises.
# Stubs for unmodelled host machinery (`exec_transition`,
# `proto_common_do_not_use`, …) are explicit Starlark values.
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
        exec_transition = _exec_transition_stub,
        proto_common_do_not_use = _proto_common_do_not_use,
        # Real provider in Bazel; Loupe doesn't enforce visibility, so
        # a `None` placeholder is enough for the binding sites that
        # forward this through their own structs.
        PackageSpecificationInfo = None,
        native = _native_namespace,
        config = _config,
    ),
    internal = struct(),
)
