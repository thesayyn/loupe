"""platform_common.* — native provider type stubs.

Bazel's platform / toolchain / constraint surface. Loupe's Rust
toolchain resolver (toolchains.rs, target.rs) looks these up by
*symbol name* via `provider_by_symbol`, so the .bzl-defined
ProviderType matches the same way a native Rust ProviderType would.

User code constructs them as `platform_common.ToolchainInfo(...)`
and the result lands in `target[platform_common.ToolchainInfo]`
cleanly.
"""

PlatformInfo = provider(doc = "Bazel platform_common.PlatformInfo. STUB at v1.")
ToolchainInfo = provider(doc = "Bazel platform_common.ToolchainInfo. Loupe's toolchain resolver populates this on resolved-toolchain target results.")
ConstraintSettingInfo = provider(doc = "Bazel platform_common.ConstraintSettingInfo. STUB at v1.")
ConstraintValueInfo = provider(doc = "Bazel platform_common.ConstraintValueInfo. STUB at v1.")
TemplateVariableInfo = provider(doc = "Bazel platform_common.TemplateVariableInfo. STUB at v1.")

platform_common = struct(
    PlatformInfo = PlatformInfo,
    ToolchainInfo = ToolchainInfo,
    ConstraintSettingInfo = ConstraintSettingInfo,
    ConstraintValueInfo = ConstraintValueInfo,
    TemplateVariableInfo = TemplateVariableInfo,
)
