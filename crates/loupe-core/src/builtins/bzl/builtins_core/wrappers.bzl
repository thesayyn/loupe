"""Loupe `builtins_core/wrappers.bzl` — engine ↔ rule-impl indirection.

Mirrors bonanza's
[`starlark/builtins_core/wrappers.bzl`](https://github.com/buildbarn/bonanza/blob/main/starlark/builtins_core/wrappers.bzl).
The engine never calls a rule's `implementation` callback directly.
It calls `invoke_rule(impl, ctx)`, which calls
`impl(_wrap_rule_ctx(ctx))`. That layer of indirection is the seam
where ctx-derived fields land in Starlark instead of as Rust stubs.

`_wrap_rule_ctx` adds the following over the raw engine `ctx`:

  - `build_file_path`, `genfiles_dir` — derived from raw ctx state.
  - `configuration`, `fragments`, `var` — empty/None placeholders.
  - `info_file`, `version_file` — empty `File`-shaped structs (path + short_path).
  - `expand_location`, `expand_make_variables`, `resolve_command`,
    `coverage_instrumented`, `target_platform_has_constraint` —
    Starlark stubs returning Bazel-shaped values.
  - `disabled_features`, `features` — empty lists Bazel-compat code reads.

The Rust-side `Ctx` is engine-essential only: `attr`, `label`,
`workspace_name`, `actions`, `files`, `file`, `executable`, `outputs`,
`toolchains`, `bin_dir`, plus the `runfiles` / `exec_groups` methods.

The Rust side calls `invoke_rule` through
[`crate::builtins::invoke_rule_callable`], which fishes the
`invoke_rule` `FrozenValue` out of the cached embedded module.
"""

def _wrap_rule_ctx(ctx):
    # Stub helpers. Each previously lived as a Rust no-op in
    # `ctx.rs::ctx_methods`; they're identical in shape and trip-wire
    # behaviour, just authored in Starlark so adding new derived
    # fields doesn't require touching Rust.

    def _coverage_instrumented(target = None):
        # STUB: Bazel honours `--instrumentation_filter`. Loupe doesn't
        # model coverage configuration, so always-False matches the
        # prior Rust stub.
        _ = target  # buildifier: unused-variable
        return False

    def _expand_location(input, targets = []):
        # STUB: real `$(location ...)` substitution needs the dep
        # graph. Pass through unchanged; matches the prior Rust stub.
        _ = targets
        return input

    def _resolve_command(
            command = "",
            attribute = None,
            expand_locations = False,
            make_variables = None,
            tools = [],
            label_dict = {},
            execution_requirements = {}):
        # STUB: Bazel returns `(inputs, command, environment)`. We
        # surface the empty tuple-shape so impls that destructure
        # don't crash. Matches the prior Rust stub.
        _ = (command, attribute, expand_locations, make_variables, tools, label_dict, execution_requirements)
        return [], [], {}

    def _expand_make_variables(attribute_name, command, additional_substitutions = None):
        # STUB: Bazel substitutes `$(VARIABLE)` tokens against
        # toolchain-provided make-vars. Loupe doesn't surface those
        # yet, so passthrough is the no-op of the right shape.
        _ = (attribute_name, additional_substitutions)
        return command

    def _target_platform_has_constraint(constraint_value):
        _ = constraint_value
        return False

    # `version_file` / `info_file` mirror Bazel's `--stamp`-mode stamp
    # files. Empty path placeholders keep `.path` reads from
    # exploding; rulesets that actually consume the contents would
    # need real plumbing.
    _empty_stamp_file = struct(path = "", short_path = "")

    # `ctx.configuration` — Bazel exposes a struct with `host_path_separator`,
    # `default_shell_env`, `coverage_enabled` etc. v1 surfaces an empty
    # placeholder so `ctx.configuration.default_shell_env` reads as `{}`
    # and string-concat / dict-merge consumers don't crash.
    _configuration = struct(
        default_shell_env = {},
        host_path_separator = ":",
        coverage_enabled = False,
    )

    return struct(
        # Engine-essential — forwarded from raw ctx unchanged.
        actions = ctx.actions,
        attr = ctx.attr,
        bin_dir = ctx.bin_dir,
        executable = ctx.executable,
        file = ctx.file,
        files = ctx.files,
        label = ctx.label,
        outputs = ctx.outputs,
        runfiles = ctx.runfiles,
        toolchains = ctx.toolchains,
        build_setting_value = ctx.build_setting_value,
        workspace_name = ctx.workspace_name,
        # Derived — used to be Rust stubs; now Starlark.
        build_file_path = ctx.label.package + "/BUILD.bazel",
        configuration = _configuration,
        coverage_instrumented = _coverage_instrumented,
        disabled_features = [],
        expand_location = _expand_location,
        expand_make_variables = _expand_make_variables,
        features = [],
        fragments = struct(),
        genfiles_dir = ctx.bin_dir,
        info_file = _empty_stamp_file,
        resolve_command = _resolve_command,
        target_platform_has_constraint = _target_platform_has_constraint,
        var = {},
        version_file = _empty_stamp_file,
    )

def invoke_rule(fn, ctx):
    """Engine entry point. Replaces `fn(ctx)` for every rule impl.

    Args:
        fn: The rule's `implementation` callable, captured at
            `rule(...)` definition time. A `FrozenValue` from the
            defining `.bzl`'s frozen module.
        ctx: The Rust-side `Ctx` value, freshly allocated for this
            target's analysis run. Wrapped in a Starlark struct that
            adds derived ctx fields; see `_wrap_rule_ctx`.

    Returns:
        Whatever the impl returns — typically a list of provider
        instances, but Loupe also tolerates a single provider
        (matching Bazel).
    """
    return fn(_wrap_rule_ctx(ctx))
