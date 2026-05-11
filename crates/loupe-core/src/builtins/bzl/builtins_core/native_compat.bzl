"""User-space `native` namespace — Tier 2 (DESIGN.md §3).

Bazel exposes `native.<rule>` as a thread-local set of host-side
constructors. Loupe's Rust core only models the small handful that
need engine state (`glob`, `package_name`); the rest are stubs that
must exist as attributes for vendored proxy files to evaluate.

The specific load chain forcing the issue:

    @rules_cc//cc:defs.bzl
      → cc/cc_binary.bzl   (et al.)
        → @cc_compatibility_proxy//:proxy.bzl
            → `cc_binary = native.cc_binary`  (bare attribute access)

`@cc_compatibility_proxy/proxy.bzl` top-level-binds every native
cc_/objc_/fdo_/propeller_ rule at module load. If any of those
attributes is missing on `native`, freeze fails and every downstream
rule that touches `@rules_cc` (rules_nodejs's `nodejs/toolchain.bzl`
for `CcInfo`) loses its toolchain.

Bonanza's approach: their `native` is a Go-side starlark.Module that
they populate explicitly. Loupe's equivalent is this struct — loaded
once and re-exported from `wrappers.bzl`. Module-local bindings
shadow globals, so user `.bzl` files see this `native` in place of
the Rust-defined `native_namespace` from `compat.rs::register_native`.

The `getattr(native, "name", None)` pattern in proxy.bzl tolerates
missing names; only the bare-access cases need to resolve.
"""

load(
    "@_builtins//builtins_core/exports.bzl",
    _cc_library = "cc_library",
    _filegroup = "filegroup",
    _genrule = "genrule",
    _sh_binary = "sh_binary",
    _sh_library = "sh_library",
    _sh_test = "sh_test",
    _test_suite = "test_suite",
)

def _noop(*args, **kwargs):
    _ = (args, kwargs)
    return None

def _glob(include, exclude = [], exclude_directories = 1, allow_empty = False):
    # STUB: real glob expansion needs the active BUILD package's
    # filesystem view; from .bzl macros we don't have one. Return
    # the patterns verbatim so macros that introspect glob results
    # for existence checks see a non-empty list back.
    _ = (exclude, exclude_directories, allow_empty)
    return include

def _package_name():
    return ""

def _repository_name():
    return "@@"

def _existing_rule(_name):
    return None

def _existing_rules():
    return {}

native = struct(
    # Real user-space rules. Same impls as the bare-named globals so
    # `native.cc_library(...)` from a macro records the same shape as
    # bare `cc_library(...)` from a BUILD.
    cc_library = _cc_library,
    filegroup = _filegroup,
    genrule = _genrule,
    sh_binary = _sh_binary,
    sh_library = _sh_library,
    sh_test = _sh_test,
    test_suite = _test_suite,
    # Stateless introspection (same shape as Rust `native_namespace`).
    glob = _glob,
    package_name = _package_name,
    repository_name = _repository_name,
    existing_rule = _existing_rule,
    existing_rules = _existing_rules,
    # Host-side native rules referenced by vendored proxy files.
    # Bazel ships these in Java; Loupe doesn't model them. Stubs
    # accept any kwargs and drop the call. proxy.bzl uses bare
    # attribute access (`cc_binary = native.cc_binary`), so the
    # names must exist; `getattr(native, "cc_static_library", None)`
    # patterns are already tolerant of missing entries.
    alias = _noop,
    config_setting = _noop,
    cc_binary = _noop,
    cc_test = _noop,
    cc_import = _noop,
    cc_shared_library = _noop,
    cc_toolchain = _noop,
    cc_toolchain_alias = _noop,
    objc_import = _noop,
    objc_library = _noop,
    fdo_prefetch_hints = _noop,
    fdo_profile = _noop,
    propeller_optimize = _noop,
)
