# Loupe-specific extras: provider types not defined by the vendored
# upstream tree but referenced by user `.bzl`s.
#
# `BuildSettingInfo` is the provider every `build_setting` rule
# returns. `bazel-skylib`'s `common_settings.bzl` defines the same
# provider; rules that don't use skylib build it themselves. Loupe's
# select() resolver looks up `BuildSettingInfo.value` by symbol on
# every analyzed flag target, so the symbol name must match.

BuildSettingInfo = provider(fields = ["value"])

# `config.bool(flag = True)`, `config.string(...)`, etc. are the
# build-setting-kind declarators rules pass to `rule(build_setting=…)`.
# Loupe doesn't track the kind, so a permissive stub that accepts and
# ignores kwargs is sufficient — `rule()` only checks whether the
# argument is non-None.
def _build_setting_kind(**_kwargs):
    return struct()

config = struct(
    bool = _build_setting_kind,
    string = _build_setting_kind,
    int = _build_setting_kind,
    string_list = _build_setting_kind,
)
