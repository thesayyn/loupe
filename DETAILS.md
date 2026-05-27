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

## The `invoke_rule` indirection

The engine never calls a rule's `implementation` callback directly. Instead, `analysis.rs::run_analysis` looks up `invoke_rule` from the embedded `builtins_core/wrappers.bzl` and dispatches `invoke_rule(impl_fn, ctx)`. That user-space wrapper is `def invoke_rule(fn, ctx): return fn(_wrap_rule_ctx(ctx))`.

Why the layer matters: it's the seam where Starlark-side ctx-derived fields land. `_wrap_rule_ctx` builds a struct from the raw Rust `ctx` plus extras — `build_file_path`, `expand_make_variables`, `runfiles` builders, `genfiles_dir` (an alias of `bin_dir`), `coverage_instrumented`, `configuration`/`fragments`/`var` (when the rule declares them), `target_platform_has_constraint` (when the rule reads it). Every one of those used to be (or would have been) a per-call Rust stub; with the wrapper they're plain Starlark.

Engine plumbing: `crate::builtins::invoke_rule_callable()` returns the `FrozenValue` and its owning `FrozenModule`; the analysis driver `add_reference`s the latter onto the analysis module's frozen heap, then `eval_function(invoke_rule_fv, [impl_fv, ctx_value], &[])`. **Don't** bypass this path. Anyone tempted to call the impl directly should add fields to `_wrap_rule_ctx` instead.

The same pattern is intended for subrules (`invoke_subrule`) and aspects when those phases land.

## Tier-2 user-space wrappers

`crates/loupe-core/src/builtins/bzl/builtins_core/` is where Loupe's user-space compat helpers live. New helpers default here; only fall back to Tier 1 (Rust) when the work touches DICE/the heap/the filesystem.

Files:

- `wrappers.bzl` — `invoke_rule`, `_wrap_rule_ctx`, `_wrap_actions`, future subrule wrappers.
- `exports.bzl` — provider type definitions (`DefaultInfo`, `OutputGroupInfo`, `InstrumentedFilesInfo`, `PlatformInfo`, …) AND native rules as `rule(...)` calls (`filegroup`, `alias`, `genrule`, `test_suite`, `config_setting`).
- `config.bzl` — the `config = struct(bool=…, int=…, string=…, string_list=…, label_list=…, exec=…, none=…)` namespace `bazel_skylib//rules:common_settings.bzl` reaches for.

The root `bzl/wrappers.bzl` aggregates and re-exports these for auto-injection. Adding a new helper means: drop the function in the right `builtins_core/` file, re-export from the root, and the next eval picks it up — no Rust change.

## DICE patterns

**Async dependencies.** Inside a compute function, `ctx.compute(other_key).await` records the dependency edge automatically. No manual dep tracking — the framework does it via the await graph.

**Don't share `Heap` across compute calls.** `Heap` is not `Sync`. Each compute evaluates into its own `Heap`, freezes the result, returns the `FrozenValue` (whose `FrozenHeap` is `Sync`). Agents will sometimes try to "optimize" by sharing a heap; this breaks parallelism and is wrong.

**Equality drives change propagation.** If a key recomputes to a value equal to its previous value, downstream keys aren't invalidated. Implement value `PartialEq` carefully.

**Versioning at v1 is trivial.** Bump the DICE version on every Loupe invocation; recompute everything. Don't try to be clever about incrementality at v1 — file-content keys + a watcher come later.

## Bazel APIs with weird histories

**`depset` exists because of memory.** Early Bazel passed plain lists through providers; large C++ projects exploded. Depsets share structure transitively. The lazy tree shape is the entire point — flattening eagerly negates the win.

**`select()` is a value because attrs need to be configurable per-config.** A `srcs = select({...})` would be useless if it resolved at load time, because the same BUILD file's targets get analyzed under multiple configs. So `select()` returns a value that survives into analysis. v1 has one config but the value type still defers — same selector might one day resolve multiple ways.

**`native.*` is the legacy macro API.** Pre-`.bzl`, BUILD files used built-in functions like `glob`. When `.bzl` came along, those functions needed to be reachable from macros (which run during loading), so they got exposed under `native.`. Loupe keeps `native.glob`, `native.package_name`, `native.repository_name` in Tier 1 (Rust); `native.existing_rule(s)` is stubbed to `None`/`{}` (order-dependent and rarely critical). `native.config_setting`, `native.test_suite`, `native.alias`, `native.filegroup`, `native.genrule` land in Tier 2 (`builtins_core/exports.bzl`) as ordinary `rule(...)` definitions reachable through the `native` namespace bindings.

**Provider types are singletons by call site.** A `provider()` call returns a type. Two BUILD files reading `MyInfo` from the same `.bzl` see the same type. Two `.bzl` files defining their own `MyInfo` see distinct types — even with identical fields. Identity by construction site, not by structural shape.

**`Label` has two phases of resolution.** Apparent name (`@foo//pkg:name`) → canonical name (`@@canonical//pkg:name`). The mapping is per-module. Always store canonical internally; convert at the boundary.

## Bzlmod and external repos

**`bazel_dep` populates the workspace's apparent → canonical repo mapping** but Loupe doesn't fetch — DESIGN.md mandates "deps assumed vendored on disk." The mapping uses Bazel 7's `<apparent>+` convention for canonical names (`rules_oci+`, `aspect_bazel_lib+`, …). `bazel_dep(name=X, repo_name=Y)` lets BUILD code reach the dep as `@Y//...` while the canonical name stays `X+`.

**External `.bzl` resolution** is fully in-process: configure `with_bcr()` (or `with_registry(...)`) plus `with_bzlmod_cache(<dir>)` and the engine resolves `bazel_dep` declarations against the registry, fetches archives over HTTP into the cache, and serves `@<canonical>//<pkg>:<file>` reads from `<cache>/<canonical>/<pkg>/<file>`. Module-extension impls run in-process too, emitting their generated repos with full `(owner, ext, repo)` structural info so nothing downstream has to parse canonical names back into parts.

If a label load reaches a non-main repo and the engine has no `RepoSpec` for that canonical (no registry configured, or the resolver couldn't reach it), the load errors with the missing-canonical name embedded.

There is no `bazel mod vendor` mode and no `<apparent>+` fallback: every cross-repo reference must resolve through the per-module `repo_mapping` the resolver assembles, or be one of the hardcoded native repos (`bazel_tools`, `platforms`, `local_config_platform`).

**`use_repo_rule` is an innate module extension.** Bazel implements a direct repo-rule call in MODULE.bazel (`r = use_repo_rule(bzl, rule); r(name=…, …)`) as an "innate" module extension — a synthetic extension whose name carries a space and whose generated repo canonical is `<declaring_module><sep><rule_name><sep><repo_name>` (`+my_repo+hello` for a root-module call, since main's canonical is the empty string). Loupe mirrors that: each `proxy(name=…, **attrs)` invocation is captured (`RepoRuleInvocation`) on the per-module record in the `BzlmodGraph` snapshot, `derive_extension_demand` synthesises one innate `ExtensionId` per `(declaring module, defining_bzl, rule_name)` and seeds the `owner_index`, and `ModuleExtensionKey::compute` has an innate branch that skips the `module_extension` impl, loads the `repository_rule` symbol instead, and promotes each invocation's literal attrs straight to a `RepoSpec::StarlarkRule`. Materialisation then flows through the same `maybe_materialise → ExtMaterializedRepoKey` path real extensions use. Repos are module-local: the `name` binds `apparent → canonical` only in the declaring module's repo mapping. Handled for both the root MODULE.bazel (`module_graph.rs`) and transitive modules (the lenient parser in `resolution.rs`). The implicit `dev_dependency` proxy attribute is accepted and dropped (not forwarded to the rule); v1 doesn't gate transitive innate repos on it.

**Out of scope for v1.** `single_version_override`, `multiple_version_override`, `archive_override`, and similar override directives. MVS version selection is implemented in the in-process resolver; registry fetches are too. Module extensions (`use_extension`, `use_repo`) are re-run against the live workspace — their generated repos materialise into the bzlmod cache and get full structural metadata (owner, ext_name, repo_short) at construction time.

## Common rules to know about

**`rules_oci`** is Loupe's v1 target. Three in-scope rules: `oci_image` (compose layers + manifest), `oci_push` (executable), `oci_load` (the tarball rule, also executable, returns `OutputGroupInfo`). The fourth rule, `oci_image_index`, uses a Starlark configuration transition and is out of scope — skip it via `Engine::analyze_workspace(&["oci_image_index"])` or `loupe analyze --skip-kind oci_image_index`. Surface used: `ctx.actions.{declare_file,declare_directory,write,run,expand_template,args}`, `ctx.runfiles(transitive_files=depset(File))`, `ctx.toolchains[<type>]`, `DefaultInfo`, `OutputGroupInfo`.

**`rules_python`** uses `PyInfo` as the main provider. The toolchain dance is real but stubbable for v1. `py_binary` builds a runfiles tree; recording the runfiles structure is enough for analysis.

**`rules_go`** uses `GoArchive`/`GoSource`/`GoLibrary`. Heavy on toolchains and transitions. Probably not your v1 target.

**`rules_cc`** has `CcInfo` as a large, assumed-everywhere provider. The C++ toolchain is its own multi-day rabbit hole. Avoid for v1.

**`genrule`** is a native Bazel rule. If needed, implement as a Starlark-defined rule with the right shape (run a shell command, declare outputs). Or skip until needed.

## Performance traps

- Eager depset flattening on access destroys the whole point of depsets.
- String allocation per `ctx.attr.foo` access — intern attr names.
- `Configuration` is part of `AnalysisKey` and gets hashed constantly — keep it cheap.

## Bazel-identical Action Cache (AC) keys

`crate::ac_key` computes REAPI v2 `Action` digests for recorded `Run`/`RunShell` actions. The intent is that a Loupe-computed digest is byte-for-byte identical to what Bazel writes to a remote action cache for the same action — so downstream tooling can look up cache entries, answer "would this hit cache?" without running Bazel, or speak to the same `bytestream.googleapis.com` Bazel does.

The whole thing is small: REAPI v2 is a handful of protobuf messages (`Action`, `Command`, `Directory`, `FileNode`, `DirectoryNode`, `SymlinkNode`, `Digest`, `Platform`) and proto3 has a canonical wire format. The module hand-rolls the wire encoders (no `prost` / `protobuf` dep) for the fields Loupe actually populates. Unit tests inside `ac_key.rs` pin three REAPI fixed points (empty Command/Directory/Action digests) so any encoder drift is loud.

What's *not* in scope for the AC-key module (these are v1 design decisions, not bugs in the encoder):

- **Workspace-name prefix in exec_paths.** Bazel's execroot lays main-repo sources under `<workspace_name>/<pkg>/<short>`; Loupe's `File::path()` drops that segment. Same caveat applies to external repos (`external/<canonical>/...`).
- **`fake-cfg` config segment.** Bazel uses a content-derived hash (`k8-fastbuild`); Loupe uses a fixed placeholder. Generated-input exec_paths therefore differ from Bazel's.
- **`Platform.properties`.** Loupe doesn't model `exec_properties` or platform-derived constraints as REAPI Platform properties yet — the field stays empty.
- **`is_executable` bit on inputs.** Loupe's `File` doesn't track POSIX mode; `FileNode.is_executable` is always `false`. Affects toolchain-binary inputs (`chmod +x`-shaped).
- **Generated-input content.** Loupe records actions, doesn't execute. For inputs whose content is derivable from analysis alone (`Write`/`Symlink`/`ExpandTemplate` outputs) the caller pre-walks the action graph and stuffs `KnownContentResolver::generated`. For inputs that are `Run`/`RunShell` outputs of upstream actions: `AcKeyError::UnknownGenerated` — that's the `loupe-exec` boundary, surfaced explicitly rather than silently substituted.

`tests/ac_key_e2e.rs` drives the analysis engine, harvests a recorded `Action`, and computes its AC key. The pipelined-write-then-run test demonstrates the cross-action-handoff pattern that makes the whole thing tractable without an executor.

REAPI proto source of truth: `https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto`. Field numbers in `ac_key.rs::pb` are pinned to that file.

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
