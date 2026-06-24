# Pull Ourselves Together — the bonanza revert

> Working spec + tracker for undoing the bonanza-era drift and snapping the
> engine back to the design it was always supposed to be. Read DESIGN.md,
> DETAILS.md, PLAN.md first; this document is the consolidated decision record
> and the order of work. Delete this file once all phases land and the
> principles are absorbed into the three standing docs.

## What went wrong

We followed bonanza's "vendor and run all of Bazel's builtins" strategy. It
conflicts head-on with Loupe's stated design (tiny core, real-or-fail,
user-space-first). The symptoms, all interlocked:

- **18.7k lines of vendored `builtins_bzl`** — the *pre*-starlarkification cc /
  objc / python / xcode rule tree (cc alone is 13.5k). `rules_oci` (our v1
  target) and the `rules_js` probe touch none of it.
- **~2.8k lines of `cc_common` / `cc_internal` / `java_common` / `py_builtins`
  reimplemented in `builtins_core/exports.bzl`** — copied verbatim from
  bonanza. The receipt: `exports.bzl:3450` still reads *"starlark-**go** does
  not support json.encode_indent()"* in a starlark-**rust** repo. Bazel's
  `cc_common` is native (Java); only bonanza reimplemented it in Starlark
  because it has no Java host. It has no analog in today's Bazel.
- **`WildcardStruct` + `bind_freeze_time_wildcards` (~80 names)** — the crutch
  that lets the stale tree freeze. Direct violation of "real or fail; never
  silent stubs" and of PLAN Phase 5 ("Drop `WildcardStruct`. Type deleted."),
  which never happened because the vendored tree can't freeze without it.

These three are one removal: the vendored tree forces the reimpl forces the
wildcards. Cut the tree and the whole stack collapses.

## The principle (where the native/Starlark line falls)

Native (Rust) **iff the engine itself reads the value** — i.e. it is tied to
DICE / the heap / the filesystem, **or** the engine introspects it (matches its
identity, downcasts it, reads its fields). Everything else is user-space
`.bzl`, or it does not exist (and referencing it fails).

There are two kinds of "native Bazel code", and conflating them is the original
sin:

- **Handles** — values the analysis driver itself reads to wire the graph (the
  DICE key, the thing it indexes for `target[P]`, the provider it reads for
  default outputs). For these, *"not native" does not become "in Starlark"; it
  becomes "reached back by string / repr from Rust"* — which is the exact
  `provider_by_symbol("DefaultInfo")` / repr-scraping leak we are reverting.
  Making them native is what keeps the engine honest.
- **Algorithms** — computations *over* values (`expand_location`, select
  resolution logic, make-var expansion, `run_shell` lowering, all of
  `cc_common`). These can live anywhere; Starlark is the right home unless they
  are inseparable from a native handle.

### The native-3

The only Bazel-shaped values that stay native, because the engine reads them on
every target:

1. **`Configuration`** — the typed spine. Everything configuration-related
   dangles off it. It is the `AnalysisKey` DICE key, so it must be a real
   `Eq + Hash` Rust type; an opaque frozen value would force repr-hashing.
2. **`RuleContext` (`ctx`)** — reads `Configuration`, attrs, toolchains, the
   action sink, the heap. Fully native, *including* the derived algorithms
   (`expand_location`, `expand_make_variables`/`var`, `coverage_instrumented`,
   `resolve_command`, `run_shell` lowering). It reads so much engine state that
   a native type is cheaper to reason about than per-field Starlark round-trips.
3. **`DefaultInfo`** — the analysis driver constructs it (source-file synthesis,
   alias forwarding, empty fallback) and reads it (default outputs, runfiles).
   Native, with engine-owned identity recognised by pointer.

Everything else — the cc/objc/java/python compat surface, the native rule
*constructors* (`filegroup`, `genrule`, …), the no-op declarators — is Starlark
or gone.

### Track Bazel's starlarkification; never prop a stale tree

Bazel 9/10 moved the cc/objc/python rule implementations out of `builtins_bzl`
into `rules_cc` / `rules_python`. The vendored `builtins_bzl` (Tier 3) tracks
**current Bazel master**, bumped wholesale, never edited locally, and **never
propped up by wildcards**. Removed rules resolve to a real failure handler
("add a `load()`"), not a stub that fabricates a value. Rulesets that need cc
resolve `rules_cc` through the registry like any other dep.

## The work

Each phase is its own PR. `rules_oci` stays 10/10 at every merge; the `rules_js`
probe must not regress.

- **PR 1 — Tier 2 strip + stop injecting the bonanza tree.** Split by what is
      verifiable without a local build (this sandbox can't compile — the buck2
      git dep is egress-blocked — so CI is the verification oracle):
  - [x] **1a — strip the cc/java/py reimpl from `builtins_core/exports.bzl`.**
        Self-contained Starlark; 3560 → 788 lines; `_builtins.internal` is now an
        empty struct (access raises). *Landed.*
  - [x] **1b — stop the eager `builtins_bzl/exports.bzl` second-source
        injection.** The cc/objc/python/xcode tree is no longer force-loaded or
        injected as globals. *Landed.*
  - [ ] **1c — physically delete the dead vendored cc/objc/xcode tree (DEFERRED;
        needs a working build).** `builtin_exec_platforms.bzl → py_internal.bzl →
        cc` is still referenced as the (v1-never-fired) `exec_transition` default
        in `command_line_option/BUILD.bazel`. Deleting cleanly means bumping
        Tier 3 to current Bazel master (slim layout) and adapting the exec
        transition + the thin `cc_common`/`java_common`/`py_internal` shims, which
        must be verified against `loupe analyze`. Vendored files are "never edited
        locally; bumped wholesale" — so this is a bump, not a hand-edit.
  - [x] **1d — `WildcardStruct` + freeze-time wildcards deleted (#152).** Landed:
        `_builtins.{toplevel,internal}` are empty strict structs (access raises);
        `bind_freeze_time_wildcards` → `bind_freeze_time_placeholders` binds only
        genuinely-unmodeled names to `None` (real globals no longer shadowed);
        the skip-guards and the type are gone; `config_common.toolchain_type` is a
        `fail()` not a wildcard. CI green confirms the embedded tree still freezes
        and the rules_oci probe doesn't trip the new `fail()`.
- [x] **PR 2 — `DefaultInfo` identity (already substantially implemented).**
      Investigated on the merged tree: the engine already owns the DefaultInfo
      handle. `bzl_eval::stamp_origin_with_names` stamps
      DefaultInfo/OutputGroupInfo/InstrumentedFilesInfo *native*
      (`defining_bzl: None`); engine-synthesised instances (source files, empty
      fallback) carry that canonical `type_ptr`; `target[P]` matches by
      `ProviderId` with no symbol fall-through; and the old
      `target[P] → WildcardStruct` fallback is gone (it raises now =
      real-or-fail). The PLAN's "item 2" to-do is stale. Residual: two
      engine-internal `provider_by_symbol("DefaultInfo"/"OutputGroupInfo")`
      convenience lookups that are functionally identity-equivalent given the
      canonical stamping — left as-is (tightening them is churn with blind
      compile risk and ~zero behaviour change). A *full native Rust constructor*
      (reimplementing the runfiles computed-fields in Rust) is the optional
      "sanctioned exception" and is **not** needed for the handle.
- [x] **PR 3 — select de-skylib (already implemented); typed build-settings skipped.**
      Investigated on the merged tree: `select()`/`config_setting` resolution
      already reads `flag_target.build_setting_value` (engine state) in
      `dice_keys.rs`, explicitly *not* skylib's `BuildSettingInfo` by symbol+repr
      (the "resolve select() flags from engine state" commit). So the PLAN's
      "de-skylib" goal landed. The remaining "make `build_settings` Label-keyed"
      idea is **skipped**: it's low-value and risky — `config_setting(values=…)`
      correctly matches by short flag name (`"compilation_mode"`), not Label, so
      typing the key would be wrong, not better. Optional leftover (not blocking,
      doesn't touch rules_js): collapse the `CtxConfiguration` shadow (carries
      only `is_exec`) into a real view over `Configuration` — a pure-cleanliness
      refactor, deferred.
- [ ] **PR 4 — (independent) move no-ops to user-space.** `stub.rs` declarators
      (`visibility`, `repository_rule`, `module_extension`, `tag_class`,
      `transition`, `exec_group`) and the `native_rules.rs` recorders
      (`alias`, `label_flag`, `label_setting`) become `.bzl`. (PLAN Phases 3 & 6.)

## What does NOT change

- The native `RuleContext` trajectory (the last ~10 commits) was correct under
  the introspection criterion — keep it. Bucket A is *not* reverted.
- `subrule` / `aspect` / `transitions` remain v1 no-op-but-shaped seams; this
  work must not foreclose them.
- The DICE key set, the `add_reference` heap discipline, depset/select/Label
  semantics — untouched.

## Gates and invariants (every phase)

- `cargo build --workspace` clean; `cargo test -p loupe-core` green.
- `rules_oci` cquery comparison stays at 10/10; `rules_js` probe not regressed.
- No banner-style comments. Preserve Starlark `Diagnostic` source spans.
  Deterministic iteration order. No `.unwrap()` on Starlark `Result`s.
- Ask before adding any new stub at the API surface; default to letting it raise.

## Doc reconciliation (folded in as part of this work)

- **DESIGN.md** — strengthen "engine owns what it introspects" with the
  handles-vs-algorithms distinction and the native-3; add the "track
  starlarkification / Tier-3 bump / never prop with wildcards" rule.
- **PLAN.md** — the "user-space-first refactor" Phase 2 (push ctx fields into
  Starlark) is *superseded*: `ctx` is native by the introspection criterion.
  Phases 4–5 (drop Lenient/Wildcard) are now landing as PR 1. Record the
  native-3 outcome of the "engine owns introspected identity" follow-on.
- **DETAILS.md** — native `ctx` stays documented as correct; drop any framing
  that treats the full vendored-builtins tree as the intended Tier-3 shape.
