load("@bazel_tools//fragments:fragment_info.bzl", "FragmentInfo")
load(
    "//:exports.bzl",
    "PlatformInfo",
    "SymlinkEntry",
    "runfiles",
)

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

def _to_symlink_entry_depset(v):
    if type(v) != "depset":
        return depset([
            SymlinkEntry(
                path = path,
                target_file = target_file,
            )
            for path, target_file in v.items()
        ])
    return v

def _wrap_rule_ctx(ctx):
    def ctx_runfiles(files = [], transitive_files = None, collect_data = False, collect_default = False, symlinks = {}, root_symlinks = {}, skip_conflict_checking = False):
        direct = runfiles(
            files = depset(direct = files, transitive = [transitive_files] if transitive_files else []),
            symlinks = _to_symlink_entry_depset(symlinks),
            root_symlinks = _to_symlink_entry_depset(root_symlinks),
        )
        if collect_data or collect_default:
            # TODO: Implement this feature!
            pass
        return direct

    ctx_fields = {
        field: getattr(ctx, field)
        for field in dir(ctx)
    } | {
        "actions": _wrap_actions(ctx.actions, ctx.bin_dir, ctx.label),
        # build_file_path, genfiles_dir, features, disabled_features,
        # coverage_instrumented, resolve_command, expand_location are
        # native now (starlark_types/ctx.rs) — they come in through the
        # dir(ctx) comprehension / methods above rather than as Starlark
        # literals/closures here.
        "runfiles": ctx_runfiles,
        "workspace_name": "_main",
    }

    # If the rule depends on one or more fragments, an attribute with
    # name "__fragments" of type attr.label_list() is injected. The
    # default value of this attribute will refer to targets offering a
    # FragmentInfo. Make these available through ctx.configuration and
    # ctx.fragments.
    if hasattr(ctx.attr, "__configuration"):
        ctx_fields["configuration"] = ctx.attr.__configuration[FragmentInfo]
    _maybe_add_ctx_fragments(ctx_fields, getattr(ctx.attr, "__fragments", []))

    # If the rule has a default exec group, expose its toolchains
    # through ctx.toolchains.
    if "" in ctx.exec_groups:
        ctx_fields["toolchains"] = ctx.exec_groups[""].toolchains

    # `var` (make-variable dict) and `expand_make_variables` are native
    # now (starlark_types/ctx.rs) — the built-ins come from the real
    # Configuration via `ctx.compilation_mode`/`ctx.cpu` and the
    # toolchain `TemplateVariableInfo` merge is read natively through
    # `toolchain[TemplateVariableInfo]`.

    # Even though most rules depend on the target platform in an
    # indirect way (e.g., through toolchain resolution), only rules that
    # call ctx.target_platform_has_constraint() depend on the actual
    # definition of the target platform.
    if hasattr(ctx.attr, "__target_platform"):
        def ctx_target_platform_has_constraint(constraintValue):
            return ctx.attr.__target_platform[PlatformInfo].constraints.get(
                constraintValue.constraint.label,
                constraintValue.constraint.default_constraint_value,
            ) == constraintValue.label

        ctx_fields["target_platform_has_constraint"] = ctx_target_platform_has_constraint

    return struct(**ctx_fields)

def invoke_rule(fn, ctx):
    return fn(_wrap_rule_ctx(ctx))

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
