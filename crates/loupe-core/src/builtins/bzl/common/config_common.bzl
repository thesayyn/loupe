"""`config_common.*` user-space stub.

Bazel exposes `config_common` as a top-level global with helpers that
build up `rule(toolchains = ...)` lists and configuration-setting
matches. Loupe's analysis engine doesn't run transitions, but the
*shape* of these calls has to type-check at parse / freeze time
because vendored Bazel files (`bazel_tools/tools/cpp/toolchain_utils.bzl`)
reference them at module top level.

Each method returns a value of the right shape so callers reading
`config_common.toolchain_type(...)` into a `toolchains=` list don't
crash. The values themselves are inert — Loupe's toolchain resolver
treats label strings the same regardless of whether they were
wrapped in a `config_common.toolchain_type(...)` builder call.
"""

def _toolchain_type(name, mandatory = True):
    _ = mandatory
    # `rule(toolchains=…)` accepts both bare label strings and the
    # builder-result; returning the bare string keeps the downstream
    # canonicaliser happy.
    return name

def _feature_flag_provider():
    return None

config_common = struct(
    toolchain_type = _toolchain_type,
    FeatureFlagInfo = None,
)
