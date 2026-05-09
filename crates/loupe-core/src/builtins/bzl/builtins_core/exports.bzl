"""Loupe `builtins_core/exports.bzl` — native rules as user-space rules.

Mirrors bonanza's
[`starlark/builtins_core/exports.bzl`](https://github.com/buildbarn/bonanza/blob/main/starlark/builtins_core/exports.bzl).
Native Bazel rules whose semantics are simple enough to express in
Starlark live here as `rule(...)` calls. The engine sees them as
ordinary user-defined rules — they go through `RuleClass::invoke` at
BUILD-eval time and `analysis::run_analysis` (via `invoke_rule`) at
analyze time.

Phase 3 of the bonanza-pattern refactor lifts `filegroup` out of
`crates/loupe-core/src/starlark_types/native_rules.rs`. The legacy
`synthesise_filegroup_analysis` path in `dice_keys.rs` becomes
unreachable for this kind (any `filegroup(...)` target now carries
`bzl_label = Some(@_builtins//builtins_core/exports.bzl)`).

Native rules that *aren't* moved here have engine-tied semantics that
are awkward to express as a rule — `alias` (provider forwarding;
keep `synthesise_alias_analysis`), `config_setting`/`platform`/
`constraint_setting`/`constraint_value`/`toolchain`/`toolchain_type`
(toolchain resolver reads their attrs directly off the
UnconfiguredTarget). A future phase reworks those.

The embedded tree evaluator doesn't auto-inject the `DefaultInfo` /
`OutputGroupInfo` re-exports (those live on root `wrappers.bzl`), so
this file `load`s them directly from where they're declared.
"""

load("@_builtins//common/providers.bzl", "DefaultInfo")

def _filegroup_impl(ctx):
    """Collects every dep's `DefaultInfo.files` into a single depset.

    Mirrors bonanza's filegroup impl, simplified: Loupe v1 doesn't
    model `output_group` and runfiles flow at this site is delegated
    to `ctx.runfiles` callers downstream, so we just propagate
    `files`. When `srcs` is exactly one target we forward its full
    `DefaultInfo` so `files_to_run`/`runfiles` are preserved.
    """
    if len(ctx.attr.srcs) == 1:
        return [ctx.attr.srcs[0][DefaultInfo]]

    files = []
    for src in ctx.attr.srcs:
        files.append(src[DefaultInfo].files)
    return [DefaultInfo(files = depset(direct = [], transitive = files))]

filegroup = rule(
    implementation = _filegroup_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "data": attr.label_list(allow_files = True),
        "output_group": attr.string(),
    },
)
