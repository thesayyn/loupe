# Loupe — Implementation Plan

> Phases, milestones, stop gates. Read DESIGN.md and DETAILS.md first.

## Status

Phases 0 through 6 are done. v1 has been declared and the engine analyzes the `rules_oci` fixture end-to-end, including real toolchain resolution (which graduated from "stub it" mid-Phase-6 once `rules_oci` made the wall unavoidable). The bulk-analysis driver (`Engine::analyze_workspace`, `loupe analyze`) and the `DiscoverPackagesKey` that makes it idempotent landed shortly after. Subsequent work happens in named follow-on phases (see "Future phases" at the bottom); the per-phase narrative below is preserved for history.

## Working principles

These apply throughout. Internalize before starting.

- **One phase at a time.** Do not work ahead. If a phase feels too big for one change, split it into commits — but don't roll subsequent phases into the work.
- **Tests live with the code, not after.** If a test is hard to write, the design is probably wrong — surface it.
- **Stop at every 🛑 gate.** Wait for explicit human go-ahead before continuing. Do not assume.
- **Commit messages reference the phase and what changed.** A reviewer should be able to grok the commit from the message alone.
- **When in doubt, ask.** Guessing on Bazel-shaped problems is the most expensive thing you can do.
- **No speculative fields or features.** Forward-compat is handled where DESIGN.md mandates it; everywhere else, YAGNI.
- **Ask before stubbing.** When you encounter API surface DESIGN.md doesn't cover, the default is to let the access raise — that error is a real Tier-1 task. Ask the user before adding a fake-value stub, and explain the trade-off (cquery results lose fidelity; users of the engine get silently-wrong analysis). The only pre-approved stub-shaped seams are documented in DESIGN.md's "Real or fail; never silent stubs" invariant.

## Phase 0 — Skeleton

Initialize the repo as its own standalone project (not nested under any consumer). Set up the workspace with three crates: `loupe-core`, `loupe-cli`, `loupe-test-harness`. Pin dependencies (starlark-rust, dice, tokio, anyhow, thiserror, tracing, plus what the harness needs). Confirm `cargo test` runs.

**Done when:** workspace builds clean, deps fetched, repo committed with DESIGN/PLAN/DETAILS at root.

## Phase 1 — Comparison harness

Build the cquery wrapper and one trivial fixture *before writing any engine code*. The harness shells out to `bazel cquery --output=starlark`, captures output, and diffs structurally against Loupe's (currently empty) output. The first fixture is one custom rule returning one provider.

**Done when:** harness runs against fixture #1 and produces a useful diff report ("Loupe produced nothing; Bazel produced X").

🛑 **STOP for human review.** Confirm the harness shape, the fixture's cquery output structure, and the v1 ruleset target before continuing.

## Phase 2 — Loader

Implement enough loading to evaluate a `BUILD` file with a minimal Starlark environment (no `rule()` yet, no `load()` yet). Deliverables: `Label` type with canonical resolution, `MODULE.bazel` parsing for the repo mapping, workspace walking to discover packages, a `BUILD` file evaluator that captures rule-call shapes. Wire DICE for `ModuleGraphKey`, `PackageKey`, and a stub `BzlLoadKey`.

**Done when:** `engine.list_package()` works on fixture #1.

## Phase 3 — Loading-phase builtins

Implement `load()` end-to-end (which makes `BzlLoadKey` real), then `provider()`, `attr.*`, `rule()` (definition only — calling a rule from BUILD records an `UnconfiguredTarget`, doesn't analyze yet), `aspect()` as a no-op that doesn't error.

**Done when:** a `.bzl` defining a rule + a `BUILD` calling it produces a captured `UnconfiguredTarget`.

🛑 **STOP for human review.** Provider identity model, any `rule()` kwargs being silently dropped, ruleset confirmation.

## Phase 4 — Core value types

`select()` returning `SelectorList` with deferred resolution. `depset` as a real lazy tree with all four orders. Real `glob()` with package boundary respect. `File` for source and generated artifacts (use `bazel-out/<config>/bin/...` paths matching what Bazel produces). `exports_files` and source target resolution.

This is the phase most likely to surface bad-default thinking. Re-read DESIGN.md's invariants section and DETAILS.md's notes on these primitives before starting.

**Done when:** each value type has thorough unit tests including the gotchas DETAILS.md calls out.

## Phase 5 — Analysis

Implement `Configuration`, `AnalysisKey`, the analysis compute function (recursive analysis of label deps, select resolution, ctx construction, rule impl invocation), `Target` value with provider indexing, `ctx` minimum (attr, label, workspace_name).

**Done when:** cquery comparison passes for fixture #1.

🛑 **STOP for human review.** First major milestone — demo to human. Discuss any divergences in cquery output before continuing.

After the stop, build out the rest of `ctx`: `actions.*` recording, `files`/`file`/`executable`, `runfiles`, then stubs for `toolchains`/`fragments`/`var`/`expand_location`/`resolve_command`/`configuration`/`bin_dir`/`genfiles_dir`. Each stub gets a `// STUB:` comment.

## Phase 6 — First real ruleset (`rules_oci`)

v1's ruleset target is `rules_oci`. Pre-read its source to inventory every `ctx.*`, `*_common.*`, and provider type used. Categorize each as implemented / stubbed-OK / needs-work. The inventory itself lands as a doc commit; subsequent commits work through the items one at a time.

A small fixture (one `oci_image` plus the deps it pulls) is the cquery-comparison target. Add `rules_oci` to `DETAILS.md`'s "Common rules to know about" once the inventory is in.

**Done when:** cquery comparison passes for the `rules_oci` fixture (or divergences are documented and accepted). **Done.**

## Future phases (do not start without explicit direction)

Aspect propagation, module extensions that re-run (vs. just reading the lockfile), repository rules, visibility, incremental analysis with file watchers (`loupe-watch`), `loupe-exec`, `loupe-rpc`. Real toolchain resolution started here and graduated into v1; the rest still wait for a target ruleset to make them unavoidable. Each gets its own plan when its time comes.

Split transitions and `ctx.split_attr` are deferred under the same principle: single-output user transitions land as part of the cfg rollout, but `ResolvedDep::Split`/`SplitList` and the per-branch `ctx.split_attr` struct only show up when a fixture exercises them.

## Active follow-on: user-space-first refactor

The two-ruleset milestone (`rules_oci` plus the `rules_js` probe) made it clear that the "stub everywhere" tactic was running out of road: each new ruleset surfaced 5-10 host stubs, fidelity was dropping (silent wildcards masked real bugs), and the marginal cost was rising. Course correction: minimal Rust core, everything else in user-space `.bzl`. New ruleset compatibility lands as a `.bzl` change, not a Rust change.

Working plan (this is the active multi-PR migration; details and risks in `~/.claude/plans/vast-swinging-lynx.md`):

- **Phase 1 — `invoke_rule` bridge.** Introduce `builtins_core/wrappers.bzl::invoke_rule(fn, ctx)`; wire `analysis.rs::run_analysis` to dispatch every rule impl through it. `_wrap_rule_ctx` is identity at this point — pure refactor, no behaviour change. **Foundation for every later phase.** *In flight.*
- **Phase 2 — Move ctx-derived fields into `_wrap_rule_ctx`.** `build_file_path`, `expand_location`, `expand_make_variables`, `resolve_command`, `version_file`, `info_file`, `fragments`, `var`, `configuration`, `target_platform_has_constraint` migrate from `starlark_types/ctx.rs` to user-space. `bin_dir`/`genfiles_dir` revert to plain strings. Rust `ctx` shrinks to engine-essential fields.
- **Phase 3 — Native rules as `rule(...)` in `builtins_core/exports.bzl`.** `filegroup`, `alias`, `genrule`, `test_suite`, `config_setting` get full Starlark impls; the Rust recorders in `native_rules.rs` go. New BUILD-side targets flow through `RuleClass::invoke` like any user-defined rule.
- **Phase 4 — Drop `LenientStruct`.** `_builtins.toplevel.X` becomes strict; every vendored Bazel reference resolves to a real binding or is patched. `config = struct(bool=…, int=…, …)` gets a real implementation in `builtins_core/config.bzl`.
- **Phase 5 — Drop `WildcardStruct`.** Type deleted. `ctx.toolchains[unmodelled]` raises (matches Bazel). Bzlmod `use_extension` keeps a dedicated proxy type. Failure surfaces become the new diagnostics.
- **Phase 6 — Final cleanup.** Remaining no-op stubs (`visibility`, `licenses`, `repository_rule`, `module_extension`, `tag_class`, `package`, `package_group`, `macro`, `transition`, `exec_group`, the 10 native-provider const NoneTypes) move to user-space.

Each phase ships as its own PR. `rules_oci` probe stays at 10/10 across every merge; `rules_js` probe recovers via Tier-2 implementations as wildcards are removed.

Old "stub, don't error" guidance has been replaced — see DESIGN.md's "Vision and the expansion principle" section. The old principle stands only for the *forward-compat* sense: keep dimensions plumbed even when degenerate. Do not stub at the API surface anymore; route through Tier-2 user-space `.bzl` instead.

Multi-config / transitions, aspect propagation, etc. remain deferred — but the user-space migration must not foreclose them. Phase 2's `_wrap_rule_ctx` shape uses the `__configuration`/`__fragments` hidden-attr convention, which is exactly how transitions will plug in later.

## Cross-cutting requirements

These apply at every phase, not as separate work:

- **Tracing.** Every DICE compute function gets a tracing span. Every error logged with context. `RUST_LOG=loupe=debug` should produce a useful trace.
- **Errors.** Every error includes what failed, where in user code (preserve Starlark `Diagnostic` source spans), and why. Never `unwrap()` on Starlark `Result`s.
- **Determinism.** Sort `HashMap` iterations or use `BTreeMap`. Sort `glob` results and directory listings. Anything user-visible has stable order.
- **Forward-compat.** Before removing or simplifying anything, ask: am I closing a future door? If yes, document the door explicitly.
- **Debugging infrastructure built early.** A `--trace=<label>` mode showing the analysis chain. A `dump-package` CLI command. You'll need these while building correctness, not after.
