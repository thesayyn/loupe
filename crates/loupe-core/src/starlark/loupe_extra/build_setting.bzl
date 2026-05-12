# Loupe-specific extras: provider types not defined by the vendored
# upstream tree but referenced by user `.bzl`s.
#
# `BuildSettingInfo` is the provider every `build_setting` rule
# returns. `bazel-skylib`'s `common_settings.bzl` defines the same
# provider; rules that don't use skylib build it themselves. Loupe's
# select() resolver looks up `BuildSettingInfo.value` by symbol on
# every analyzed flag target, so the symbol name must match.

BuildSettingInfo = provider(fields = ["value"])
