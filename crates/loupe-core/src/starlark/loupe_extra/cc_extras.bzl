# Loupe-specific cc-related top-levels that real Bazel registers as
# host-Java built-ins but the vendored upstream's `exports.bzl`
# leaves out of `exported_toplevels`. Ruleset code at module scope
# expects these as bare names — rules_cc's `native.bzl` aliases
# `NativeCcSharedLibraryInfo = CcSharedLibraryInfo`, for example.

# Identity-significant: any rule that returns `CcSharedLibraryInfo(...)`
# resolves the bare name through this binding, so consumers reading
# `target[CcSharedLibraryInfo]` see the same provider type. The
# vendored `builtins_bzl/common/cc/cc_shared_library.bzl` defines a
# fuller schema with `linker_input`/`exports` fields, but loading it
# pulls in the entire cc_common cascade. Keep this minimal for now —
# Loupe doesn't model cc actions so the field shape is unused.
CcSharedLibraryInfo = provider(fields = ["linker_input", "exports"])

# `apple_common` — host-Java struct in real Bazel. The vendored
# upstream's `_builtins.internal.apple_common` only carries
# `dotted_version`; rules_cc's `cc/extensions.bzl` and the unix
# toolchain config reach for `Objc`, `XcodeVersionConfig`,
# `apple_toolchain()`, and `new_objc_provider`. Loupe doesn't model
# Apple action generation — these are surfaced as bare-name
# placeholders so `ObjcInfo = apple_common.Objc` at module scope
# resolves.

# Provider stubs. Identity-significant: re-emitted `target[ObjcInfo]`
# probes use these values.
_Objc = provider()
_XcodeVersionConfig = provider()

def _apple_toolchain_stub(*args, **kwargs):
    _ = (args, kwargs)
    return struct()

def _new_objc_provider_stub(*args, **kwargs):
    _ = (args, kwargs)
    return None

def _dotted_version_stub(s):
    return s

apple_common = struct(
    Objc = _Objc,
    XcodeVersionConfig = _XcodeVersionConfig,
    apple_toolchain = _apple_toolchain_stub,
    new_objc_provider = _new_objc_provider_stub,
    dotted_version = _dotted_version_stub,
)
