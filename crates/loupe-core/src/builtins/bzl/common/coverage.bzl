"""coverage_common namespace + InstrumentedFilesInfo.

Bazel's coverage hook. v1 doesn't model coverage but `rules_shell`'s
`sh_library` (reached transitively from `oci_load`) returns the
result alongside DefaultInfo. The constructor captures the
attribute-name lists as provider fields and ignores `ctx` — no
execution → no instrumentation.
"""

InstrumentedFilesInfo = provider(
    doc = "Bazel coverage_common.instrumented_files_info return shape.",
    fields = ["source_attributes", "dependency_attributes", "extensions", "metadata_files"],
)

def _instrumented_files_info(
        ctx,
        source_attributes = [],
        dependency_attributes = [],
        extensions = [],
        metadata_files = [],
        **kwargs):
    # ctx is positional in Bazel's signature but unused here — v1
    # doesn't generate coverage. Extra kwargs (e.g. `source_extensions`,
    # `dependency_attribute_attribute_aspects`) are silently dropped.
    _ = ctx
    _ = kwargs
    return InstrumentedFilesInfo(
        source_attributes = source_attributes,
        dependency_attributes = dependency_attributes,
        extensions = extensions,
        metadata_files = metadata_files,
    )

coverage_common = struct(
    instrumented_files_info = _instrumented_files_info,
)
