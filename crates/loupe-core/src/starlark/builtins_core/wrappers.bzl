# `FragmentInfo` is a native global now (starlark_types/provider.rs); the
# surviving subrule helper `_maybe_add_ctx_fragments` references it
# directly (no load). The rule context is fully native (no
# `_wrap_rule_ctx`).

def _wrap_actions(actions, bin_dir, label):
    def actions_declare_shareable_artifact(path, artifact_root = None):
        if artifact_root and artifact_root != bin_dir:
            fail("artifact_root %s is not equal to bin_dir %s, which is not supported by this implementation" % (artifact_root, bin_dir))

        expected_path_prefix = label.workspace_root + "/"
        if label.package:
            expected_path_prefix += label.package
            expected_path_prefix += "/"
        if not path.startswith(expected_path_prefix):
            fail("path %s does not start with %s, which is not supported by this implementation" % (path, expected_path_prefix))

        return actions.declare_file(path.removeprefix(expected_path_prefix))

    # `run_shell` is native now (starlark_types/actions.rs) — it lowers
    # to `run(executable="/bin/bash", …)` with Bazel's `$0`-slot argv
    # layering itself, so it comes through the dir(actions) copy below
    # rather than as a wrapper closure.
    actions_fields = {
        field: getattr(actions, field)
        for field in dir(actions)
    } | {
        "declare_shareable_artifact": actions_declare_shareable_artifact,
    }
    return struct(**actions_fields)

def _maybe_add_ctx_fragments(ctx_fields, fragments):
    if fragments:
        ctx_fields["fragments"] = struct(**{
            fragment.label.name: fragment[FragmentInfo]
            for fragment in fragments
        })


def invoke_rule(fn, ctx):
    # The rule context is native now (starlark_types/ctx.rs): the engine
    # passes the raw `ctx` straight to the rule impl, no `_wrap_rule_ctx`
    # struct. Every field the wrapper used to synthesise is provided by
    # the native `CtxGen` / `Actions` / `Runfiles` types:
    #   - values: build_file_path, genfiles_dir, features,
    #     disabled_features, compilation_mode, cpu, workspace_name
    #     ("_main"), version_file, info_file
    #   - algorithms: coverage_instrumented, resolve_command,
    #     expand_location, var, expand_make_variables
    #   - actions: run_shell (lowered to run=/bin/bash), declare_file,
    #     declare_shareable_artifact, …
    #   - runfiles: ctx.runfiles(...) -> native Runfiles (with merge /
    #     merge_all; .files is depset-shaped)
    #   - fragment/platform: configuration / fragments (off the
    #     __configuration / __fragments hidden attrs),
    #     target_platform_has_constraint
    return fn(ctx)

def _wrap_subrule_ctx(ctx, fragments):
    ctx_fields = {
        field: getattr(ctx, field)
        for field in dir(ctx)
    } | {
        "actions": _wrap_actions(ctx.actions, None, ctx.label),
    }

    _maybe_add_ctx_fragments(ctx_fields, fragments)

    return struct(**ctx_fields)

def invoke_subrule(fn, ctx, *args, __fragments = [], **kwargs):
    return fn(_wrap_subrule_ctx(ctx, __fragments), *args, **kwargs)
