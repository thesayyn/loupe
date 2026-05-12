# Loupe-specific accept-and-ignore stubs for Bazel surface items
# the vendored upstream tree doesn't define but user `.bzl`/BUILD
# code routinely references.
#
# Each function records nothing and returns `None`. Loupe doesn't
# model the underlying semantics (load-visibility enforcement,
# license rollup, exec-group routing, …) for cquery-style
# introspection, but user code must still parse and freeze.

def visibility(*args, **kwargs):
    """`visibility([...])` — `.bzl` load-visibility marker."""
    _ = (args, kwargs)
    return None

def licenses(*args, **kwargs):
    """`licenses([...])` — BUILD-level license declaration."""
    _ = (args, kwargs)
    return None

def package(**kwargs):
    """`package(default_visibility=…, …)` — BUILD-level directive."""
    _ = kwargs
    return None

def package_group(**kwargs):
    """`package_group(name=…, packages=…, …)` — BUILD-level."""
    _ = kwargs
    return None

def repository_rule(*args, **kwargs):
    """`repository_rule(...)` — repo rule constructor."""
    _ = (args, kwargs)
    return None

def module_extension(*args, **kwargs):
    """`module_extension(...)` — module extension constructor."""
    _ = (args, kwargs)
    return None

def tag_class(**kwargs):
    """`tag_class(...)` — module extension tag class declaration."""
    _ = kwargs
    return None

def macro(implementation = None, **kwargs):
    """`macro(implementation=…, …)` — symbolic macro constructor."""
    _ = (implementation, kwargs)
    return None

def subrule(**kwargs):
    """`subrule(...)` — subrule constructor."""
    _ = kwargs
    return None

def transition(implementation = None, **kwargs):
    """`transition(implementation=…, …)` — configuration transition."""
    _ = (implementation, kwargs)
    return None

def analysis_test_transition(**kwargs):
    """`analysis_test_transition(...)` — test-only transition."""
    _ = kwargs
    return None

def exec_group(**kwargs):
    """`exec_group(...)` — exec-group declaration."""
    _ = kwargs
    return None
