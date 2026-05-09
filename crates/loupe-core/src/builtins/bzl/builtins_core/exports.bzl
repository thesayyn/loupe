"""Loupe `builtins_core/exports.bzl` — native rules as user-space rules.

Mirrors bonanza's
[`starlark/builtins_core/exports.bzl`](https://github.com/buildbarn/bonanza/blob/main/starlark/builtins_core/exports.bzl).
Native Bazel rules whose semantics are simple enough to express in
Starlark live here as `rule(...)` calls. The engine sees them as
ordinary user-defined rules — they go through `RuleClass::invoke` at
BUILD-eval time and `analysis::run_analysis` (via `invoke_rule`) at
analyze time.

Native rules NOT defined here have engine-tied semantics that are
awkward to express as a rule:

- `alias` — provider forwarding lives in
  `dice_keys.rs::synthesise_alias_analysis`.
- `config_setting` / `platform` / `constraint_setting` /
  `constraint_value` / `toolchain` / `toolchain_type` — the
  toolchain resolver reads their attrs directly off the
  UnconfiguredTarget.

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

# Empty-impl native rules. Their impls return `[DefaultInfo()]` —
# enough for `bazel cquery` introspection. v1 doesn't model action
# execution, so the impls don't actually run any commands. Tests and
# binaries pick up implicit Bazel attrs (`size`, `timeout`, `flaky`,
# `local`, `shard_count`, `args`, `env`, `env_inherit`) automatically
# through `rule(test=True)` / `rule(executable=True)` synthesis.

def _empty_impl(ctx):
    _ = ctx  # buildifier: unused-variable
    return [DefaultInfo()]

sh_library = rule(
    implementation = _empty_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
        "data": attr.label_list(allow_files = True),
    },
)

sh_binary = rule(
    implementation = _empty_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
        "data": attr.label_list(allow_files = True),
    },
    executable = True,
)

sh_test = rule(
    implementation = _empty_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
        "data": attr.label_list(allow_files = True),
    },
    test = True,
)

cc_library = rule(
    implementation = _empty_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "hdrs": attr.label_list(allow_files = True),
        "deps": attr.label_list(),
        "data": attr.label_list(allow_files = True),
    },
)

genrule = rule(
    implementation = _empty_impl,
    attrs = {
        "srcs": attr.label_list(allow_files = True),
        "outs": attr.output_list(),
        "cmd": attr.string(),
        "tools": attr.label_list(),
    },
)

test_suite = rule(
    implementation = _empty_impl,
    attrs = {
        "tests": attr.label_list(),
    },
)
