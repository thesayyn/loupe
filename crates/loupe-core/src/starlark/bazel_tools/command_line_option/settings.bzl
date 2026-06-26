load("@bazel_tools//command_line_option:common_settings.bzl", "BuildSettingInfo")

def _simple_impl(ctx):
    return [BuildSettingInfo(value = ctx.build_setting_value)]

bool_flag = rule(
    implementation = _simple_impl,
    build_setting = config.bool(flag = True),
    needs = [],
)

int_flag = rule(
    implementation = _simple_impl,
    build_setting = config.int(flag = True),
    needs = [],
)

label_list_flag = rule(
    implementation = _simple_impl,
    build_setting = config.label_list(flag = True),
    needs = [],
)

string_list_flag = rule(
    implementation = _simple_impl,
    build_setting = config.string_list(flag = True),
    needs = [],
)

repeatable_string_list_flag = rule(
    implementation = _simple_impl,
    build_setting = config.string_list(flag = True, repeatable = True),
    needs = [],
)

def _string_impl(ctx):
    if ctx.attr.values and ctx.build_setting_value not in ctx.attr.values:
        fail("value '%s' is not one of %s" % ctx.build_setting_value, ctx.attr.values)
    return [BuildSettingInfo(value = ctx.build_setting_value)]

string_flag = rule(
    implementation = _string_impl,
    attrs = {
        "values": attr.string_list(),
    },
    build_setting = config.string(flag = True),
    needs = [],
)

# `--xcode_version_config` points (via the apple fragment's
# `xcode_config_label`) at a target the darwin cc toolchain config reads
# `apple_common.XcodeVersionConfig` off, to compute `-mmacosx-version-min`.
# In real Bazel that's `@local_config_xcode//:host_xcodes`, produced by a
# host-inspecting repo rule Loupe can't run. So this flag target doubles
# as the XcodeVersionConfig provider with a fixed minimum-OS.
def _xcode_min_os_for_platform_type(_platform_type):
    # ponytail: read-only analysis never compiles; any valid version
    # satisfies the `-mmacosx-version-min=` flag string. Bump if a rule
    # ever compares it.
    return "11.0"

def _xcode_version_config_impl(ctx):
    return [
        BuildSettingInfo(value = ctx.build_setting_value),
        apple_common.XcodeVersionConfig(
            minimum_os_for_platform_type = _xcode_min_os_for_platform_type,
        ),
    ]

xcode_version_config_flag = rule(
    implementation = _xcode_version_config_impl,
    build_setting = config.string(flag = True),
    needs = [],
)
