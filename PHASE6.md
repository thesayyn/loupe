# Phase 6 Inventory — `rules_oci`

The pre-read PLAN.md Phase 6 mandates: every Starlark API the `rules_oci`
rules use, categorized against what Loupe ships today.

Source inspected (main branch of `bazel-contrib/rules_oci`):

- `oci/private/image.bzl` — `oci_image`
- `oci/private/image_index.bzl` — `oci_image_index`
- `oci/private/push.bzl` — `oci_push`
- `oci/private/load.bzl` — `oci_load` (the tarball rule)
- `oci/private/util.bzl` — shared helpers
- `oci/defs.bzl` — re-exports + macro wrappers

`oci_image_index` is **out of scope for v1** because it uses Starlark
configuration transitions (`cfg = _image_index_transition` rewriting
`//command_line_option:platforms`); DESIGN.md v1 scope is "single
configuration, no transitions." The cquery-parity fixture targets
`oci_image` + `oci_push` + `oci_load`.

## `ctx.*` surface used across all in-scope rules

Legend: ✅ implemented · 🟡 stubbed-OK for v1 · 🔴 needs work for Phase 6.

| API | State | Notes |
| --- | --- | --- |
| `ctx.label.name` | ✅ | `ctx.label` is a string in v1; `.name` is substring-after-colon. |
| `ctx.workspace_name` | ✅ | Read off `MODULE.bazel`. |
| `ctx.attr.<name>` | ✅ | All kinds rules_oci uses (label, label_list, string, bool). |
| `ctx.file.<name>` | ✅ | Single-file label deps. |
| `ctx.files.<name>` | ✅ | label_list flattening. |
| `ctx.executable.<name>` | ✅ | label with `executable=True`. |
| `ctx.actions.declare_file` | ✅ | |
| `ctx.actions.write(is_executable=)` | ✅ | |
| `ctx.actions.run(executable, arguments, inputs, outputs, env, mnemonic, progress_message)` | 🟡 → 🔴 | Missing kwargs rules_oci passes: `tools=` (additional input executables), `execution_requirements=` (accept-and-ignore). |
| `ctx.actions.declare_directory` | 🔴 | Bazel returns a tree-artifact `File`. v1 plan: return a regular `File` flagged as a directory; consumers iterating its tree get an empty list. STUB. |
| `ctx.actions.expand_template(template, output, substitutions, is_executable=)` | 🔴 | New action kind. Records the substitution dict; recording shape only (we don't execute). |
| `ctx.actions.args()` | 🔴 | Bazel's lazy `Args` value with `.add`, `.add_all`, `.format_each`, `.use_param_file`, etc. Significant new value type. v1: a wrapper that materializes to `list[str]` on action recording (no laziness). |
| `ctx.runfiles(files=, transitive_files=, symlinks=, root_symlinks=)` | 🟡 | `transitive_files` currently accepts a flat list (STUB); rules_oci passes `depset(File)`. Accept depset directly: flatten on construction (still v1 — depset-shaped runfiles is the future fix). |
| `runfiles.merge` / `merge_all` | ✅ | |
| `ctx.toolchains["<type>"]` | 🔴 | Currently `ctx.toolchains` is an empty dict. Rules_oci does `ctx.toolchains["@..//:regctl_toolchain_type"].crane_info.version` etc. v1 plan: synthetic toolchain registry — `ctx.toolchains[label_str]` returns a struct populated from a fixture-side stub map. Real toolchain resolution stays out of v1 per DESIGN.md. |
| `resource_set` callback on `actions.run` | 🟡 | Accept and ignore (v1 doesn't model action scheduling cost). |

## Provider types referenced

| Provider | State | Notes |
| --- | --- | --- |
| `DefaultInfo` (read + returned) | ✅ | Native, in tree. |
| `OutputGroupInfo` | 🔴 | `oci_load` returns it. Need a native constructor + a real read path on `target.output_groups` (currently empty struct stub). |
| `platform_common.PlatformInfo` | 🔴 | Read on `oci_image_index`'s `platforms` attr — that rule is out of scope, so this is **not blocking** the fixture. Stub a `platform_common.PlatformInfo` provider symbol so Starlark name resolution succeeds; never actually read. |
| `platform_common.ToolchainInfo` | 🔴 | Same: name resolution only; the `ctx.toolchains[]` synthetic registry (above) carries the actual data shape. |

## `attr.*` modifiers used

| Modifier | State | Notes |
| --- | --- | --- |
| `attr.label(allow_single_file=True)` | 🟡 | Currently accepted-and-ignored (no enforcement). Fine for v1. |
| `attr.label(allow_single_file=[".txt"])` | 🟡 → 🔴 | List form is also accepted-and-ignored today; rules_oci has `allow_single_file=[".txt"]` on `oci_load.repo_tags`. v1 keeps as accept-and-ignore. |
| `attr.label_list(allow_files=[".tar", ...])` | 🟡 | Accept-and-ignore. |
| `attr.label(executable=True, cfg="exec"/"target")` | 🟡 | `executable=True` works. `cfg="exec"` / `cfg="target"` strings are accepted-and-ignored (single config). Document explicitly. |
| `attr.string(values=[...])` | 🔴 | `oci_load` has `format = attr.string(values=["docker","oci"])`. v1: accept the kwarg, don't enforce. STUB. |
| `attr.label(default = "<label>", cfg="exec")` | 🟡 | Default-label resolution at rule-construction time already works for non-cfg. Cross-cfg defaults: same as above (single config). |

## Starlark globals required

| Global | State | Notes |
| --- | --- | --- |
| `depset(...)` | ✅ | Just landed in PR #11. |
| `select(...)` | ✅ | |
| `provider(...)` | ✅ | |
| `rule(implementation, attrs, executable=, toolchains=)` | 🟡 → 🔴 | `executable=True` rule kwarg + `toolchains=[...]` kwarg need plumbing. v1: store both on the rule shape; `toolchains=` becomes the key set the synthetic `ctx.toolchains[]` registry can satisfy. |
| `attr.*` | ✅ | (modulo modifier work above) |
| `aspect(...)` | ✅ | Rules_oci doesn't use aspects in the in-scope rules — unblocked. |

## Bazel features required that v1 will NOT ship

These are deliberate scope cuts; the fixture is constructed to avoid them.

- **Starlark configuration transitions.** `oci_image_index` only. Out
  of scope; the fixture skips that rule.
- **TreeArtifacts (real `declare_directory` semantics).** v1 records
  the declaration as a regular `File`; downstream consumers that
  iterate the tree see an empty list. Document as a STUB.
- **Real toolchain resolution** (`config_setting`, `register_toolchains`,
  platform constraints). v1 ships a synthetic registry; real
  resolution is a future phase.
- **`Args` lazy command-line construction.** v1 materializes eagerly to
  `list[str]` at action-recording time. Equivalent for v1 fixtures.
- **`expand_template` execution.** v1 records the action with its
  substitutions dict; we don't actually run the substitution at
  analysis time. Consumers reading the action's content see the
  templated source verbatim. STUB.

## Order of work for Phase 6

The categorization above implies this implementation order. Each
bullet is intended to be one PR.

1. **`ctx.actions.declare_directory`** (STUB — File flagged as
   directory). Smallest. Unblocks rules that touch directories.
2. **`ctx.actions.expand_template`**. New `ActionKind::ExpandTemplate`
   variant + Starlark binding. Record-only.
3. **`ctx.actions.args()` value type + recording integration.**
   Materialize-eagerly v1 — return `list[str]`.
4. **Extend `ctx.actions.run` kwargs** — `tools`, `execution_requirements`.
   Mostly accept-and-record; `tools` adds to the inputs list.
5. **`ctx.runfiles(transitive_files=depset(File))`.** Accept the
   depset, flatten on construction.
6. **`OutputGroupInfo` native provider.** Constructor + real read path
   on `target.output_groups`.
7. **`rule(executable=True, toolchains=[...])` plumbing** + synthetic
   `ctx.toolchains[]` registry that satisfies declared keys with a
   fixture-supplied data dict.
8. **`platform_common.PlatformInfo` / `ToolchainInfo`** symbol stubs
   (name resolution only).
9. **`attr.string(values=[...])`** — accept-and-ignore.
10. **The `rules_oci` fixture** — vendor `rules_oci` + a single
    `oci_image` → `oci_load` workspace. cquery comparison is the
    pass criterion.
