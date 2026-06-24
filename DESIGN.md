# Loupe — Design

## What Loupe is

A read-only, analysis-only Bazel-compatible analysis engine in Rust, built on DICE and starlark-rust. Loupe loads a Bzlmod workspace, runs Bazel's loading and analysis phases, and produces a configured target graph with providers and recorded actions, exposed as live Starlark values.

Loupe does not execute actions. It is not a build system. It *is* aiming to be a replacement for `bazel query`, `bazel cquery`, and `bazel aquery` — pure analysis, no execution — though not a drop-in one: the surface is a Rust API over live frozen Starlark values, not a CLI that mimics Bazel's flags and output formats one-to-one. The point is to have analysis results live in your own process with no IPC tax, while still answering the questions those query tools answer.

## v1 scope

Small Bzlmod workspaces. v1's ruleset target is `rules_oci` — a small, real Bazel ruleset whose use-list (file/depset/runfiles plumbing, a handful of providers, no toolchain-resolution rabbit-hole) matches what Loupe ships. Single configuration, no transitions. Aspects accepted syntactically but not propagated. All deps assumed vendored on disk.

## Vision and the expansion principle

v1 is deliberately narrow. The architecture must not preclude later growth: real toolchain resolution, aspect propagation, multiple configurations and transitions, visibility enforcement, repository fetching, action execution (in a separate `loupe-exec` crate), incremental analysis with file watchers, cross-process RPC.

The single principle to internalize: **fail closed; user-space first; stub only at the host boundary.** Loupe keeps a *small* Rust core (~30 primitives — `provider`, `rule`, `aspect`, `attr.*`, `depset`, `select`, `struct`, the raw `ctx`, `File`, `Label`, `glob`, `exports_files`) and layers everything else in user-space `.bzl`. New ruleset compatibility lands as a `.bzl` change, not a Rust change. When a value can't be produced, raise — don't return a wildcard placeholder. The previous "stub everywhere" stance led to per-ruleset Rust patches with rapidly diminishing fidelity; the small-core, user-space-first split eliminates that whole category of work.

The boundary has **two** criteria, not one. A value belongs to the core when it's tied to DICE/the heap/the filesystem — *or* when the engine itself must introspect it: downcast it, match its identity, or read its fields. The second criterion is what keeps the core *honest* rather than merely *small*. "User-space first" is the default, not an absolute: a provider the analysis driver reads to wire the graph (`DefaultInfo`) must have an identity the engine *owns*, even when its constructor logic stays in `.bzl`. Pushing such a value entirely into user-space and then reaching back across the boundary by symbol name or by stringifying its repr is not thinness — it's a hidden coupling that reads as a stub. See "The engine owns what it introspects" under the invariants.

Plumbed-but-degenerate dimensions still hold: `AnalysisKey` carries a `config` field even with one config; action records carry every field a future executor would need; closing a future door for v1 convenience is the worst thing the agent can do.

## Output compatibility — match Bazel, do not reinvent

Loupe exists to produce output comparable to `bazel cquery` on the same workspace. **Use Bazel's own conventions throughout.** Paths are `bazel-out/<config>/bin/<package>/<file>` with the config segment matching what Bazel produces (obtain via `bazel info` on the fixture if needed). Provider type names are the real ones (`DefaultInfo`, `CcInfo`, etc.). Workspace name comes from `MODULE.bazel`. Do not invent Loupe-specific naming for anything visible in output — we are mimicking Bazel, not replacing it.

The cquery comparison harness is the ground truth for what "compatible" means in any specific case. When in doubt, run cquery and match.

## Architecture

starlark-rust handles Starlark eval and value types. DICE handles the computation graph. Loupe is the layer between, providing Bazel-flavored builtins, the loader, and the analyzer. Public API (`Engine`, `Analysis`, `Target`) is small and in Loupe's vocabulary, not any consumer's.

Crates: `loupe-core` (engine, no consumer awareness), `loupe-cli` (debugging), `loupe-test-harness` (cquery comparison; shells out to `bazel`, kept separate so its deps don't reach core). Future crates already named: `loupe-exec`, `loupe-rpc`, `loupe-watch`. Loupe lives in its own repo from day one, never as a subdirectory of a consumer.

### Tier 1 / Tier 2 / Tier 3 split

The Bazel-compat surface a user `.bzl` reaches for spans three tiers. Knowing which tier to add a new piece to is the central architectural call:

- **Tier 1 — Rust** (`loupe-core/src/starlark_types/`): the *minimal* engine surface. Provider/rule/aspect constructors, `attr.*` shapes, `select`/`depset`/`struct`/`Label`, `glob` (filesystem walk), `exports_files`, the raw `ctx` value (`attr`, `label`, `workspace_name`, `actions`, `files`, `file`, `executable`, `outputs`, `toolchains`, `exec_groups`, `bin_dir` as a string), `ctx.actions.*`, `File`, the `_builtins` injection mechanism. ~30 primitives, no more. New surface lands here only if it's tied to DICE/the heap/the filesystem, or if the engine itself must introspect the value (downcast it, match its identity, read its fields) — see "The engine owns what it introspects."

  **Don't wrap plain Starlark shapes in custom Rust types.** When a value is naturally `dict[k, v]`, `struct(...)`, or `list[v]`, allocate it directly as a Starlark dict/struct/list — don't add a `ProvidesStaticType` wrapper just to give it a name. Every custom wrapper forces a boundary crossing (`downcast_ref` → Rust method dispatch → `heap.alloc` back to `Value`) on each access; plain Starlark values stay inside the VM. Wrap only when the type carries *engine-side* invariants the Starlark side can't enforce: `ProviderInstance` tracks `type_ptr` identity and computed_fields fill-in; `Target` carries an `Arc<AnalyzedTarget>` whose frozen heap must stay anchored on the consumer's heap.
- **Tier 2 — Loupe-authored Starlark** (`loupe-core/src/builtins/bzl/builtins_core/`): the wrappers between engine and rule logic. `wrappers.bzl` defines `invoke_rule(fn, ctx)` (the engine's only entry into a rule's impl); `_wrap_rule_ctx` adds Starlark-derived ctx fields (`build_file_path`, `expand_make_variables`, `runfiles` builders, `genfiles_dir`, …); `exports.bzl` defines provider types and native rules (`filegroup`, `alias`, `genrule`, `test_suite`, `config_setting`) as ordinary `rule(...)` calls. New compat helpers go here unless they truly need engine state.
- **Tier 3 — Vendored Bazel** (`loupe-core/src/starlark/{builtins_bzl,bazel_tools}/`): unmodified upstream `.bzl` files, tracking **current Bazel master** (post-starlarkification — the cc/objc/python/java rule implementations now live in `rules_cc`/`rules_python`/… and resolve through the registry, *not* here). Never edited locally; bumped wholesale on upstream change. Removed rules resolve to a real failure handler ("add a `load()`"), never a fabricated value. **The vendored tree is never propped up with wildcard stubs to make it freeze.** If it reaches for host machinery Loupe doesn't model, that access fails (real-or-fail) — it does not return a `WildcardStruct`. Vendoring the *pre*-starlarkification tree (13.5k lines of cc rule impls) and then reimplementing `cc_common`/`cc_internal` in Starlark to satisfy it — the bonanza strategy — is exactly the trap this rule forbids.

The `_builtins` injection mechanism (`crate::builtins::inject_builtins_into`) makes Tier-2 names reachable from every user `.bzl`/BUILD as if they were native. The vendored tree (Tier 3) sees Tier-2 names through the same injection.

### Host boundary

`glob()` and `exports_files()` stay in Tier 1 (Rust) because they're tied to Loupe's filesystem walk; everything else — `config.bool/int/string/...`, `expand_make_variables`, `coverage_instrumented`, native-rule constructors — sits in Tier 2 user-space.

## Phases

**Loading.** Parse `MODULE.bazel` (don't fetch — assume vendored), discover packages, evaluate `.bzl` files topologically into frozen modules, evaluate `BUILD` files into unconfigured target dicts.

**Analysis.** Given a label, look up the unconfigured target, resolve `select()` against the configuration, recursively analyze label-typed deps, build a `ctx`, run the rule's impl function, freeze and cache the resulting providers and actions.

**Execution.** Not in v1. Eventually `loupe-exec` consumes `Analysis` values; engine unchanged.

## DICE keys

`ModuleGraphKey`, `BzlLoadKey(label)`, `DiscoverPackagesKey`, `PackageKey(pkg)`, `MaterialisedAttrsKey(label, config)`, `ResolvedToolchainsKey(label, config)`, `TargetPlatformConstraintsKey(config)`, `AnalysisKey { label, config }`. Keys are always canonical labels; apparent-name conversion happens at the parsing boundary, never inside the engine. `config` is plumbed through every per-target key even with one config. Future keys to keep in mind but not implement: `AspectKey`, `ActionExecutionKey`, `RepoKey`.

DICE compute functions must be deterministic. Sort at every boundary where iteration order would otherwise leak.

## The non-negotiable invariants

These are the things agents most reliably get wrong; if any slip past review, the engine is broken in subtle ways.

- **`Label` is a real type, not a string.** Hash and equality by canonical form. Methods for relative resolution. Display in canonical form.
- **`depset` is a lazy tree, not a flat collection.** Direct items + transitive children + order. Flatten only on iteration. Dedupe per the order's traversal. Implement all four orders (default/postorder, preorder, topological).
- **`select()` returns a value, not a resolved result.** Resolution happens during analysis, not loading. Selectors must survive across the loading→analysis boundary with their full branch dict intact (future multi-config will re-resolve the same selector multiple times).
- **Provider types are singletons by call site.** Identity is `FrozenValue` pointer equality, established when `provider()` is called in a `.bzl`. Two `.bzl` files defining their own `MyInfo` produce distinct types even with identical fields.
- **The engine owns what it introspects; it never recognizes or reads a provider by string.** When the analysis driver needs a provider's identity or fields, it holds the canonical `FrozenValue` and compares by pointer. `provider_by_symbol("DefaultInfo")` (symbol-name recognition) and `field.to_value().to_string().trim_matches('"')` (repr scraping for a typed value) are bugs, not idioms — they make core correctness depend on a string and on Starlark's print formatting.

  The deeper rule this serves: **native (Rust) iff the engine itself reads the value** — it is tied to DICE/the heap/the filesystem, *or* the engine introspects it. There are two kinds of would-be-native Bazel code, and conflating them is the trap: *handles* — the values the analysis driver reads to wire the graph (the DICE key, the `target[P]` index, the provider it reads for default outputs) — must be native, because for a handle "not native" does not mean "in Starlark," it means "reached back by string/repr," the very leak this invariant bans; *algorithms* — computations over values (`expand_location`, select resolution, make-var expansion, all of `cc_common`) — belong in Starlark unless inseparable from a handle. The **native-3** this yields: `Configuration` (the typed spine and DICE key), the `RuleContext` (reads engine state on every target — derived algorithms included), and `DefaultInfo` (the driver constructs *and* reads it). Everything else Bazel-shaped is user-space `.bzl`, or absent.

  Two cases, distinguished by *who owns the type*:
  - **Loupe's own analysis ABI** (`DefaultInfo`, `OutputGroupInfo`, `InstrumentedFilesInfo`): the core owns the canonical provider-type identity and stamps it on *every* instance — including engine-synthesised ones (source files, aliases, the empty fallback) — so identity-match never falls back to a name. Engine-owned identity is the *floor*, not the ceiling: the constructor / computed-field logic may stay in `.bzl`, but a fuller Rust-side implementation of one of these types is a **sanctioned exception** when it makes the Rust side easier to reason about. `DefaultInfo` is the expected case — the analysis driver reads it on every target — so a native Rust `DefaultInfo` is acceptable if it keeps `analysis.rs` clearer than reaching into a `.bzl` value.
  - **Ruleset-owned types** (`BuildSettingInfo` from `bazel_skylib`): the core must *not* own the type, and must *not* make a core operation depend on the ruleset being vendored. It resolves the value it needs from its own typed state (e.g. a typed, `Label`-keyed build-settings map on `Configuration`) and, where it genuinely must read the foreign provider, reads typed fields — never a symbol lookup or a stringified repr.
- **The analysis heap of every `AnalyzedTarget` stays alive for the lifetime of the engine.** Provider field values, declared `File`s, and recorded action arguments are stored as live `FrozenValue`s into the analysis-output `FrozenModule` — `AnalyzedTarget` carries an `Arc<FrozenModule>` to keep that heap pinned. Downstream rules that read a dep via `target[Provider]` or `ctx.attr.<dep>` anchor that heap on their own analysis module via `frozen_heap().add_reference(...)`. Rust-side `AttrValue` is for BUILD-time captured shapes only — it does not round-trip provider data, because that would lose the typed fidelity (lists of `File`, `depset`s, structs) `cquery` consumers need.
- **`FrozenValue`s flow across DICE compute boundaries through the `add_reference` discipline.** Rust-native types (no Starlark pointers) is the *general* DICE rule, but the `BzlLoadKey → LoadedBzl { module: Arc<FrozenModule> }` and `AnalysisKey → AnalyzedTarget { analysis_heap: Arc<FrozenModule> }` paths intentionally carry frozen Starlark heaps. Each downstream consumer must `add_reference` on the upstream frozen heap before dereferencing any `FrozenValue` from it.
- **Real or fail; never silent stubs.** When v1 doesn't model a Bazel primitive (`ctx.target_platform_has_constraint`, `ctx.expand_make_variables`, `ctx.configuration.default_shell_env`, `ctx.version_file`), the access raises so the gap is visible — not a structurally-valid-but-wrong default. Silent stubs (`always-False`, `empty-default`, `fake paths`) make cquery results lie about the analyzed target and mask real Tier-1 work. `WildcardStruct`-style values — that return another wildcard from any field access or call so unmodeled code "just freezes" — are the most corrosive form and are **banned**; an unmodeled access must fail, not absorb. *Acceptable* seams where a non-raising path is fine:

  - **Pure aliases** that reflect Loupe's modelling choices, not unmodelled state (`File.label` aliases `File.owner` because Loupe doesn't track per-action owner labels separately; `Target.original_label` aliases `Target.label` because Loupe doesn't model configuration transitions yet).
  - **Synthesised providers that replicate Bazel's real shape** end-to-end (source-file `DefaultInfo` populates the canonical field set; the rule impl that consumes it sees the same shape it would from a real impl) — *and* carry the canonical provider **identity**, not a symbol-only stand-in, so the engine matches them by pointer (see "The engine owns what it introspects").
  - **Vendored-wrapper conformance** where the vendored upstream rewrites the user's call into a different primitive shape (`actions.run(executable=str)` accepts a string because `wrappers.bzl::actions_run_shell` rewrites `run_shell` to `actions.run(executable="/bin/bash", …)`).

  Anything else — especially fake-state stubs on `ctx.*` host primitives — must be **asked about before being added**. Default to letting the access raise; that error is a real Tier-1 task surfacing itself.

  Toolchain access is the canonical example of this policy applied. `ResolvedToolchainsKey` walks `register_toolchains`, picks a matching `toolchain` rule, and surfaces its `ToolchainInfo`; unresolved `ctx.toolchains[T]` raises rather than returning a wildcard, exposing the gap so the resolver gets fixed (not papered over). The invariant generalises that pattern to every host primitive.
- **Preserve Starlark `Diagnostic` source spans.** Errors should point at the user's `BUILD`/`bzl` file, not at Loupe's source.

## Public API shape

```rust
pub struct Engine { /* ... */ }
impl Engine {
    pub fn new(workspace_root: PathBuf, config: Configuration) -> Result<Self>;
    pub fn analyze(&self, label: &Label) -> Result<Analysis>;
    pub fn analyze_transitive(&self, roots: &[Label]) -> Result<Vec<Analysis>>;
    pub fn list_package(&self, package: &PackagePath) -> Result<Vec<Label>>;
}

pub struct Analysis {
    pub label: Label,
    /// Captured providers, indexed by provider type identity (the
    /// `(defining_bzl, exported_symbol)` pair stamped at `provider()`
    /// call time — see DETAILS.md). The Starlark surface is
    /// `target[Foo]` lookup, not a struct of named fields.
    pub providers: Vec<CapturedProvider>,
    pub actions: FrozenValue,    // list of Action values
}
```

Small surface, Loupe's vocabulary, frozen Starlark values as the boundary type. Consumers traverse the heap directly.

## Testing

The cquery comparison harness is mandatory and exists before any engine code. It runs `bazel cquery --output=starlark` on a fixture and compares against Loupe's output structurally. This is the feedback loop that keeps the agent honest.

Fixtures progress from a single custom rule to a small real-rules project. Each new fixture surfaces the next batch of missing builtins.

## What not to implement at v1

Configuration transitions (single config), aspect propagation (accept syntax, don't propagate), repository rules and module extensions beyond reading their lockfile output (no re-running extensions, no fetches), visibility enforcement (everything public), output groups beyond `DefaultInfo.files` (return empty for unknown groups), action execution (record only). Each row's expansion is additive when its time comes — none requires a rewrite.

Toolchain resolution started on this list and graduated mid-v1 once `rules_oci` made the wall unavoidable. Treat that as the precedent: if a target ruleset can't be analyzed without a feature here, the feature gets promoted, but the rest stay stubbed until they similarly block.

## Things the agent will probably try and should not

Implementing `depset` as `Vec` or `HashSet`. Resolving `select()` at BUILD evaluation time. Eagerly computing template expansions. Treating `Label` as a string. Sharing a `Heap` across DICE compute calls (`Heap` is not `Sync`). `.unwrap()` on Starlark `Diagnostic`s. Adding consumer-specific types to `loupe-core`. Optimizing before correctness. Combining multiple PLAN.md phases in one change. Closing future doors for v1 convenience.

## When the agent is confused

Order of authority: this document, then cquery output, then https://bazel.build/rules, then Bazel's source. If still unclear after all four, stub conservatively, document the stub in code with a `// STUB:` comment, flag it in the PR description. Do not guess and ship.
