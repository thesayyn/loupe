"""Loupe-side `wrappers.bzl` — the .bzl-resident slice of the
Bazel-compatibility surface.

Every symbol defined here is auto-injected as a top-level name in
every user `.bzl` and BUILD evaluation, so consumer code (rules_oci,
rules_shell, aspect_bazel_lib, …) sees them as if they were native
globals — no `load("@_builtins//…")` required.

The pattern matches what `bonanza` does in
`starlark/builtins_core/wrappers.bzl`: keep the bottom primitives
(`provider`, `attr.*`, `rule`, `ctx.actions.*`, `File`, `depset`)
in the host (Rust), and layer Bazel-compatibility behaviour on top
in this file. New compat surface lands here as a `.bzl` change, not
a Rust change.

CONSTRAINT: this file is evaluated against the *base* `bzl_globals`
(the same globals user `.bzl`s see, *minus* the auto-injected
symbols). It can use `provider()`, `struct(...)`, `attr.*`, etc.
It must not depend on any other auto-injected symbol — that would
create an evaluation-order cycle.
"""

# coverage_common.instrumented_files_info(ctx, source_attributes=...,
# dependency_attributes=...) — Bazel's coverage hook. v1 doesn't model
# coverage but `rules_shell`'s `sh_library` (reached transitively from
# `oci_load`) returns the result alongside DefaultInfo. The constructor
# captures the attribute-name lists as provider fields and ignores ctx
# (no execution → no instrumentation).
InstrumentedFilesInfo = provider(
    doc = "Bazel coverage_common.instrumented_files_info return shape.",
    fields = ["source_attributes", "dependency_attributes", "extensions", "metadata_files"],
)

def _instrumented_files_info(
        ctx,
        source_attributes = [],
        dependency_attributes = [],
        extensions = [],
        metadata_files = [],
        **kwargs):
    # ctx is positional in Bazel's signature but unused here — v1
    # doesn't generate coverage. Extra kwargs (e.g. `source_extensions`,
    # `dependency_attribute_attribute_aspects`) are silently dropped.
    _ = ctx
    _ = kwargs
    return InstrumentedFilesInfo(
        source_attributes = source_attributes,
        dependency_attributes = dependency_attributes,
        extensions = extensions,
        metadata_files = metadata_files,
    )

coverage_common = struct(
    instrumented_files_info = _instrumented_files_info,
)

# platform_common.* — native provider type stubs for Bazel's platform
# / toolchain / constraint surface. Loupe's Rust toolchain resolver
# looks them up by *symbol* (see `provider_by_symbol`) so the user
# can construct them as `platform_common.ToolchainInfo(...)` and the
# result lands in `target[platform_common.ToolchainInfo]` cleanly,
# regardless of whether the provider type lives in Rust or here.
PlatformInfo = provider(doc = "Bazel platform_common.PlatformInfo. STUB at v1.")
ToolchainInfo = provider(doc = "Bazel platform_common.ToolchainInfo. Loupe's toolchain resolver populates this on resolved-toolchain target results.")
ConstraintSettingInfo = provider(doc = "Bazel platform_common.ConstraintSettingInfo. STUB at v1.")
ConstraintValueInfo = provider(doc = "Bazel platform_common.ConstraintValueInfo. STUB at v1.")
TemplateVariableInfo = provider(doc = "Bazel platform_common.TemplateVariableInfo. STUB at v1.")

platform_common = struct(
    PlatformInfo = PlatformInfo,
    ToolchainInfo = ToolchainInfo,
    ConstraintSettingInfo = ConstraintSettingInfo,
    ConstraintValueInfo = ConstraintValueInfo,
    TemplateVariableInfo = TemplateVariableInfo,
)

# OutputGroupInfo — Bazel's named output groups. Open-fields native
# provider whose user-chosen kwargs become group names. v1 surfaces
# `target.output_groups` by reading whatever the impl returned with
# this provider. Loupe's Rust analysis looks up the provider by
# symbol (see `provider_by_symbol`), so the .bzl-defined version
# matches identically.
OutputGroupInfo = provider(
    doc = "Bazel's named output groups. Open-fields — group names are user-chosen kwargs.",
)
