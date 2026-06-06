# `BuildSettingInfo`, relocated out of a vendored `@bazel_skylib` copy.
#
# Loupe's command-line-option / fragments / make-variable shims model
# build settings internally and need this provider, but the engine no
# longer reads it (select()/config_setting resolve from engine state —
# see `AnalyzedTarget.build_setting_value`). Rather than ship a fallback
# `@bazel_skylib` repo just for these internal shims, define the provider
# here in the Loupe-controlled `bazel_tools` tree. A workspace that uses
# `bazel_skylib` resolves its own `@bazel_skylib//rules:common_settings.bzl`
# (a distinct provider identity, used only in user space).
BuildSettingInfo = provider(
    doc = "A singleton provider that contains the raw value of a build setting.",
    fields = {"value": "The raw value of the build setting."},
)
