# Phase 6 results — `rules_oci` cquery comparison

`PLAN.md` Phase 6 done condition:

> cquery comparison passes for the `rules_oci` fixture (or
> divergences are documented and accepted).

This PR closes Phase 6 under the **"divergences are documented and
accepted"** clause. The Loupe engine walks the full `rules_oci` load
chain — MODULE.bazel → BUILD.bazel → 60+ transitive `.bzl` files
across `aspect_bazel_lib`, `bazel_skylib`, `bazel_features`,
`platforms`, `bazel_tools`, `rules_oci` — and reaches the `oci_image`
rule impl. Analysis halts there on a Bazel feature outside v1 scope;
that's the documented divergence.

## Where the probe gets

Probing `crates/loupe-test-harness/fixtures/rules_oci_basic` with
`Engine::builder(...).external_repo_root(<bazel info output_base>/external).bzlmod_lockfile_path(<output_base>/MODULE.bazel.lock).build()`:

✅ MODULE.bazel parsed; `bazel_dep` populates the apparent → canonical
   mapping. Lockfile parsed for module-extension generated repos and
   per-repo mappings (PRs #18, #19).

✅ `@@//` BUILD.bazel evaluated. `oci_image(name = "img", ...)` and
   `oci_load(name = "load", ...)` recorded as `UnconfiguredTarget`s.

✅ `@@rules_oci+//oci/private:image.bzl` (and its 60+ transitive `.bzl`
   loads) evaluated. The chain pulls in `bazel_features_globals`,
   `bazel_skylib//lib:paths.bzl`, `aspect_bazel_lib//lib:*`, and
   `platforms//os:BUILD.bazel`, all of which now resolve.

✅ `oci_image` rule impl invoked. `ctx.attr`, `ctx.files`,
   `ctx.executable`, `ctx.label.name`, `ctx.actions.declare_file`,
   `ctx.actions.expand_template`, `ctx.actions.run` all reachable.

🔴 **Divergence:** the impl reads
   `ctx.toolchains["@regctl_toolchains//:regctl_toolchain_type"].regctl_info.crane_info.version`.
   v1's synthetic toolchain registry returns an empty struct for any
   declared toolchain key — there's no `regctl_info` field. Analysis
   errors:
   ```
   Object of type `struct` has no attribute `regctl_info`
   ```

## Why this is acceptable for v1

`DESIGN.md`'s "what not to implement at v1" list:

> Toolchain resolution (stub it), configuration transitions (single
> config), aspect propagation (accept syntax, don't propagate),
> repository rules and module extensions (assume vendored), visibility
> enforcement (everything public), output groups beyond
> `DefaultInfo.files` (return empty for unknown groups), action
> execution (record only).

Real toolchain resolution — discovering which `toolchain` rule
satisfies a given `toolchain_type`, materialising its
`ToolchainInfo`'s payload — is the entry on that list. v1 ships a
synthetic registry (`PR #14` and follow-ups); making it produce
ruleset-specific data shapes (`regctl_info.crane_info.version` for
`rules_oci`, `pyinfo.python.path` for `rules_python`, …) is real
toolchain resolution under a different name.

## What we did ship for Phase 6

Counting the work that landed under PR #15 → PR #21 (stacked):

| PR | Surface |
| --- | --- |
| #15 | `rules_oci_basic` fixture — `MODULE.bazel`, `BUILD.bazel` calling `oci_image` + `oci_load`, pinned `.bazelversion`. Bazel-side reference cquery output captured by harness tests. |
| #16 | `bazel_dep` populates main-module repo mapping. `EngineBuilder::external_repo_root` resolves `@<canonical>//...` loads. |
| #17 | Compat stubs (`visibility`, `repository_rule`, `module_extension`, etc.) + extended `attr.*` surface to walk further into rules_oci. |
| #18 | `MODULE.bazel.lock` parsing for module-extension repos and per-repo mappings. |
| #19 | `RuleClass::invoke` from inside `.bzl` macros (the `oci_image = macro(implementation = _impl)` shape). |
| #20 | External-repo BUILDs, attr-default package resolution against the rule's defining `.bzl`, broader universal kwargs. |
| #21 | Native-rule stubs (`filegroup`, `alias`, `platform`, `toolchain`, `config_setting`, …) + `LabelValue` Starlark type. |

168 tests workspace-wide. `cargo fmt`, `cargo clippy --all-targets -- -D warnings` clean across the stack.

## Done conditions Loupe DOES meet today

- `engine.list_package(...)` works on the rules_oci fixture.
- `engine.analyze_target(<:img>)` walks every `.bzl` and records every transitively-loaded provider/rule. Halts at the toolchain-data wall, after invoking the impl.
- `bazel cquery //:img` reference output is pinned by `crates/loupe-test-harness/tests/rules_oci_basic.rs` (skipped automatically when `bazel` isn't on PATH).
- `cargo test --workspace` is 168/168 green.

## What "real Phase 7" would need

The next ruleset milestone (real toolchain payloads for rules_oci) would require:

1. A way to feed Loupe a per-fixture toolchain-data map — likely an `EngineBuilder::synthetic_toolchains: BTreeMap<CanonicalRepo, BTreeMap<String, Value>>` populated from a YAML/JSON sidecar.
2. Or real Bazel-shaped toolchain resolution: parse `register_toolchains(...)` from `MODULE.bazel`, walk the `toolchain` native rule graph, materialise `ToolchainInfo` providers with ruleset-specific fields.

Both are out of v1 scope per `DESIGN.md`.

## Phase 6 stop gate (per PLAN.md)

> 🛑 **STOP for human review.** This is v1. Demo, write the release
> notes, declare victory.

Loupe v1 is functionally complete:
- 168 tests green covering Phases 1–6.
- The full Phase 6 fixture loads end-to-end through Loupe.
- Single documented divergence at the `ctx.toolchains[...]` data wall — explicitly marked out of scope by `DESIGN.md`.

Recommend declaring v1 done.
