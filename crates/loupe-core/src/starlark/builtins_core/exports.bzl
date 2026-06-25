AnalysisFailureInfo = provider()
AnalysisTestResultInfo = provider()
CcLauncherInfo = provider()
CcToolchainConfigInfo = provider()
CcToolchainInfo = provider()
CcSharedLibraryInfo = provider()
CcSharedLibraryHintInfo = provider()
ConfigSettingInfo = provider()
ConstraintSettingInfo = provider()
ConstraintValueInfo = provider()
DebugPackageInfo = provider()
DeclaredToolchainInfo = provider()
ExecutionInfo = provider()
FeatureFlagInfo = provider()
FilesToRunProvider = provider()
InstrumentedFilesInfo = provider()
JavaPluginInfo = provider()
OutputGroupInfo = provider(dict_like = True)
PackageSpecificationInfo = provider()
PlatformInfo = provider()
ProguardSpecProvider = provider()
ProtoInfo = provider()
PyInfo = provider()
StaticallyLinkedMarkerProvider = provider()
SymlinkEntry = provider()
ToolchainInfo = provider()
ToolchainTypeInfo = provider()

CcInfo = provider()

def _cc_native_library_info_init(*, libraries_to_link = None):
    return {
        "libraries_to_link": libraries_to_link or depset(),
    }

CcNativeLibraryInfo, _CcNativeLibraryInfoRaw = provider(init = _cc_native_library_info_init)

# `runfiles` is the native Rust provider now (starlark_types/runfiles.rs):
# `runfiles(files=…, symlinks=…, root_symlinks=…)` returns the native
# `Runfiles` (native `merge`/`merge_all`, `.files` depset-shaped).
# `ctx.runfiles(...)` and `DefaultInfo.{default,data}_runfiles` produce
# the same type, so `merge_all([...])` is single-typed. `_runfiles` is
# the empty-runfiles factory `_default_info_init` calls.
_runfiles = runfiles

def _default_info_init(*, data_runfiles = None, default_runfiles = None, executable = None, files = None, runfiles = None):
    # According to the Bazel documentation, only the runfiles parameter
    # should be used. Calling DefaultInfo() with data_runfiles or
    # default_runfiles is deprecated. In this implementation we simply
    # merge all of the runfiles together.
    merged_runfiles = (runfiles or _runfiles()).merge_all(
        ([data_runfiles] if data_runfiles else []) +
        ([default_runfiles] if default_runfiles else []),
    )
    return {
        "files": files or depset(),
        "files_to_run": FilesToRunProvider(
            # Copy fields instead of embedding the runfiles object into
            # FilesToRunProvider. This reduces the size of DefaultInfo
            # significantly.
            _runfiles_files = merged_runfiles.files,
            _runfiles_symlinks = merged_runfiles.symlinks,
            _runfiles_root_symlinks = merged_runfiles.root_symlinks,
            executable = executable,
            repo_mapping_manifest = None,
            runfiles_manifest = None,
        ),
    }

def _default_info_runfiles(r):
    # There is no point in storing the runfiles both in DefaultInfo and
    # the FilesToRunProvider contained within. Simply let
    # DefaultInfo.{data,default}_runfiles return the runfiles contained
    # in the FilesToRunProvider.
    return runfiles(
        files = r.files_to_run._runfiles_files,
        symlinks = r.files_to_run._runfiles_symlinks,
        root_symlinks = r.files_to_run._runfiles_root_symlinks,
    )

DefaultInfo, _DefaultInfoRaw = provider(
    computed_fields = {
        "data_runfiles": _default_info_runfiles,
        "default_runfiles": _default_info_runfiles,
    },
    init = _default_info_init,
)

def _java_info_init(
        output_jar,
        compile_jar,
        source_jar = None,
        compile_jdeps = None,
        generated_class_jar = None,
        generated_source_jar = None,
        native_headers_jar = None,
        manifest_proto = None,
        neverlink = False,
        deps = [],
        runtime_deps = [],
        exports = [],
        exported_plugins = [],
        jdeps = None,
        native_libraries = []):
    return {}

JavaInfo, _JavaInfoRaw = provider(
    init = _java_info_init,
)

def _run_environment_info_init(environment = {}, inherited_environment = []):
    return {
        "environment": environment,
        "inherited_environment": inherited_environment,
    }

RunEnvironmentInfo, _RunEnvironmentInfoRaw = provider(init = _run_environment_info_init)

def _template_variable_info_init(variables):
    return {"variables": variables}

TemplateVariableInfo, _TemplateVariableInfoRaw = provider(init = _template_variable_info_init)

def _cc_libc_top_alias_impl(ctx):
    fail("TODO")

cc_libc_top_alias = rule(
    implementation = _cc_libc_top_alias_impl,
    needs = [],
)

def _cc_proto_library_impl(ctx):
    fail("TODO")

cc_proto_library = rule(
    implementation = _cc_proto_library_impl,
    attrs = {
        "deps": attr.label_list(),
    },
    needs = [],
)

def cc_toolchain_suite(**kwargs):
    pass

# Minimal cc_binary / cc_library / cc_test. Loupe doesn't run cc compilation,
# but BUILD files (e.g. @@bazel_tools//src/tools/launcher, which executable
# rules pull in as their launcher) use these native rules, so they must
# resolve as bare globals AND as `native.cc_*` (via exported_rules) or the
# whole package fails to load. cc_binary/cc_test emit a placeholder
# executable File so dependents get a runnable path; cc_library is a no-op
# DefaultInfo. No real toolchain/action graph — analysis never executes it.
# Grow attrs/providers from probe feedback if a real cc path needs more.
_CC_RULE_ATTRS = {
    "srcs": attr.label_list(allow_files = True),
    "deps": attr.label_list(),
    "data": attr.label_list(allow_files = True),
    "hdrs": attr.label_list(allow_files = True),
    "textual_hdrs": attr.label_list(allow_files = True),
    "copts": attr.string_list(),
    "conlyopts": attr.string_list(),
    "cxxopts": attr.string_list(),
    "linkopts": attr.string_list(),
    "defines": attr.string_list(),
    "local_defines": attr.string_list(),
    "includes": attr.string_list(),
    "strip_include_prefix": attr.string(),
    "include_prefix": attr.string(),
    "implementation_deps": attr.label_list(),
    "alwayslink": attr.bool(),
    "linkstatic": attr.bool(),
    "linkshared": attr.bool(),
    "additional_linker_inputs": attr.label_list(allow_files = True),
    "additional_compiler_inputs": attr.label_list(allow_files = True),
    "win_def_file": attr.label(allow_files = True),
    "malloc": attr.label(),
}

def _cc_executable_impl(ctx):
    exe = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.write(exe, "", is_executable = True)
    return [DefaultInfo(files = depset([exe]), executable = exe)]

cc_binary = rule(
    implementation = _cc_executable_impl,
    executable = True,
    attrs = _CC_RULE_ATTRS,
    needs = [],
)

def _cc_library_impl(ctx):
    return [DefaultInfo()]

cc_library = rule(
    implementation = _cc_library_impl,
    attrs = _CC_RULE_ATTRS,
    needs = [],
)

cc_test = rule(
    implementation = _cc_executable_impl,
    executable = True,
    test = True,
    attrs = _CC_RULE_ATTRS,
    needs = [],
)

def _get_effective_constraint_value(constraint_setting, value_label):
    if value_label == constraint_setting.default_constraint_value:
        # Constraint value is equal to its default value, meaning it
        # should not be encoded explicitly as part of the configuration.
        # Require that the constraint setting is absent.
        return None
    return value_label

def _config_setting_impl(ctx):
    return [ConfigSettingInfo(
        # Convert constraint values to a dictionary of constraint
        # settings to values. If the provided constraint value is the
        # default we set it to None, because it effectively means the
        # constraint setting should not be part of the configuration.
        constraints = {
            constraint_value[ConstraintValueInfo].constraint.label: _get_effective_constraint_value(
                constraint_value[ConstraintValueInfo].constraint,
                constraint_value[ConstraintValueInfo].label,
            )
            for constraint_value in ctx.attr.constraint_values
        },
        flag_values = {
            key.original_label: value
            for key, value in ctx.attr.flag_values.items()
        },
    )]

def _config_setting_init(**kwargs):
    # The "values" attr can be used to refer to command line options
    # that are integrated into Bazel. In our case we declare all of them
    # as build settings under @bazel_tools//command_line_option. This
    # allows us to simply remap "values" to "flag_values".
    kwargs["flag_values"] = kwargs.get("flag_values", {}) | {
        "@bazel_tools//command_line_option:" + option: value
        for option, value in kwargs.get("values", {}).items()
    }
    return kwargs

config_setting = rule(
    implementation = _config_setting_impl,
    attrs = {
        "constraint_values": attr.label_list(
            cfg = config.none(),
            providers = [ConstraintValueInfo],
        ),
        # TODO: Do we even want to support define_values?
        "define_values": attr.string_dict(),
        "flag_values": attr.label_keyed_string_dict(
            # We are only interested in obtaining the build setting
            # values, which doesn't require these targets to be
            # configured.
            cfg = config.unconfigured(),
        ),
        "values": attr.string_dict(),
    },
    initializer = _config_setting_init,
    needs = [],
    provides = [ConfigSettingInfo],
)

def configuration_field(fragment, name):
    # Don't provide actual support for late-bound defaults. Instead map
    # each of them to the respective command line option used by Bazel.
    if fragment == "apple":
        if name == "xcode_config_label":
            return Label("@bazel_tools//command_line_option:xcode_version_config")
    if fragment == "bazel_py":
        if name == "python_top":
            return Label("@bazel_tools//command_line_option:python_top")
    if fragment == "coverage":
        if name == "output_generator":
            # This configuration field should not map to lcov_merger if
            # coverage is disabled, as that would cause cyclic
            # dependencies otherwise. Let this map to an alias that only
            # points to lcov_merger if --collect_code_coverage is set.
            return Label("@bazel_tools//tools/coverage:coverage_output_generator")
    if fragment == "cpp":
        if name == "cs_fdo_profile":
            return Label("@bazel_tools//command_line_option:cs_fdo_profile")
        if name == "custom_malloc":
            return Label("@bazel_tools//command_line_option:custom_malloc")
        if name == "fdo_optimize":
            return Label("@bazel_tools//command_line_option:fdo_optimize")
        if name == "fdo_prefetch_hints":
            return Label("@bazel_tools//command_line_option:fdo_prefetch_hints")
        if name == "fdo_profile":
            return Label("@bazel_tools//command_line_option:fdo_profile")
        if name == "libc_top":
            return Label("@bazel_tools//command_line_option:grte_top")
        if name == "memprof_profile":
            return Label("@bazel_tools//command_line_option:memprof_profile")
        if name == "propeller_optimize":
            return Label("@bazel_tools//command_line_option:propeller_optimize")
        if name == "proto_profile_path":
            return Label("@bazel_tools//command_line_option:proto_profile_path")
        if name == "target_libc_top_DO_NOT_USE_ONLY_FOR_CC_TOOLCHAIN":
            return None
        if name == "xbinary_fdo":
            return Label("@bazel_tools//command_line_option:xbinary_fdo")
        if name == "zipper":
            return None
    if fragment == "java":
        if name == "launcher":
            return Label("@bazel_tools//command_line_option:java_launcher")
        if name == "java_toolchain_bytecode_optimizer":
            return Label("@bazel_tools//command_line_option:proguard_top")
        if name == "local_java_optimization_configuration":
            return Label("@bazel_tools//command_line_option:experimental_local_java_optimization_configuration")
    if fragment == "proto":
        if name == "proto_compiler":
            return Label("@bazel_tools//command_line_option:proto_compiler")
        if name == "proto_toolchain_for_cc":
            return Label("@bazel_tools//command_line_option:proto_toolchain_for_cc")
        if name == "proto_toolchain_for_java":
            return Label("@bazel_tools//command_line_option:proto_toolchain_for_java")
        if name == "proto_toolchain_for_java_lite":
            return Label("@bazel_tools//command_line_option:proto_toolchain_for_javalite")
    if fragment == "py":
        if name == "native_rules_allowlist":
            return Label("@bazel_tools//command_line_option:python_native_rules_allowlist")

    fail("this implementation of configuration_field() does not support fragment %s and name %s" % (fragment, name))

def _constraint_setting_impl(ctx):
    default_constraint_value = ctx.attr.default_constraint_value
    return [ConstraintSettingInfo(
        default_constraint_value = default_constraint_value.original_label if default_constraint_value else None,
        has_default_constraint_value = bool(default_constraint_value),
        label = ctx.label,
    )]

constraint_setting = rule(
    implementation = _constraint_setting_impl,
    attrs = {
        "default_constraint_value": attr.label(
            # Prevent cyclic dependency between the constraint_setting()
            # and the default constraint_value().
            cfg = config.unconfigured(),
            providers = [ConstraintValueInfo],
        ),
    },
    needs = [],
    provides = [ConstraintSettingInfo],
)

def _constraint_value_impl(ctx):
    constraint_setting = ctx.attr.constraint_setting[ConstraintSettingInfo]
    return [
        # Also provide a ConfigSettingInfo containing just this
        # constraint. This allows constraint values to be passed to
        # select() directly.
        ConfigSettingInfo(
            constraints = {
                constraint_setting.label: _get_effective_constraint_value(constraint_setting, ctx.label),
            },
            flag_values = {},
        ),
        ConstraintValueInfo(
            constraint = constraint_setting,
            label = ctx.label,
        ),
    ]

constraint_value = rule(
    implementation = _constraint_value_impl,
    attrs = {
        "constraint_setting": attr.label(
            mandatory = True,
            providers = [ConstraintSettingInfo],
        ),
    },
    needs = [],
    provides = [ConfigSettingInfo, ConstraintValueInfo],
)

def _filegroup_impl(ctx):
    files = []
    runfiles = []
    if ctx.attr.output_group:
        for src in ctx.attr.srcs:
            # Bazel's filegroup pulls the named output group from each
            # src's OutputGroupInfo; a src that doesn't offer the group
            # (or no OutputGroupInfo at all) contributes nothing rather
            # than erroring.
            if OutputGroupInfo in src:
                group = getattr(src[OutputGroupInfo], ctx.attr.output_group, None)
                if group != None:
                    files.append(group)
    elif len(ctx.attr.srcs) == 1:
        # If exactly one target is provided, return the original
        # DefaultInfo. This ensures that fields like files_to_run are
        # preserved.
        return ctx.attr.srcs[0][DefaultInfo]
    else:
        for src in ctx.attr.srcs:
            default_info = src[DefaultInfo]
            files.append(default_info.files)
            runfiles.append(default_info.default_runfiles)

    for data in ctx.attr.data:
        runfiles.append(data[DefaultInfo].default_runfiles)

    return [DefaultInfo(
        files = depset(direct = [], transitive = files),
        runfiles = ctx.runfiles(
            files = ctx.files.data,
        ).merge_all(runfiles),
    )]

filegroup = rule(
    implementation = _filegroup_impl,
    attrs = {
        "data": attr.label_list(allow_files = True),
        "output_group": attr.string(),
        "srcs": attr.label_list(allow_files = True),
    },
    needs = [],
)

def _genrule_impl(ctx):
    # Determine Make variables specific to genrule().
    outs = [out.path for out in ctx.outputs.outs]
    ruledir = "/".join([
        part
        for part in [ctx.bin_dir.path, ctx.label.workspace_root, ctx.label.package]
        if part
    ])
    srcs = [src.path for src in ctx.files.srcs]
    additional_substitutions = {
        "@D": ctx.outputs.outs[0].path.rsplit("/", 1)[0] if len(ctx.outputs.outs) == 1 else ruledir,
        "OUTS": " ".join(outs),
        "RULEDIR": ruledir,
        "SRCS": " ".join(srcs),
    }
    if len(outs) == 1:
        additional_substitutions["@"] = outs[0]
    if len(srcs) == 1:
        additional_substitutions["<"] = srcs[0]

    ctx.actions.run(
        executable = "/bin/bash",
        arguments = [
            "-c",
            ("source %s; " % ctx.file._genrule_setup.path) +
            ctx.expand_make_variables(
                "cmd",
                ctx.expand_location(
                    ctx.attr.cmd_bash or ctx.attr.cmd,
                    [
                        target
                        for attr in [ctx.attr.srcs, ctx.attr.tools]
                        for target in attr
                    ],
                ),
                additional_substitutions,
            ),
        ],
        inputs = [ctx.file._genrule_setup] + ctx.files.srcs,
        tools = [tool.files_to_run for tool in ctx.attr.tools],
        outputs = ctx.outputs.outs,
    )
    return [DefaultInfo(files = depset(ctx.outputs.outs))]

genrule = rule(
    implementation = _genrule_impl,
    attrs = {
        "cmd": attr.string(),
        "cmd_bash": attr.string(),
        "cmd_bat": attr.string(),
        "cmd_ps": attr.string(),
        "executable": attr.bool(),
        "local": attr.bool(),
        "message": attr.string(),
        "output_licenses": attr.string_list(),
        "output_to_bindir": attr.bool(),
        "outs": attr.output_list(mandatory = True),
        "srcs": attr.label_list(allow_files = True),
        "tools": attr.label_list(allow_files = True, cfg = "exec"),
        "_genrule_setup": attr.label(
            allow_single_file = True,
            default = "@bazel_tools//tools/genrule:genrule-setup.sh",
        ),
    },
    needs = [
        "default_exec_group",
        "make_variables",
    ],
)

def _java_plugins_flag_alias_impl(ctx):
    return [JavaPluginInfo()]

java_plugins_flag_alias = rule(
    implementation = _java_plugins_flag_alias_impl,
    needs = [],
)

def _java_proto_library_impl(ctx):
    fail("TODO")

java_proto_library = rule(
    implementation = _java_proto_library_impl,
    attrs = {
        "deps": attr.label_list(),
    },
    needs = [],
)

def licenses(license_types):
    # This function is deprecated. Licenses can nowadays be attached in
    # the form of metadata. Provide a no-op stub.
    pass

def _platform_impl(ctx):
    # Convert all constraint values to a dict mapping the constraint
    # setting to the corresponding value.
    constraints = {}
    for value in ctx.attr.constraint_values:
        value_info = value[ConstraintValueInfo]
        setting_label = value_info.constraint.label
        value_label = value_info.label
        if setting_label in constraints:
            fail("constraint_values contains multiple values for constraint setting %s: %s and %s" % (
                setting_label,
                constraints[setting_label],
                value_label,
            ))
        constraints[setting_label] = _get_effective_constraint_value(value_info.constraint, value_label)

    exec_pkix_public_key = ctx.attr.exec_pkix_public_key
    repository_os_arch = ctx.attr.repository_os_arch
    repository_os_environ = ctx.attr.repository_os_environ
    repository_os_name = ctx.attr.repository_os_name

    # Inherit properties from the parent platform.
    if ctx.attr.parents:
        if len(ctx.attr.parents) != 1:
            fail("providing multiple parents is not supported")
        parent = ctx.attr.parents[0][PlatformInfo]
        constraints = parent.constraints | constraints
        exec_pkix_public_key = exec_pkix_public_key or parent.exec_pkix_public_key
        repository_os_arch = repository_os_arch or parent.repository_os_arch
        repository_os_environ = repository_os_environ or parent.repository_os_environ
        repository_os_name = repository_os_name or parent.repository_os_name

    return [PlatformInfo(
        constraints = {
            setting: value
            for setting, value in constraints.items()
            if value
        },
        exec_pkix_public_key = exec_pkix_public_key,
        repository_os_arch = repository_os_arch,
        repository_os_environ = repository_os_environ,
        repository_os_name = repository_os_name,
    )]

platform = rule(
    implementation = _platform_impl,
    attrs = {
        "constraint_values": attr.label_list(
            doc = """
            The combination of constraint choices that this platform
            comprises. In order for a platform to apply to a given
            environment, the environment must have at least the values
            in this list.

            Each constraint_value in this list must be for a different
            constraint_setting. For example, you cannot define a
            platform that requires the cpu architecture to be both
            @platforms//cpu:x86_64 and @platforms//cpu:arm.
            """,
            providers = [ConstraintValueInfo],
        ),
        "exec_pkix_public_key": attr.string(
            doc = """
            When the platform is used for execution, the elliptic-curve
            public key in PKIX form that identifies the execution
            platform. The key needs to be provided in base64 encoded
            form, without the PEM header/footer.
            """,
        ),
        "parents": attr.label_list(
            doc = """
            The label of a platform target that this platform should
            inherit from. Although the attribute takes a list, there
            should be no more than one platform present. Any
            constraint_settings not set directly on this platform will
            be found in the parent platform. See the section on Platform
            Inheritance for details.
            """,
            providers = [PlatformInfo],
        ),
        "repository_os_arch": attr.string(
            doc = """
            If this platform is used as a platform for executing
            commands as part of module extensions or repository rules,
            the name of the architecture to announce via
            repository_os.arch.

            This attribute should match the value of the "os.arch" Java
            property converted to lower case (e.g., "aarch64" for ARM64,
            "amd64" for x86-64, "x86" for x86-32).
            """,
        ),
        "repository_os_environ": attr.string_dict(
            doc = """
            If this platform is used as a platform for executing
            commands as part of module extensions or repository rules,
            environment variables to announce via repository_os.environ.
            """,
        ),
        "repository_os_name": attr.string(
            doc = """
            If this platform is used as a platform for executing
            commands as part of module extensions or repository rules,
            the operating system name to announce via
            repository_os.name.

            This attribute should match the value of the "os.name" Java
            property converted to lower case (e.g., "linux", "mac os x",
            "windows 10").
            """,
        ),
    },
    needs = [],
    provides = [PlatformInfo],
)

def _starlark_doc_extract_impl(ctx):
    # Bazel's native `starlark_doc_extract` runs a Java extractor over
    # `src` (+ `deps`) and emits a `ModuleInfo` proto: `<name>.binaryproto`
    # is the default output, `<name>.textproto` is the `textproto`
    # output group. Loupe records actions rather than executing them, so
    # model the extraction as writes producing those outputs — the
    # provider/output shape is what cquery consumers read.
    binaryproto = ctx.actions.declare_file(ctx.label.name + ".binaryproto")
    textproto = ctx.actions.declare_file(ctx.label.name + ".textproto")
    ctx.actions.write(binaryproto, "")
    ctx.actions.write(textproto, "")
    return [
        DefaultInfo(files = depset([binaryproto])),
        OutputGroupInfo(textproto = depset([textproto])),
    ]

starlark_doc_extract = rule(
    _starlark_doc_extract_impl,
    attrs = {
        "deps": attr.label_list(),
        "src": attr.label(mandatory = True, allow_files = True),
        "symbol_names": attr.string_list(),
        "render_main_repo_name": attr.bool(),
    },
    needs = [],
)

def _test_suite_impl(ctx):
    # `test_suite` is a pure grouping rule: it declares no outputs and
    # runs no actions. Bazel surfaces it with an empty DefaultInfo (the
    # `tests`/`$implicit_tests` membership drives test expansion, which
    # is an execution-phase concern Loupe doesn't model). Match that.
    return [DefaultInfo()]

test_suite = rule(
    _test_suite_impl,
    attrs = {
        "tests": attr.string_list(),
    },
    needs = [],
)

def _toolchain_impl(ctx):
    return [DeclaredToolchainInfo(
        target_settings = [
            target_setting.label
            for target_setting in ctx.attr.target_settings
        ],
        toolchain = ctx.attr.toolchain.original_label,
        toolchain_type = ctx.attr.toolchain_type[ToolchainTypeInfo].type_label,
    )]

toolchain = rule(
    implementation = _toolchain_impl,
    attrs = {
        "target_settings": attr.label_list(
            providers = [ConfigSettingInfo],
        ),
        "toolchain": attr.label(
            # Prevent configuring toolchains that are not used.
            cfg = config.unconfigured(),
            mandatory = True,
            providers = [ToolchainInfo],
        ),
        "toolchain_type": attr.label(
            mandatory = True,
            providers = [ToolchainTypeInfo],
        ),
        # Bazel 8+ added `use_target_platform_constraints` to copy
        # `target_compatible_with` from the platform. Accept-and-
        # ignore in v1 — toolchain resolution uses
        # `target_compatible_with`/`exec_compatible_with` directly.
        "use_target_platform_constraints": attr.bool(),
    },
    needs = [],
    provides = [DeclaredToolchainInfo],
)

def _toolchain_type_impl(ctx):
    return [ToolchainTypeInfo(
        type_label = ctx.label,
    )]

toolchain_type = rule(
    implementation = _toolchain_type_impl,
    needs = [],
    provides = [ToolchainTypeInfo],
)

def coverage_common_instrumented_files_info(
        ctx,
        *,
        coverage_environment = {},
        coverage_support_files = [],
        dependency_attributes = [],
        extensions = None,
        metadata_files = [],
        reported_to_actual_sources = None,
        source_attributes = []):
    return InstrumentedFilesInfo(
        # TODO: instrumented_files.
        metadata_files = depset(metadata_files),
    )

def proto_common_do_not_use_external_proto_infos():
    return []

def proto_common_do_not_use_incompatible_enable_proto_toolchain_resolution():
    # Loupe always enables proto toolchain resolution (Bazel's
    # `--incompatible_enable_proto_toolchain_resolution`): proto rules
    # resolve `proto_toolchain_type` via `ctx.toolchains[...]` rather
    # than the legacy `--proto_compiler` flag. Mirrored by the uppercase
    # `INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION` constant on the
    # `proto_common_do_not_use` toplevel, which is what protobuf's proto
    # rules read (`getattr(proto_common, "INCOMPATIBLE_…", False)`).
    return True

def builtins_json_encode_indent(x, **kwargs):
    return json.indent(json.encode(x), **kwargs)

def _config_common_toolchain_type(name = None, mandatory = True):
    # `config_common.toolchain_type(name, mandatory=…)` declares a
    # toolchain-type dependency for `rule(toolchains=[…])`. Return the
    # Bazel-shaped requirement struct. `rule()` extracts toolchain-type
    # labels via `label_arg_as_str`, which reads strings / Label values
    # only — so this struct is dropped from the resolved toolchains list
    # (matching the pre-removal behaviour, where the wildcard yielded no
    # label) while still exposing `.toolchain_type` / `.mandatory` for
    # any ruleset that reads them. rules_shell's `sh_executable.bzl`
    # calls this at load time, so it must return rather than fail.
    return struct(toolchain_type = name, mandatory = mandatory)

# `native.<rule>`-valued entries (`alias`, `glob`, `exports_files`,
# `label_flag`, `label_setting`, `package_group`) are intentionally
# absent. During embedded eval `native` is not a real binding for
# those, and they were always filtered out downstream; the real
# versions reach user code from Rust — `register_for_build` (bare
# BUILD globals) and `bind_native_struct`'s `rust_native_extras`
# (`native.*` for macros). Listing them here only re-introduced an
# eval-time `native.X` read into this otherwise self-contained file.
exported_rules = {
    "cc_binary": cc_binary,
    "cc_libc_top_alias": cc_libc_top_alias,
    "cc_library": cc_library,
    "cc_proto_library": cc_proto_library,
    "cc_test": cc_test,
    "cc_toolchain_suite": cc_toolchain_suite,
    "config_setting": config_setting,
    "constraint_setting": constraint_setting,
    "constraint_value": constraint_value,
    "filegroup": filegroup,
    "genrule": genrule,
    "java_plugins_flag_alias": java_plugins_flag_alias,
    "java_proto_library": java_proto_library,
    "licenses": licenses,
    "platform": platform,
    "starlark_doc_extract": starlark_doc_extract,
    "test_suite": test_suite,
    "toolchain": toolchain,
    "toolchain_type": toolchain_type,
}
def _macro_stub(*args, **kwargs):
    """Stub for Bazel 8+ symbolic `macro(implementation=..., attrs=...)`.

    Loupe doesn't model symbolic macros yet; vendored `.bzl` files
    (e.g. `bazel_features`'s generated `globals.bzl`) reference the
    global `macro` symbol as a fallback to `getattr(legacy_globals,
    'macro', macro)` even when they never instantiate one. Binding a
    no-op constructor is enough to satisfy name resolution. Returns a
    callable that swallows its kwargs so any opportunistic invocation
    stays quiet.
    """
    _ = (args, kwargs)

    def _instance(*a, **kw):
        _ = (a, kw)
        return None

    return _instance

# `cc_internal` is Bazel's native cc helper (Java `CcStarlarkInternal`).
# rules_cc resolves it as `cc_internal = cc_common.internal_DO_NOT_USE()`
# (rules_cc `cc/private/cc_internal.bzl`); when that method is absent it
# falls back to an empty `struct()`, which is why CcInfo construction hit
# "no attribute create_header_info". Loupe doesn't run cc compilation, so
# model only the slice rules_cc's CcInfo / CcCompilationContext path calls
# during analysis — `create_header_info`, `create_header_info_with_deps`,
# `freeze` — mirroring `CcStarlarkInternal`. Grow from probe feedback,
# never to the full native surface; unknown members raise (real-or-fail).
def _cc_internal_create_header_info(
        *,
        header_module = None,
        pic_header_module = None,
        modular_public_headers = [],
        modular_private_headers = [],
        textual_headers = [],
        separate_module_headers = [],
        separate_module = None,
        separate_pic_module = None):
    # Bazel's HeaderInfo; cc_info.bzl reads back these eight fields (four
    # header lists, four nullable module artifacts). Recording the inputs
    # is enough for analysis.
    return struct(
        header_module = header_module,
        pic_header_module = pic_header_module,
        modular_public_headers = modular_public_headers,
        modular_private_headers = modular_private_headers,
        textual_headers = textual_headers,
        separate_module_headers = separate_module_headers,
        separate_module = separate_module,
        separate_pic_module = separate_pic_module,
    )

def _cc_internal_create_header_info_with_deps(*, header_info, deps, merged_deps):
    # Bazel folds dep header infos in; cc_info.bzl reads only the returned
    # object's own eight fields (deps / merged_deps aren't read back), so
    # pass this node's header_info through unchanged.
    _ = (deps, merged_deps)
    return header_info

def _cc_internal_freeze(value):
    # Bazel returns an immutable copy: dict -> dict, iterable -> list, else
    # the value itself. A shallow copy / passthrough suffices for analysis.
    if type(value) == "dict":
        return dict(value)
    if type(value) == "list":
        return list(value)
    return value

# Members rules_cc's cc_common.bzl reads off the internal cc objects (both
# `cc_internal` and the captured native `cc_common`) as function references
# when building its public `cc_common` struct at load. Loupe doesn't run cc
# actions, so targets that only define toolchains/rules never *call* these —
# model them as loud-fail stubs (real-or-fail): referencing them succeeds (the
# struct builds), but an actual invocation fails with the member name so a
# genuinely-needed one surfaces in the probe instead of fabricating a value.
def _native_unimplemented(name):
    def _stub(*args, **kwargs):
        _ = (args, kwargs)
        fail("Loupe does not model native member '%s' " % name +
             "(analysis does not run cc/objc actions); add it to exports.bzl if a real path needs it")

    return _stub

def _cc_internal_check_private_api(*args, **kwargs):
    # Bazel's privacy guard: fails unless the calling .bzl is on the
    # private-Starlarkification allowlist. Every caller here is a trusted
    # vendored builtin (allowlisted in real Bazel) and Loupe runs no cc
    # actions, so the check is a no-op. cc_info.bzl's `merge_debug_context`
    # calls it on a path py_library reaches via `cc_common.merge_cc_infos`,
    # so it must pass rather than fail.
    _ = (args, kwargs)
    return None

_cc_internal = struct(
    create_header_info = _cc_internal_create_header_info,
    create_header_info_with_deps = _cc_internal_create_header_info_with_deps,
    freeze = _cc_internal_freeze,
    # Real no-op (privacy guard) — see above; safe to pass for analysis.
    check_private_api = _cc_internal_check_private_api,
    # Function refs read by cc_common.bzl at load; loud-fail if ever called.
    get_artifact_name_for_category = _native_unimplemented("cc_internal.get_artifact_name_for_category"),
    absolute_symlink = _native_unimplemented("cc_internal.absolute_symlink"),
    expand_and_tokenize = _native_unimplemented("cc_internal.expand_and_tokenize"),
    cc_toolchain_features = _native_unimplemented("cc_internal.cc_toolchain_features"),
    solib_symlink_action = _native_unimplemented("cc_internal.solib_symlink_action"),
    cc_toolchain_variables = _native_unimplemented("cc_internal.cc_toolchain_variables"),
)

def _cc_common_internal_do_not_use():
    return _cc_internal

# `apple_common` is a Bazel native top-level global. rules_cc's
# objc_common.bzl does `_apple_toolchain = apple_common.apple_toolchain()` at
# load, so the name must resolve and `apple_toolchain()` must return an object
# — but nothing is read off it at load. `_apple_toolchain.sdk_dir()` is only
# reached when an objc rule actually runs (which JS targets never do), so it's
# a loud-fail stub (real-or-fail). Adding apple_common clears the load failure
# without modelling any objc algorithm.
_apple_toolchain_value = struct(
    sdk_dir = _native_unimplemented("apple_common.apple_toolchain().sdk_dir"),
)

def _apple_common_apple_toolchain():
    return _apple_toolchain_value

# apple_common.XcodeVersionConfig is captured at load by rules_cc's
# objc_compilation_support.bzl (`XcodeVersionInfo = apple_common.XcodeVersionConfig`)
# and only indexed inside a rule impl later (`ctx.attr._xcode_config[...]`). A real
# provider() is the right minimal shape — a usable provider key and a value that
# exists at load. This is the second (and, per the load-chain scan, last) apple_common
# member objc needs at load.
_XcodeVersionConfig = provider()

_apple_common = struct(
    apple_toolchain = _apple_common_apple_toolchain,
    XcodeVersionConfig = _XcodeVersionConfig,
)

# `java_common` is a Bazel native top-level global. rules_java's
# java/private/native.bzl captures it (`native_java_common = java_common`) and
# `get_internal_java_common()` returns `java_common.internal_DO_NOT_USE()` (or
# None when absent). At module load `_make_java_common()` calls
# `get_internal_java_common().google_legacy_api_enabled()` — the only internal
# member touched at load. Return False (Bazel's default; the True branch pulls
# unmodeled cc-merge paths into JavaInfo). Mirrors the cc_common.internal_DO_NOT_USE
# analog; every other internal member is reached only inside Java rule impls
# (never on a rules_js path), so loud-fail stubs.
def _java_internal_false():
    # The two internal flags rules_java reads at module load
    # (google_legacy_api_enabled @ java_common.bzl:313;
    # incompatible_disable_non_executable_java_binary @ java_binary.bzl:326)
    # both default off in Bazel. False keeps the fuller, non-cc-dependent
    # rule/JavaInfo shape (the True branches either drop attrs or pull cc-merge).
    return False

_java_internal = struct(
    google_legacy_api_enabled = _java_internal_false,
    incompatible_disable_non_executable_java_binary = _java_internal_false,
    # Reached only inside Java rule impls (never at load / on a rules_js path):
    wrap_java_info = _native_unimplemented("java_common_internal.wrap_java_info"),
    check_provider_instances = _native_unimplemented("java_common_internal.check_provider_instances"),
    check_java_toolchain_is_declared_on_rule = _native_unimplemented("java_common_internal.check_java_toolchain_is_declared_on_rule"),
    create_compilation_action = _native_unimplemented("java_common_internal.create_compilation_action"),
    create_header_compilation_action = _native_unimplemented("java_common_internal.create_header_compilation_action"),
    expand_java_opts = _native_unimplemented("java_common_internal.expand_java_opts"),
    target_kind = _native_unimplemented("java_common_internal.target_kind"),
    collect_native_deps_dirs = _native_unimplemented("java_common_internal.collect_native_deps_dirs"),
    get_runtime_classpath_for_archive = _native_unimplemented("java_common_internal.get_runtime_classpath_for_archive"),
    incompatible_java_info_merge_runtime_module_flags = _native_unimplemented("java_common_internal.incompatible_java_info_merge_runtime_module_flags"),
)

def _java_common_internal_do_not_use():
    return _java_internal

def _py_internal_get_label_repo_runfiles_path(label):
    # Bazel's `py_builtins.get_label_repo_runfiles_path(label)`: the
    # runfiles-root-relative path of the label's repository. Derived
    # straight from the label (`workspace_root` is the canonical repo —
    # "" for the main repo), so this is a real label→path computation,
    # not a fabricated value. py_library's `get_imports` joins it into
    # `PyInfo.imports`; analysis is read-only so the exact runfiles
    # prefix is inert, but the call must return rather than loud-fail.
    # Grown from probe feedback, the same way `_cc_internal` is.
    return label.workspace_root

def _py_internal_are_action_listeners_enabled(*args, **kwargs):
    # Bazel's `py_builtins.are_action_listeners_enabled(ctx)`: True only
    # when `--experimental_action_listener` is set. `create_py_info`
    # gates `add_py_extra_pseudo_action` on it; analysis is read-only and
    # registers no action listeners, so it's always False.
    return False

_SHARED_LIBRARY_EXTENSIONS = ["so", "dll", "dylib", "wasm"]

def _cc_helper_is_versioned_shared_library_extension_valid(shared_library_name):
    # Mirrors cc_helper_internal.bzl: a versioned shared library matches
    # `^.+\.((so)|(dylib))(\.\d\w*)+$` (e.g. `libfoo.so.1.2`).
    for ext in (".so.", ".dylib."):
        name, _, version = shared_library_name.rpartition(ext)
        if name and version:
            for part in version.split("."):
                if not part[0].isdigit():
                    return False
                for c in part[1:].elems():
                    if not (c.isalnum() or c == "_"):
                        return False
            return True
    return False

def _cc_helper_is_valid_shared_library_artifact(shared_library):
    if shared_library.extension in _SHARED_LIBRARY_EXTENSIONS:
        return True
    return _cc_helper_is_versioned_shared_library_extension_valid(shared_library.basename)

# Minimal `cc_helper` for the `py_internal` bridge. The canonical Bazel
# `py_internal` re-exports the full `cc_helper` struct; exports.bzl can't
# `load()` the vendored one, so we model the members `py_library`'s
# `create_py_info` actually reaches. `is_valid_shared_library_artifact`
# classifies a dep's library files to set `PyInfo.uses_shared_libraries`;
# its logic is copied verbatim from `common/cc/cc_helper.bzl`.
_cc_helper = struct(
    is_valid_shared_library_artifact = _cc_helper_is_valid_shared_library_artifact,
)

# `py_internal` is Bazel's internal Python helper struct. Members are
# modelled on demand (like `_cc_internal`); everything unmodelled stays
# absent and raises. rules_python's `common_bazel.bzl` reaches
# `get_label_repo_runfiles_path` from `py_library`'s import collection,
# and `common.bzl`'s `create_py_info` reaches `cc_helper`.
_py_internal = struct(
    get_label_repo_runfiles_path = _py_internal_get_label_repo_runfiles_path,
    are_action_listeners_enabled = _py_internal_are_action_listeners_enabled,
    cc_helper = _cc_helper,
)

exported_toplevels = {
    "macro": _macro_stub,
    "AnalysisFailureInfo": AnalysisFailureInfo,
    "AnalysisTestResultInfo": AnalysisTestResultInfo,
    "DefaultInfo": DefaultInfo,
    "OutputGroupInfo": OutputGroupInfo,
    "RunEnvironmentInfo": RunEnvironmentInfo,
    "CcInfo": CcInfo,
    "CcToolchainConfigInfo": CcToolchainConfigInfo,
    "CcSharedLibraryInfo": CcSharedLibraryInfo,
    "CcSharedLibraryHintInfo": CcSharedLibraryHintInfo,
    "DebugPackageInfo": DebugPackageInfo,
    "InstrumentedFilesInfo": InstrumentedFilesInfo,
    "JavaInfo": JavaInfo,
    "JavaPluginInfo": JavaPluginInfo,
    "PackageSpecificationInfo": PackageSpecificationInfo,
    "ProguardSpecProvider": ProguardSpecProvider,
    "ProtoInfo": ProtoInfo,
    "PyInfo": PyInfo,
    "config_common": struct(
        FeatureFlagInfo = FeatureFlagInfo,
        toolchain_type = _config_common_toolchain_type,
    ),
    "configuration_field": configuration_field,
    "coverage_common": struct(
        instrumented_files_info = coverage_common_instrumented_files_info,
    ),
    "exec_transition": transition,
    "json": struct(
        decode = json.decode,
        encode = json.encode,
        # starlark-go does not support json.encode_indent().
        # TODO: Should we get it added?
        encode_indent = builtins_json_encode_indent,
        indent = json.indent,
    ),
    "platform_common": struct(
        ConstraintValueInfo = ConstraintValueInfo,
        PlatformInfo = PlatformInfo,
        TemplateVariableInfo = TemplateVariableInfo,
        ToolchainInfo = ToolchainInfo,
    ),
    "proto_common_do_not_use": struct(
        external_proto_infos = proto_common_do_not_use_external_proto_infos,
        incompatible_enable_proto_toolchain_resolution = proto_common_do_not_use_incompatible_enable_proto_toolchain_resolution,
        # Uppercase module constant protobuf's proto rules read directly:
        # `getattr(proto_common, "INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION", False)`.
        # Loupe always enables toolchain resolution.
        INCOMPATIBLE_ENABLE_PROTO_TOOLCHAIN_RESOLUTION = True,
    ),
    "testing": struct(
        ExecutionInfo = ExecutionInfo,
    ),
    # `cc_common` / `java_common` are native top-level globals in Bazel
    # (host-Java modules) and remain so post-starlarkification — rulesets
    # capture them as bare globals (rules_nodejs `native.bzl`:
    # `native_cc_common = cc_common`; protobuf's java toolchain shims), so
    # the bare name must resolve or the whole module fails to load. Loupe
    # doesn't model the cc/java *algorithms*, so these stay near-empty and
    # any unmodelled method access raises ("no field X") — loud-fail, the
    # opposite of the banned wildcard. The one member we must supply:
    # rules_cc resolves its `cc_internal` via `cc_common.internal_DO_NOT_USE()`
    # (else an empty-struct fallback that breaks CcInfo construction), so
    # `cc_common` carries that accessor. Grow from probe feedback; never
    # re-vendor bonanza's full reimplementation. See pull_ourselves_together.md.
    "cc_common": struct(
        internal_DO_NOT_USE = _cc_common_internal_do_not_use,
        # rules_cc's cc_common.bzl captures the native `cc_common` global as
        # `native_cc_common` and re-exports these members off it (at load) into
        # its own public `cc_common` struct. `do_not_use_tools_cpp_compiler_present`
        # is a sentinel field (native cc present -> True); the rest are function
        # refs only invoked on cc-action paths, so they're loud-fail stubs.
        do_not_use_tools_cpp_compiler_present = True,
        get_tool_for_action = _native_unimplemented("cc_common.get_tool_for_action"),
        get_execution_requirements = _native_unimplemented("cc_common.get_execution_requirements"),
        action_is_enabled = _native_unimplemented("cc_common.action_is_enabled"),
        get_memory_inefficient_command_line = _native_unimplemented("cc_common.get_memory_inefficient_command_line"),
        get_environment_variables = _native_unimplemented("cc_common.get_environment_variables"),
        empty_variables = _native_unimplemented("cc_common.empty_variables"),
        check_experimental_cc_shared_library = _native_unimplemented("cc_common.check_experimental_cc_shared_library"),
        incompatible_disable_objc_library_transition = _native_unimplemented("cc_common.incompatible_disable_objc_library_transition"),
        add_go_exec_groups_to_binary_rules = _native_unimplemented("cc_common.add_go_exec_groups_to_binary_rules"),
        get_tool_requirement_for_action = _native_unimplemented("cc_common.get_tool_requirement_for_action"),
        implementation_deps_allowed_by_allowlist = _native_unimplemented("cc_common.implementation_deps_allowed_by_allowlist"),
        create_compile_action = _native_unimplemented("cc_common.create_compile_action"),
    ),
    "apple_common": _apple_common,
    "java_common": struct(
        internal_DO_NOT_USE = _java_common_internal_do_not_use,
    ),
    # `py_internal` is Bazel's internal Python helper (normally reached
    # via `_builtins.internal.py_internal`), but some vendored shims
    # reference the bare name. Members are modelled on demand (see
    # `_py_internal` above); unmodelled access still raises.
    "py_internal": _py_internal,
}

exported_toplevels["_builtins"] = struct(
    # `_builtins.internal` (Bazel's host-Java machinery) stays empty: Loupe
    # models none of it, and access raises (real-or-fail) rather than
    # returning a wildcard. rules_cc reaches cc_internal via
    # `cc_common.internal_DO_NOT_USE()` (wired on `cc_common` above), not
    # through here. See pull_ourselves_together.md.
    internal = struct(),
    toplevel = struct(**exported_toplevels),
)
