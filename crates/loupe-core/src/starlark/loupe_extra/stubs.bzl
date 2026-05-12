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

_TransitionInfo = provider(fields = ["implementation", "inputs", "outputs"])

def transition(implementation = None, inputs = [], outputs = [], **kwargs):
    """`transition(implementation=…, inputs=[…], outputs=[…])` —
    configuration transition. Bonanza-style: returns a
    `_TransitionInfo` provider instance carrying the impl function +
    its declared input/output flag labels. Loupe's analysis-side
    `derive_dep_config` downcasts this when an `attr.label(cfg=…)`
    carries it as the cfg payload.
    """
    _ = kwargs
    return _TransitionInfo(
        implementation = implementation,
        inputs = list(inputs),
        outputs = list(outputs),
    )

def analysis_test_transition(**kwargs):
    """`analysis_test_transition(...)` — test-only transition."""
    _ = kwargs
    return None

def exec_group(**kwargs):
    """`exec_group(...)` — exec-group declaration."""
    _ = kwargs
    return None
