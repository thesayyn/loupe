# Loupe — Implementation Details

> Gotchas, idioms, history. Reference, not tutorial.
> Add to this file when you find a new gotcha. It should grow.

## Bazel concepts worth understanding

**Three phases are deeply separated.** Loading, analysis, execution have different available APIs, different value types, different error modes. `glob` only works in BUILD files, not rule impls. `ctx.actions.run` only works in rule impls. Asking why an API works "here but not there" is almost always asking why it's in a different phase.

**Configurations are part of cache identity.** A configured target is a target paired with a configuration. The same target analyzed under two configs is two distinct cache entries. v1 has one config so this is degenerate, but `AnalysisKey` carries the field anyway because retrofitting is much worse than carrying a constant.

**Macros vs. rules.** Most things users call in BUILD files (e.g. `py_binary`) are Starlark macros that expand to one or more rule calls. Macros run during loading; rule impls run during analysis. Loupe gets macros for free — they're just function calls during BUILD evaluation. Don't "implement macro support" as a separate feature.

**Aspects propagate along edges.** An aspect attached to a rule attr applies to the deps reached through that attr, transitively. v1 doesn't propagate, just accepts the syntax. The use case is cross-cutting concerns (linting, IDE indexing, codegen) that walk the dep graph without modifying the rules they walk.

**Output paths are structured.** Bazel's outputs land under `bazel-out/<configuration>/bin/<package>/<file>` (and `bazel-out/<configuration>/genfiles/...` historically, mostly merged with `bin/` now). The `<configuration>` segment is a hash-derived string like `k8-fastbuild` or `darwin_arm64-opt`. Match Bazel's paths exactly — `bazel info output_path` on the fixture tells you what to use.

## starlark-rust idioms

**Heap, FrozenHeap, Module.** `Heap` holds mutable Starlark values for one evaluation. `FrozenHeap` holds immutable values that can be referenced from other heaps via `add_reference`. `Module` is the result of evaluating a `.bzl` file — it freezes when loaded.

**Frozen ≠ const in the Rust sense.** Frozen values are immutable Starlark values, but the underlying memory can still be referenced and traversed. The freezing model is what makes provider identity work via pointer equality.

**Anything stored outside the `Module` / `FrozenModule` wrappers must be frozen.** Live `Value<'v>` references are heap-bound — a `Value<'v>` may only ever live as long as the `Module<'v>` that owns it, and short-lived values get GC'd within an evaluator pass. Stashing a `Value<'v>` in a Rust struct that escapes the evaluation (cache, DICE compute output, anything Arc'd) is unsound: by the time you read it back, the heap may be gone or reused. Freezing gives the static guarantee otherwise: a `FrozenValue` is `'static` and its `FrozenHeap` is `Sync`, so cross-evaluation references work as long as each consumer `add_reference`s the upstream `FrozenHeap`. **Rule of thumb:** if a value needs to outlive `eval_function` / `eval_module`, freeze it first — usually by allocating on a `Module`, freezing the module to a `FrozenModule`, and storing the `FrozenValue`s under that frozen heap. `AnalyzedTarget.analysis_heap` follows exactly this pattern; `CapturedProvider.fields` are `FrozenValue`s into that heap, not `Value<'v>`s.

**Defining builtins.** Use `#[starlark_module]`. Functions take `&mut Evaluator` and Starlark-typed args. Read the starlark-rust crate's tests for examples.

**Custom value types.** Implement `StarlarkValue` for your Rust struct, with `#[starlark_value]` for boilerplate. For values that need to freeze, also implement `Freeze`.

**Diagnostics carry source spans.** When evaluation fails, the `Diagnostic` knows where in user code it happened. Preserve this by propagating with `?`. Surfacing "error in foo.bzl line 42" beats "error in analysis.rs line 247" by an order of magnitude.

**Globals are layered.** Build separate `Globals` for loading-phase (no `ctx`) vs analysis-phase (no `glob`) so APIs aren't accidentally available in the wrong phase.

## DICE patterns

**Async dependencies.** Inside a compute function, `ctx.compute(other_key).await` records the dependency edge automatically. No manual dep tracking — the framework does it via the await graph.

**Don't share `Heap` across compute calls.** `Heap` is not `Sync`. Each compute evaluates into its own `Heap`, freezes the result, returns the `FrozenValue` (whose `FrozenHeap` is `Sync`). Agents will sometimes try to "optimize" by sharing a heap; this breaks parallelism and is wrong.

**Equality drives change propagation.** If a key recomputes to a value equal to its previous value, downstream keys aren't invalidated. Implement value `PartialEq` carefully.

**Versioning at v1 is trivial.** Bump the DICE version on every Loupe invocation; recompute everything. Don't try to be clever about incrementality at v1 — file-content keys + a watcher come later.

## Bazel APIs with weird histories

**`depset` exists because of memory.** Early Bazel passed plain lists through providers; large C++ projects exploded. Depsets share structure transitively. The lazy tree shape is the entire point — flattening eagerly negates the win.

**`select()` is a value because attrs need to be configurable per-config.** A `srcs = select({...})` would be useless if it resolved at load time, because the same BUILD file's targets get analyzed under multiple configs. So `select()` returns a value that survives into analysis. v1 has one config but the value type still defers — same selector might one day resolve multiple ways.

**`native.*` is the legacy macro API.** Pre-`.bzl`, BUILD files used built-in functions like `glob`. When `.bzl` came along, those functions needed to be reachable from macros (which run during loading), so they got exposed under `native.`. Implement `native.glob`, `native.package_name`, `native.repository_name`. Stub `native.existing_rule(s)` to return `None`/`{}` — order-dependent and rarely critical.

**Provider types are singletons by call site.** A `provider()` call returns a type. Two BUILD files reading `MyInfo` from the same `.bzl` see the same type. Two `.bzl` files defining their own `MyInfo` see distinct types — even with identical fields. Identity by construction site, not by structural shape.

**`Label` has two phases of resolution.** Apparent name (`@foo//pkg:name`) → canonical name (`@@canonical//pkg:name`). The mapping is per-module. Always store canonical internally; convert at the boundary.

## Bzlmod and external repos

**`bazel_dep` populates the workspace's apparent → canonical repo mapping** but Loupe doesn't fetch — DESIGN.md mandates "deps assumed vendored on disk." The mapping uses Bazel 7's `<apparent>+` convention for canonical names (`rules_oci+`, `aspect_bazel_lib+`, …). `bazel_dep(name=X, repo_name=Y)` lets BUILD code reach the dep as `@Y//...` while the canonical name stays `X+`.

**External `.bzl` resolution** consults the `EngineBuilder::external_repo_root` directory: a non-main label `@<canonical>//<pkg>:<file>` resolves to `<external_root>/<canonical>/<pkg>/<file>`. Two natural sources for that directory:

- `bazel mod vendor` (Bazel 8+) — emits a stable `vendor/` tree at the workspace root. Hermetic, commit-friendly.
- `<bazel info output_base>/external/` — Bazel's own fetch cache. Brittle (path is host-dependent) but lets you run Loupe right after `bazel build` without an extra step.

If a label load reaches a non-main repo and `external_repo_root` isn't set, the engine errors with the missing-canonical name embedded.

**Out of scope for v1.** Module extensions (`use_extension`, `use_repo`), MVS version selection, `MODULE.bazel.lock` parsing, registry fetches, override directives. Those land when a future ruleset surfaces a dep on them.

## Common rules to know about

**`rules_oci`** is Loupe's v1 target. Three in-scope rules: `oci_image` (compose layers + manifest), `oci_push` (executable), `oci_load` (the tarball rule, also executable, returns `OutputGroupInfo`). The fourth rule, `oci_image_index`, uses a Starlark configuration transition and is out of scope. Surface used: `ctx.actions.{declare_file,declare_directory,write,run,expand_template,args}`, `ctx.runfiles(transitive_files=depset(File))`, `ctx.toolchains[<type>]`, `DefaultInfo`, `OutputGroupInfo`. See `PHASE6.md` for the full inventory and order of work.

**`rules_python`** uses `PyInfo` as the main provider. The toolchain dance is real but stubbable for v1. `py_binary` builds a runfiles tree; recording the runfiles structure is enough for analysis.

**`rules_go`** uses `GoArchive`/`GoSource`/`GoLibrary`. Heavy on toolchains and transitions. Probably not your v1 target.

**`rules_cc`** has `CcInfo` as a large, assumed-everywhere provider. The C++ toolchain is its own multi-day rabbit hole. Avoid for v1.

**`genrule`** is a native Bazel rule. If needed, implement as a Starlark-defined rule with the right shape (run a shell command, declare outputs). Or skip until needed.

## Performance traps

- Eager depset flattening on access destroys the whole point of depsets.
- String allocation per `ctx.attr.foo` access — intern attr names.
- `Configuration` is part of `AnalysisKey` and gets hashed constantly — keep it cheap.

## Reading Bazel source

`https://github.com/bazelbuild/bazel`. Most of what's relevant lives in:

- `src/main/java/com/google/devtools/build/lib/analysis/` — analysis machinery
- `.../analysis/starlark/` — Starlark API for rules (`StarlarkRuleContext`, etc.)
- `.../packages/` — loading phase (`Package`, `Rule`, attr types)
- `.../skyframe/` — Skyframe keys/functions

Search by API name: if a rule calls `ctx.actions.expand_template`, find `expandTemplate` in `StarlarkRuleContext` and trace from there.

## Reading Buck2 source for reference

`https://github.com/facebook/buck2`. Conceptually closer to Loupe than Bazel:

- `app/buck2_analysis/` — analysis phase
- `app/buck2_interpreter_for_build/` — Starlark builtins for BUILD/.bzl
- `dice/` — the DICE engine itself

Buck2 uses a different rule API than Bazel, so don't copy verbatim, but the patterns for wiring DICE + starlark-rust together are directly relevant.

## Debugging analysis divergences

When the cquery harness shows Loupe diverging from Bazel:

1. Narrow to a single target and a single starlark expression.
2. Capture exact `bazel cquery //x:y --output=starlark --starlark:expr='...'` output.
3. Run Loupe with `RUST_LOG=loupe=debug` and `--trace=//x:y`.
4. Compare. Common culprits: select resolution mismatch, depset ordering, missing provider field, attr default not applied.

## Versioning and stability

starlark-rust and dice are not stable APIs — they evolve with Buck2. Pin to specific versions. Upgrade deliberately; expect breakages on each upgrade.

Pin a Bazel version in the test harness too (via `.bazelversion`). Comparing Loupe against two different Bazel versions can produce different diffs even with identical Loupe code.
