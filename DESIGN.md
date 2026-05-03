# Loupe — Design

## What Loupe is

A read-only, analysis-only Bazel-compatible analysis engine in Rust, built on DICE and starlark-rust. Loupe loads a Bzlmod workspace, runs Bazel's loading and analysis phases, and produces a configured target graph with providers and recorded actions, exposed as live Starlark values.

Loupe does not execute actions. It is not a build system. It is not a query tool replacement; it exists for cases where you want analysis results live in your own process with no IPC tax.

## v1 scope

Small Bzlmod workspaces. A handful of common rulesets — start with `rules_python` or hand-written rules; confirm the v1 ruleset target with the human before pre-reading. Single configuration, no transitions. Aspects accepted syntactically but not propagated. All deps assumed vendored on disk.

## Vision and the expansion principle

v1 is deliberately narrow. The architecture must not preclude later growth: real toolchain resolution, aspect propagation, multiple configurations and transitions, visibility enforcement, repository fetching, action execution (in a separate `loupe-exec` crate), incremental analysis with file watchers, cross-process RPC.

The single principle to internalize: **stub, don't error; plumb the dimension even when degenerate; preserve future doors at the cost of slight v1 awkwardness.** `AnalysisKey` carries a `config` field even with one config. Toolchain access returns dummy values rather than refusing. Action records carry every field a future executor would need. Closing a door for v1 convenience is the worst thing the agent can do.

## Output compatibility — match Bazel, do not reinvent

Loupe exists to produce output comparable to `bazel cquery` on the same workspace. **Use Bazel's own conventions throughout.** Paths are `bazel-out/<config>/bin/<package>/<file>` with the config segment matching what Bazel produces (obtain via `bazel info` on the fixture if needed). Provider type names are the real ones (`DefaultInfo`, `CcInfo`, etc.). Workspace name comes from `MODULE.bazel`. Do not invent Loupe-specific naming for anything visible in output — we are mimicking Bazel, not replacing it.

The cquery comparison harness is the ground truth for what "compatible" means in any specific case. When in doubt, run cquery and match.

## Architecture

starlark-rust handles Starlark eval and value types. DICE handles the computation graph. Loupe is the layer between, providing Bazel-flavored builtins, the loader, and the analyzer. Public API (`Engine`, `Analysis`, `Target`) is small and in Loupe's vocabulary, not any consumer's.

Crates: `loupe-core` (engine, no consumer awareness), `loupe-cli` (debugging). Future crates already named: `loupe-exec`, `loupe-rpc`, `loupe-watch`. Loupe lives in its own repo from day one, never as a subdirectory of a consumer.

## Phases

**Loading.** Parse `MODULE.bazel` (don't fetch — assume vendored), discover packages, evaluate `.bzl` files topologically into frozen modules, evaluate `BUILD` files into unconfigured target dicts.

**Analysis.** Given a label, look up the unconfigured target, resolve `select()` against the configuration, recursively analyze label-typed deps, build a `ctx`, run the rule's impl function, freeze and cache the resulting providers and actions.

**Execution.** Not in v1. Eventually `loupe-exec` consumes `Analysis` values; engine unchanged.

## DICE keys

`ModuleGraphKey`, `BzlLoadKey(label)`, `PackageKey(pkg)`, `AnalysisKey { label, config }`. Keys are always canonical labels; apparent-name conversion happens at the parsing boundary, never inside the engine. `config` is plumbed through `AnalysisKey` even with one config. Future keys to keep in mind but not implement: `AspectKey`, `ToolchainKey`, `ActionExecutionKey`, `RepoKey`.

DICE compute functions must be deterministic. Sort at every boundary where iteration order would otherwise leak.

## The non-negotiable invariants

These are the things agents most reliably get wrong; if any slip past review, the engine is broken in subtle ways.

- **`Label` is a real type, not a string.** Hash and equality by canonical form. Methods for relative resolution. Display in canonical form.
- **`depset` is a lazy tree, not a flat collection.** Direct items + transitive children + order. Flatten only on iteration. Dedupe per the order's traversal. Implement all four orders (default/postorder, preorder, topological).
- **`select()` returns a value, not a resolved result.** Resolution happens during analysis, not loading. Selectors must survive across the loading→analysis boundary with their full branch dict intact (future multi-config will re-resolve the same selector multiple times).
- **Provider types are singletons by call site.** Identity is `FrozenValue` pointer equality, established when `provider()` is called in a `.bzl`. Two `.bzl` files defining their own `MyInfo` produce distinct types even with identical fields.
- **Stubs over crashes.** Toolchain access, fragments, `expand_location`, `resolve_command` — return structurally valid dummy values. Never refuse to evaluate.
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
    pub providers: FrozenValue,  // struct of provider type -> instance
    pub actions: FrozenValue,    // list of Action values
}
```

Small surface, Loupe's vocabulary, frozen Starlark values as the boundary type. Consumers traverse the heap directly.

## Testing

The cquery comparison harness is mandatory and exists before any engine code. It runs `bazel cquery --output=starlark` on a fixture and compares against Loupe's output structurally. This is the feedback loop that keeps the agent honest.

Fixtures progress from a single custom rule to a small real-rules project. Each new fixture surfaces the next batch of missing builtins.

## What not to implement at v1

Toolchain resolution (stub it), configuration transitions (single config), aspect propagation (accept syntax, don't propagate), repository rules and module extensions (assume vendored), visibility enforcement (everything public), output groups beyond `DefaultInfo.files` (return empty for unknown groups), action execution (record only). Each row's expansion is additive when its time comes — none requires a rewrite.

## Things the agent will probably try and should not

Implementing `depset` as `Vec` or `HashSet`. Resolving `select()` at BUILD evaluation time. Eagerly computing template expansions. Treating `Label` as a string. Sharing a `Heap` across DICE compute calls (`Heap` is not `Sync`). `.unwrap()` on Starlark `Diagnostic`s. "Implementing" toolchain resolution when it should be stubbed. Adding consumer-specific types to `loupe-core`. Optimizing before correctness. Combining multiple PLAN.md phases in one change. Closing future doors for v1 convenience.

## When the agent is confused

Order of authority: this document, then cquery output, then https://bazel.build/rules, then Bazel's source. If still unclear after all four, stub conservatively, document the stub in code with a `// STUB:` comment, flag it in the PR description. Do not guess and ship.
