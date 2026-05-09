"""User-space no-op stubs for Bazel APIs Loupe doesn't model.

Each entry here is a Starlark function or constant standing in for a
Bazel surface Loupe doesn't model end-to-end (visibility enforcement,
repository-rule fetching, bzlmod extension execution, configuration
transitions, …). Adding a new stub costs one `def` here instead of
one `#[starlark_module]` entry in Rust — that's the bonanza pattern.

The stubs come in two flavours:

  - **Accept-and-ignore** (`visibility`, `licenses`, `package`,
    `package_group`, …) — accept any kwargs, return `None`. Some
    return their `implementation` argument so
    `mytrans = transition(implementation = _impl)` then
    `attr.label(cfg = mytrans)` works syntactically.
  - **Native provider const placeholders** — Bazel's built-in
    providers (`PyInfo`, `JavaInfo`, …) that some bzlmod helpers
    reference by bare name in `getattr(..., default=X)` patterns.
    Bound as `None`; rules that actually consume them either fail
    loudly or pick a fallback path.
"""

def visibility(*args, **kwargs):
    """`visibility([...])` — `.bzl`-level load-visibility marker."""
    _ = (args, kwargs)
    return None

def licenses(*args, **kwargs):
    """Legacy `licenses(["notice"])` declaration."""
    _ = (args, kwargs)
    return None

def package(**kwargs):
    """`package(default_visibility=…, …)` — BUILD-level directive."""
    _ = kwargs
    return None

def package_group(**kwargs):
    """`package_group(name=…, packages=…, includes=…)`."""
    _ = kwargs
    return None

def repository_rule(*args, **kwargs):
    """`repository_rule(implementation, attrs=…, …)` — repo rule decl."""
    _ = (args, kwargs)
    return None

def module_extension(*args, **kwargs):
    """`module_extension(implementation, tag_classes=…, …)` — bzlmod ext."""
    _ = (args, kwargs)
    return None

def tag_class(**kwargs):
    """`tag_class(attrs=…, doc=…)` — module-extension tag class decl."""
    _ = kwargs
    return None

def macro(implementation = None, **kwargs):
    """`macro(implementation=…, attrs=…, doc=…, finalizer=…)`.

    Returns the impl so `mymacro = macro(implementation = _impl)` then
    invoking `mymacro(...)` reaches the impl. Macro semantics
    (per-package state, rule-call interception) aren't modelled.
    """
    _ = kwargs
    return implementation

def subrule(**kwargs):
    """`subrule(implementation, …)` — experimental subrule decl."""
    _ = kwargs
    return None

def transition(implementation = None, **kwargs):
    """`transition(implementation=…, inputs=…, outputs=…)`.

    Returns the impl so `attr.label(cfg = mytrans)` works
    syntactically. Configuration transitions aren't modelled.
    """
    _ = kwargs
    return implementation

def analysis_test_transition(**kwargs):
    """`analysis_test_transition(settings=…)`."""
    _ = kwargs
    return None

def exec_group(**kwargs):
    """`exec_group(toolchains=…, exec_compatible_with=…)`."""
    _ = kwargs
    return None

# Bazel built-in provider names some bzlmod helpers reach for by bare
# name (`getattr(..., default = X)`). Loupe doesn't model these; bind
# `None` so the bare-name lookup resolves.
PackageSpecificationInfo = None
RunEnvironmentInfo = None
ProtoInfo = None
PyInfo = None
PyRuntimeInfo = None
PyCcLinkParamsProvider = None
JavaInfo = None
JavaPluginInfo = None
CcInfo = None
CcToolchainInfo = None
CcToolchainConfigInfo = None
CcLauncherInfo = None
CcSharedLibraryInfo = None
CcSharedLibraryHintInfo = None
DebugPackageInfo = None
cc_proto_aspect = None
