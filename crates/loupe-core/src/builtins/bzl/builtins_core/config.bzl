"""Loupe `builtins_core/config.bzl` — Bazel's `config.*` build-setting namespace.

`bazel_skylib//rules:common_settings.bzl` and similar rulesets declare
build flags through `config.bool(flag=True)`, `config.int(flag=True)`,
`config.string(...)`, etc. Each call returns a metadata blob the
engine consumes through `rule(build_setting = config.<kind>(...))`.

Loupe v1 doesn't model command-line flag plumbing end-to-end (no
`--flag` parser, no per-invocation flag values). The metadata still
needs to *exist* so rule definitions evaluate; we surface a struct
with `kind` + `flag` fields — the same shape bonanza's Go side
returns via `pkg/model/starlark/builtins.go::config`. The engine's
`rule()` synthesises a `build_setting_default` attr when
`build_setting` is non-None (see `starlark_types/rule.rs`), which
covers the BUILD-side `bool_flag(build_setting_default=True)` path.
"""

def _config_bool(flag = False):
    return struct(kind = "bool", flag = flag)

def _config_int(flag = False):
    return struct(kind = "int", flag = flag)

def _config_string(flag = False, values = None):
    _ = values
    return struct(kind = "string", flag = flag)

def _config_string_list(flag = False, repeatable = False):
    _ = repeatable
    return struct(kind = "string_list", flag = flag)

def _config_label(flag = False):
    return struct(kind = "label", flag = flag)

def _config_label_list(flag = False):
    return struct(kind = "label_list", flag = flag)

def _config_exec(exec_group = None):
    # Bonanza returns a `Transition` here; Loupe doesn't model
    # transitions yet — `attr.label(cfg=config.exec())` records the
    # flag and surfaces a `[Target]` shape on `ctx.attr.<dep>`.
    _ = exec_group
    return struct(kind = "exec_transition")

def _config_none():
    # `attr.label(cfg=config.none())` opts out of any transition.
    return struct(kind = "none_transition")

config = struct(
    bool = _config_bool,
    exec = _config_exec,
    int = _config_int,
    label = _config_label,
    label_list = _config_label_list,
    none = _config_none,
    string = _config_string,
    string_list = _config_string_list,
)
