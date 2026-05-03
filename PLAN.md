# Loupe — Implementation Plan

> Phases, milestones, stop gates. Read DESIGN.md and DETAILS.md first.

## Working principles

These apply throughout. Internalize before starting.

- **One phase at a time.** Do not work ahead. If a phase feels too big for one change, split it into commits — but don't roll subsequent phases into the work.
- **Tests live with the code, not after.** If a test is hard to write, the design is probably wrong — surface it.
- **Stop at every 🛑 gate.** Wait for explicit human go-ahead before continuing. Do not assume.
- **Commit messages reference the phase and what changed.** A reviewer should be able to grok the commit from the message alone.
- **When in doubt, ask.** Guessing on Bazel-shaped problems is the most expensive thing you can do.
- **No speculative fields or features.** Forward-compat is handled where DESIGN.md mandates it; everywhere else, YAGNI.
- **Stubs over crashes.** When you encounter API surface DESIGN.md doesn't cover, stub it conservatively, document with `// STUB:`, and flag it in the PR description.

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

## Phase 6 — First real ruleset

Pick a small fixture using the v1 ruleset. Pre-read the ruleset's source to inventory every `ctx.*`, `*_common.*`, and provider type used. Categorize each as implemented / stubbed-OK / needs-work. Work through the inventory one item per commit.

**Done when:** cquery comparison passes for the real-ruleset fixture (or divergences are documented and accepted).

🛑 **STOP for human review.** This is v1. Demo, write the release notes, declare victory.

## Future phases (do not start without explicit direction)

Aspect propagation, multi-config and transitions, real toolchain resolution, module extensions and repo rules, visibility, incremental analysis with file watchers, `loupe-exec`, `loupe-rpc`. Each gets its own plan when its time comes.

## Cross-cutting requirements

These apply at every phase, not as separate work:

- **Tracing.** Every DICE compute function gets a tracing span. Every error logged with context. `RUST_LOG=loupe=debug` should produce a useful trace.
- **Errors.** Every error includes what failed, where in user code (preserve Starlark `Diagnostic` source spans), and why. Never `unwrap()` on Starlark `Result`s.
- **Determinism.** Sort `HashMap` iterations or use `BTreeMap`. Sort `glob` results and directory listings. Anything user-visible has stable order.
- **Forward-compat.** Before removing or simplifying anything, ask: am I closing a future door? If yes, document the door explicitly.
- **Debugging infrastructure built early.** A `--trace=<label>` mode showing the analysis chain. A `dump-package` CLI command. You'll need these while building correctness, not after.
