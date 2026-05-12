"""User-space `native` namespace — Tier-2 compatibility surface.

Bazel exposes `native.<rule>` as a thread-local set of host-side
constructors. Loupe's Rust core only models the handful that need
engine state (`glob`, `package_name`); the rest are stubs that must
exist as attributes for vendored proxy files (notably
`@cc_compatibility_proxy//:proxy.bzl`) to evaluate. Module-local
bindings shadow globals, so user `.bzl` files see this `native`
struct in place of the Rust-defined `native_namespace`.
"""

def _noop(*args, **kwargs):
    _ = (args, kwargs)
    return None

def _glob(include, exclude = [], exclude_directories = 1, allow_empty = False):
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
    # User-callable rule stubs (record-only at BUILD; vendored proxies
    # bare-bind these names as `cc_binary = native.cc_binary`).
    cc_library = _noop,
    filegroup = _noop,
    genrule = _noop,
    sh_binary = _noop,
    sh_library = _noop,
    sh_test = _noop,
    test_suite = _noop,
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
    # Stateless introspection.
    glob = _glob,
    package_name = _package_name,
    repository_name = _repository_name,
    existing_rule = _existing_rule,
    existing_rules = _existing_rules,
)
