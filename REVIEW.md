# Review of DESIGN.md / PLAN.md / DETAILS.md

A close read of the three project docs as of this branch. Findings are
ordered by severity — inconsistencies first, then gaps, then smaller
clarifications.

## Inconsistencies between docs

### Crate count disagrees

`DESIGN.md` (Architecture) names two crates:

> Crates: `loupe-core` (engine, no consumer awareness), `loupe-cli`
> (debugging).

`PLAN.md` (Phase 0) names three:

> Set up the workspace with three crates: `loupe-core`, `loupe-cli`,
> `loupe-test-harness`.

Pick one. If the harness is its own crate (which is the cleaner setup,
since it shells out to `bazel` and shouldn't pull deps into core),
`DESIGN.md`'s Architecture section should list it alongside `loupe-cli`.

### Future-crate list differs

`DESIGN.md`: `loupe-exec`, `loupe-rpc`, `loupe-watch`.
`PLAN.md` (Future phases): `loupe-exec`, `loupe-rpc`. No `loupe-watch`.

Minor, but `loupe-watch` deserves a one-line mention in PLAN's future
phases so it isn't forgotten.

### Public API surface vs. what Phase 5 actually builds

`DESIGN.md`'s public API:

```rust
pub fn analyze(&self, label: &Label) -> Result<Analysis>;
pub fn analyze_transitive(&self, roots: &[Label]) -> Result<Vec<Analysis>>;
```

`PLAN.md` Phase 5 says "cquery comparison passes for fixture #1" but
never describes what `Engine.analyze` returns to the harness, nor when
`analyze_transitive` shows up. Phase 6 (real ruleset) almost certainly
needs `analyze_transitive` for runfiles/depgraph traversal — call that
out in the phase, not just in the API sketch.

### `Analysis.providers` shape is under-specified

`DESIGN.md`:

> `pub providers: FrozenValue,  // struct of provider type -> instance`

A Starlark `struct` is keyed by `&str`. Provider type identity per
DETAILS.md is a `FrozenValue` pointer (or, equivalently, the
`(defining_bzl, exported_symbol)` pair you have to stamp on it for
DICE-crossing equality). Those two key models are incompatible. The
real shape is more like `Vec<(ProviderId, FrozenValue)>` indexed by
`__getitem__` (Bazel's `target[Foo]`). Either:

- Update the comment to "list of (provider type, instance) pairs,
  indexed by provider type via `__getitem__`", or
- Define a `Providers` value with the right indexing semantics and
  point at it.

Right now an implementer reading just DESIGN.md would build the wrong
container.

## Coverage gaps

### Provider identity across the DICE boundary is hand-waved

`DESIGN.md` invariants and `DETAILS.md` ("Bazel APIs with weird
histories") both say "identity by construction site, FrozenValue
pointer equality." That works inside one heap. It does **not** work
across DICE compute boundaries — different heaps, different pointers.

The actual mechanism (stamp a `ProviderId { defining_bzl, symbol }`
onto the `provider()` value at `.bzl` freeze time, compare by that
pair afterwards) is the most subtle thing in the engine. It deserves
a paragraph in DETAILS.md ("Provider identity across heaps") so the
next implementer doesn't have to rediscover it.

Concretely: native providers have `defining_bzl: None` (so
`DefaultInfo` displays bare in cquery), user providers have
`defining_bzl: Some(Label)`. That's the kind of detail that takes
half a day to derive and ten seconds to read.

### `native.*` is missing from PLAN

`DETAILS.md` calls out `native.glob`, `native.package_name`,
`native.repository_name`, and stubbed `native.existing_rule(s)`. None
of these appear in any phase of PLAN.md. They almost certainly belong
in Phase 3 (loading-phase builtins) — adding one bullet would close
the loop.

### How does Phase 2 evaluate a BUILD file with no rules registered?

PLAN.md Phase 2:

> Implement enough loading to evaluate a `BUILD` file with a minimal
> Starlark environment (no `rule()` yet, no `load()` yet) … a `BUILD`
> file evaluator that captures rule-call shapes.

The "captures rule-call shapes" phrasing implies a fallback hook
(unknown function calls become recorded `UnconfiguredTarget`s). That's
a reasonable shortcut but it's not how Bazel works, and an implementer
might instead try to pre-register a builtin per fixture rule. Spell
this out: "a catch-all that records any unrecognized top-level call as
an unconfigured target, to defer `rule()`/`load()` to Phase 3."

### `Target` (analysis value) vs. `Analysis` (Rust return) aren't connected

`DESIGN.md` only mentions `Analysis`. `PLAN.md` Phase 5 introduces
`Target`. They're related but distinct (Target is what `ctx.attr.dep`
returns inside Starlark; Analysis is what crosses the Rust API
boundary). Add one sentence to DESIGN.md's API section saying so —
otherwise Phase 5 looks like it invents a parallel type.

### Heap lifetime across consumer/dep analysis isn't covered

DETAILS.md says "Don't share `Heap` across compute calls" and
"freezes the result, returns the `FrozenValue` (whose `FrozenHeap` is
`Sync`)." That's necessary but not sufficient: when consumer analysis
reads `target[Foo].field`, the field's `FrozenValue` lives in the
**dep's** frozen heap, which the consumer's heap must `add_reference`
to. This is the second-most-subtle thing in the engine. Worth a short
"Cross-target frozen value lifetimes" subsection in DETAILS.md.

### Where do CLI debugging tools land?

`PLAN.md` cross-cutting says:

> Debugging infrastructure built early. A `--trace=<label>` mode
> showing the analysis chain. A `dump-package` CLI command. You'll
> need these while building correctness, not after.

"Early" isn't a phase. If they're needed for Phase 5 debugging, say
"by end of Phase 4." Otherwise this slips.

## Tensions worth resolving in writing

### Forward-compat vs. YAGNI

DESIGN.md: "stub, don't error; plumb the dimension even when
degenerate." PLAN.md: "No speculative fields or features."

PLAN.md cross-cutting tries to reconcile: "Forward-compat is handled
where DESIGN.md mandates it; everywhere else, YAGNI." But DESIGN.md
only explicitly mandates two: `config` on `AnalysisKey` and
"`Action` records carry every field a future executor would need."

Things that have come up implicitly (provider identity stamp,
defining_bzl on ProviderId, configuration segment on File paths) are
plumbed dimensions but DESIGN.md doesn't mark them. Either:

- Make DESIGN.md's "plumbed dimensions" list exhaustive, or
- Soften "no speculative fields" to "no speculative fields outside
  Bazel-modeled dimensions" so an implementer doesn't strip
  `defining_bzl: Option<Label>` down to the v1-needed `String`.

### "Stop at every 🛑 gate" vs. autonomy

PLAN.md says "Wait for explicit human go-ahead before continuing. Do
not assume." That's clear for the four 🛑 gates as written. But
between gates, can an implementer split work across multiple PRs
without checking in? Phase 5 alone is two distinct chunks (cquery
parity, then "build out the rest of `ctx`"). One sentence: "between
gates, prefer many small PRs over one large one; no need to ask
between PRs within the same phase."

## Smaller cleanups

- DESIGN.md "DICE keys": `BzlLoadKey(label)` is fine if `Label` here
  means a label-shaped `.bzl` reference. Worth saying so — `.bzl`
  files aren't targets and an implementer might over-canonicalize.
- DETAILS.md "Pin a Bazel version in the test harness too (via
  `.bazelversion`)" — `.bazelversion` is workspace-level, not
  harness-level. Either say "in each fixture's `.bazelversion`" or
  "at the harness fixture root."
- PLAN.md Phase 4 mentions `exports_files` and "source target
  resolution" without saying what shape they take. Worth one line:
  "`exports_files` records source-file targets; analyzing one yields
  a synthetic `DefaultInfo` over the listed files."
- DETAILS.md "Common rules to know about" lists `rules_python`,
  `rules_go`, `rules_cc`, `genrule` but not the phase-6 candidate.
  PLAN Phase 6 says "the v1 ruleset target" — the actual choice
  should land in DETAILS.md once made (or be marked TBD).
- DESIGN.md "When the agent is confused" lists four sources but no
  Buck2; DETAILS.md devotes a section to reading Buck2. Add Buck2
  as a fifth (lower-priority) source in DESIGN.md.

## Things the docs do well

Worth keeping intact through any rewrite:

- The "stubs over crashes" rule, repeated in every doc.
- The `cquery` harness as ground truth — naming a single
  observable-equality target is the strongest forcing function in the
  plan.
- The "things the agent will probably try and should not" list in
  DESIGN.md. Concrete and well-aimed.
- DETAILS.md's "Bazel APIs with weird histories" — the
  why-it-exists framing is much more memorable than reference docs.
- Stop gates with explicit "demo to human" callouts on the 🛑 lines.

## Suggested patch ordering

If applied in this order, each change is small and reviewable on its
own:

1. Reconcile crate count (DESIGN.md updates to match PLAN.md, or
   vice versa).
2. Add `native.*` bullet to PLAN.md Phase 3.
3. Clarify `Analysis.providers` shape in DESIGN.md.
4. Add "Provider identity across heaps" + "Cross-target frozen
   value lifetimes" subsections to DETAILS.md.
5. Tighten the "no speculative fields" tension with one sentence.
6. Smaller cleanups.
