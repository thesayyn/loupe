"""DefaultInfo and OutputGroupInfo — Bazel's universal native
providers, in user-space starlark.

Every analyzed target carries a `DefaultInfo` (synthesised empty by
Loupe's analysis driver if the impl returned none). Loupe's Rust
analysis looks both up by symbol via `provider_by_symbol`, so the
.bzl-defined ProviderType matches both impl-returned and
synthesised instances.

`DefaultInfo` field shape mirrors Bazel:

- `files` : depset(File) of default outputs
- `executable` : File or None (set for `rule(executable=True)`)
- `runfiles` / `default_runfiles` / `data_runfiles` : Runfiles.
  Bazel accepts any of the three; v1 aliases them at read time
  (see the runfiles aliasing in target.rs / provider.rs).

`OutputGroupInfo` is open-fields — group names are user-chosen kwargs.
"""

DefaultInfo = provider(
    doc = "Bazel's universal default provider — files, executable, runfiles.",
    fields = ["files", "executable", "runfiles", "default_runfiles", "data_runfiles"],
)

OutputGroupInfo = provider(
    doc = "Bazel's named output groups. Open-fields — group names are user-chosen kwargs.",
)
