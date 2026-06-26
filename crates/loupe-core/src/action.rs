//! Recorded actions — what `ctx.actions.*` accumulates during analysis.
//!
//! From DESIGN.md:
//! > Loupe doesn't execute actions; it records them. The action graph
//! > is the deliverable.
//!
//! Each `ctx.actions.write(...)` / `.run(...)` / `.run_shell(...)` /
//! `.symlink(...)` call appends an [`Action`] to the analysis-context
//! registry. After the rule impl returns, the recorded actions land
//! on the `AnalyzedTarget` alongside its providers, ready for cquery
//! / aquery-style consumers.
//!
//! The shape mirrors Bazel's `Action` surface but stays Rust-native —
//! no Starlark heap pointers cross the DICE boundary, mirroring the
//! same invariant `CapturedProvider` follows.
//!
//! Phase 5+ will surface `aquery`-style output. For now, the shape is
//! complete enough that a ruleset author can `dump-package` and see
//! the actions a rule recorded.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::values::NoSerialize;

use crate::starlark_types::file::File;

/// One declared action.
///
/// Plain Rust data + a `StarlarkValue` impl so `target.actions[i]`
/// reads it directly (the Starlark surface is in
/// `starlark_types::actions`). No `Arc` wrapper because heap
/// allocation copies the struct anyway and the contents stay in
/// the same heap as the surrounding `Target`.
#[derive(Debug, Clone, PartialEq, Eq, Allocative, NoSerialize, ProvidesStaticType)]
pub struct Action {
    pub kind: ActionKind,
    /// Bazel's "mnemonic" — short tag for log/aquery output.
    pub mnemonic: Option<String>,
    /// Human-readable progress string Bazel shows on `--show_progress`.
    pub progress_message: Option<String>,
    pub inputs: Vec<File>,
    pub outputs: Vec<File>,
    /// Environment variables passed to the action. v1 surfaces them
    /// but doesn't enforce a sanitization policy (Bazel's
    /// `--action_env` story lands later).
    pub env: BTreeMap<String, String>,
    /// Per-action overrides / additions on top of the configuration's
    /// platform `exec_properties`. Comes from
    /// `ctx.actions.run(execution_requirements = {...})` (and the
    /// matching kwarg on `run_shell`); Bazel folds this dict into the
    /// REAPI `Action.platform.properties` for the cache key, with the
    /// per-action entries winning on key collisions.
    ///
    /// Recorded per Action because the scope is one Action, not the
    /// whole configuration. The configuration's
    /// [`crate::Configuration::exec_properties`] carries the platform-
    /// side defaults; consumers (aquery, AC-key emitters) overlay
    /// these per-action entries on top.
    ///
    /// Defaults to empty. `BTreeMap` so iteration order is sorted —
    /// REAPI's `Platform.properties` is a sorted list, and downstream
    /// merging stays deterministic without re-sorting.
    pub execution_requirements: BTreeMap<String, String>,
    /// Mirrors Bazel's `ctx.actions.run(use_default_shell_env=…)` /
    /// `ctx.actions.run_shell(use_default_shell_env=…)` flag. When
    /// true, executors layer [`default_shell_env`] under the
    /// action-supplied `env` map when building the REAPI
    /// `Command.environment_variables`. Recorded but not consulted
    /// by analysis (Loupe doesn't execute actions); AC-key / future
    /// executor consumers read it. Only meaningful for `Run` and
    /// `RunShell`; other variants always record `false`.
    pub uses_default_shell_env: bool,
}

/// Discriminator for the four `ctx.actions.*` flavors v1 records.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub enum ActionKind {
    /// `ctx.actions.write(output, content, is_executable=False)`.
    /// Inputs is implicitly empty; outputs is `[output]`.
    Write {
        content: String,
        is_executable: bool,
    },
    /// `ctx.actions.run(executable=..., arguments=..., inputs=...,
    /// outputs=...)`.
    Run {
        /// The executable file. Conceptually one of the inputs, but
        /// kept on the action so consumers can find it without
        /// inspecting the inputs list.
        executable: File,
        arguments: Vec<String>,
    },
    /// `ctx.actions.run_shell(command=..., arguments=..., inputs=...,
    /// outputs=...)`. Bazel internally lowers this to a `Run` of
    /// `/bin/bash -c <command> -- <arguments>`. We keep it as a
    /// distinct variant so consumers can present it that way; the
    /// lowering is an aquery-side concern.
    RunShell {
        command: String,
        arguments: Vec<String>,
    },
    /// `ctx.actions.symlink(target_file=..., output=...)`. The output
    /// becomes a symlink to `target`. Inputs is `[target]`; outputs is
    /// `[output]`.
    Symlink { target: File },
    /// `ctx.actions.expand_template(template, output, substitutions,
    /// is_executable=False)`. Records the template `File` and the
    /// `key → value` substitution map; v1 doesn't actually run the
    /// substitution at analysis time (STUB) — consumers reading the
    /// recorded action see the original template as input.
    ExpandTemplate {
        template: File,
        substitutions: BTreeMap<String, String>,
        is_executable: bool,
    },
}

impl Action {
    /// Convenience: an action's "primary output" — the first listed.
    /// Used by aquery's mnemonic-matching logic and by humans.
    pub fn primary_output(&self) -> Option<&File> {
        self.outputs.first()
    }
}

/// The "default shell environment" Bazel injects into an action's
/// REAPI `Command.environment_variables` map when the rule sets
/// `use_default_shell_env=True` on `ctx.actions.run` /
/// `ctx.actions.run_shell`.
///
/// v1 ships the canonical POSIX minimum — `PATH` set to the standard
/// system search list. This is what an executor with no
/// `--action_env` overrides would see; almost every rule relying on
/// `use_default_shell_env=True` is reaching for `PATH` so that a
/// `/usr/bin/env <tool>` invocation in the action's argv (or the
/// shell command body of `run_shell`) resolves at execution time.
///
/// STUB: Bazel's actual default is `--action_env` + the host OS
/// platform-specific seed (e.g. Windows adds `SystemRoot`, `TMP`,
/// `USERPROFILE`; macOS adds the Apple developer tooling env). We
/// don't read `--action_env` here because:
///
/// 1. Loupe doesn't execute actions, so the "real" environment
///    composition is the executor's job — Loupe just records the
///    `uses_default_shell_env` bit and lets the consumer compose.
/// 2. `--action_env` would need to flow through `Configuration`
///    and ultimately every `AnalysisKey`, which is a separate piece
///    of plumbing that hasn't surfaced yet. AC-key REAPI Command
///    construction (PR #85's domain) reads the bit and consults
///    this helper as the v1 baseline; richer compositions land
///    when an executor or `--action_env` flag does.
pub fn default_shell_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".to_string(),
        "/usr/local/bin:/usr/bin:/bin".to_string(),
    );
    env
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<action {}>", self.mnemonic.as_deref().unwrap_or("?"))
    }
}

/// Result of walking a [`Symlink`](ActionKind::Symlink) target chain to
/// the action whose output produces the bytes the symlink ultimately
/// points at.
///
/// `ctx.actions.symlink(output=O, target_file=T)` records
/// `ActionKind::Symlink { target: T }`. When `T` is itself the output
/// of another recorded action, downstream consumers — the AC-key
/// resolver (see PR #85's `KnownContentResolver`), runfiles
/// materialisation, and the future executor — need to chase the chain
/// to whichever action actually produces content. That terminal kind
/// is one of: a [`Write`](ActionKind::Write), an
/// [`ExpandTemplate`](ActionKind::ExpandTemplate), a source [`File`]
/// (when the chain bottoms out at something not produced by a
/// recorded action — usually a source input from `ctx.files.*`), or a
/// [`Run`](ActionKind::Run) / [`RunShell`](ActionKind::RunShell)
/// output (whose bytes can't be known without execution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalContent<'a> {
    /// The chain bottoms out at a `File` not produced by any indexed
    /// action — typically a source file referenced via `ctx.files.*`
    /// or an artifact predeclared outside the recorded set.
    Source(&'a File),
    /// The chain terminates at a [`Write`](ActionKind::Write) action.
    Write {
        content: &'a str,
        is_executable: bool,
    },
    /// The chain terminates at an
    /// [`ExpandTemplate`](ActionKind::ExpandTemplate) action.
    ExpandTemplate {
        template: &'a File,
        substitutions: &'a BTreeMap<String, String>,
        is_executable: bool,
    },
    /// The chain terminates at a [`Run`](ActionKind::Run) or
    /// [`RunShell`](ActionKind::RunShell) — output bytes are opaque
    /// without execution. The carried `File` is the producing action's
    /// primary output.
    Opaque(&'a File),
}

/// Maximum symlink-chain depth before [`resolve_symlink_chain`] gives
/// up and returns [`TerminalContent::Opaque`] for the cycle / overly
/// long path. Matches the visited-set guard below — whichever trips
/// first ends the walk. 32 is well past any sane recorded chain;
/// `ctx.actions.symlink` chains in real rulesets are typically 1-2
/// hops.
const SYMLINK_CHAIN_MAX_DEPTH: usize = 32;

/// Walk an [`ActionKind::Symlink`] target chain to its terminal content
/// source.
///
/// Starting from `output`, the walker looks `output.path()` up in
/// `actions_by_output`. If found and the action is a `Symlink`, the
/// walker follows the `target` edge and repeats; otherwise it
/// classifies the action into the appropriate [`TerminalContent`]
/// variant. When the path isn't in the index — e.g. a source file
/// referenced via `ctx.files.*`, or an artifact predeclared outside
/// the recorded set — the walker returns [`TerminalContent::Source`]
/// pointing at the last `File` it visited.
///
/// ## Cycle behaviour
///
/// Cycles and overlong chains both resolve to
/// [`TerminalContent::Opaque`] pointing at the *first* `File` in the
/// chain (the caller's starting `output`). Choosing `Opaque` over an
/// error keeps callers (AC-key resolver, runfiles staging) on the
/// happy path: a cycle means "we can't know the bytes without
/// executing something that may itself loop", which is operationally
/// the same as a `Run` output. Two guards trigger the fallback:
///
/// 1. A visited-set: revisiting a `File::path()` returns `Opaque`.
///    Catches direct cycles `A → B → A` and indirect ones.
/// 2. A hard depth limit at [`SYMLINK_CHAIN_MAX_DEPTH`]. Catches
///    "chain too deep to be real" without keeping the visited set
///    around forever.
///
/// The visited-set covers correctness; the depth limit is a belt-and-
/// braces guard for `actions_by_output` indices that somehow encode a
/// non-cyclic but absurdly deep chain.
pub fn resolve_symlink_chain<'a>(
    output: &'a File,
    actions_by_output: &'a HashMap<String, &'a Action>,
) -> TerminalContent<'a> {
    let cycle_anchor = output;
    let mut current = output;
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..SYMLINK_CHAIN_MAX_DEPTH {
        let key = current.path();
        if !visited.insert(key.clone()) {
            return TerminalContent::Opaque(cycle_anchor);
        }
        let Some(action) = actions_by_output.get(&key) else {
            // Not produced by any recorded action — treat as a source
            // input. Matches the "chain bottoms out at ctx.files.*"
            // case.
            return TerminalContent::Source(current);
        };
        match &action.kind {
            ActionKind::Symlink { target } => {
                current = target;
            }
            ActionKind::Write {
                content,
                is_executable,
            } => {
                return TerminalContent::Write {
                    content,
                    is_executable: *is_executable,
                };
            }
            ActionKind::ExpandTemplate {
                template,
                substitutions,
                is_executable,
            } => {
                return TerminalContent::ExpandTemplate {
                    template,
                    substitutions,
                    is_executable: *is_executable,
                };
            }
            ActionKind::Run { .. } | ActionKind::RunShell { .. } => {
                // Run / RunShell outputs require execution to know
                // their bytes — opaque to analysis.
                let primary = action.primary_output().unwrap_or(current);
                return TerminalContent::Opaque(primary);
            }
        }
    }
    // Exceeded the depth limit without terminating — treat as a cycle.
    TerminalContent::Opaque(cycle_anchor)
}

/// Build a `path() → &Action` index keyed by each action's *primary*
/// output (`Action::primary_output()`, i.e. the first listed output).
///
/// Multi-output actions only index under the primary; secondary
/// outputs of one action that another action symlinks into would need
/// a separate index. v1 ruleset usage (`rules_oci`, `rules_js` probe)
/// only declares one output per action, so the primary-only index
/// matches every observed lookup. Extending to all outputs is a
/// straight loop change when a multi-output ruleset surfaces.
pub fn index_actions_by_output(actions: &[Action]) -> HashMap<String, &Action> {
    let mut idx = HashMap::with_capacity(actions.len());
    for a in actions {
        if let Some(out) = a.primary_output() {
            idx.insert(out.path(), a);
        }
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_labels::{CanonicalRepo, PackageId, PackagePath};

    fn pkg() -> PackageId {
        PackageId::new(CanonicalRepo::main(), PackagePath::root())
    }

    #[test]
    fn primary_output_is_first_output() {
        // Aquery-style mnemonic search keys off the primary output.
        // Confirm the order we record matches what consumers will see.
        let a = Action {
            kind: ActionKind::Write {
                content: "x".into(),
                is_executable: false,
            },
            mnemonic: Some("Write".into()),
            progress_message: None,
            inputs: vec![],
            outputs: vec![
                File::generated_bin(pkg(), "first.txt", "fake-cfg"),
                File::generated_bin(pkg(), "second.txt", "fake-cfg"),
            ],
            env: BTreeMap::new(),
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        };
        assert_eq!(
            a.primary_output().map(|f| f.path()),
            Some("bazel-out/fake-cfg/bin/first.txt".to_string())
        );
    }

    #[test]
    fn empty_outputs_has_no_primary() {
        // Doesn't happen in practice — Bazel rejects actions with no
        // outputs — but guard against the panic shape.
        let a = Action {
            kind: ActionKind::RunShell {
                command: "echo".into(),
                arguments: vec![],
            },
            mnemonic: None,
            progress_message: None,
            inputs: vec![],
            outputs: vec![],
            env: BTreeMap::new(),
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        };
        assert!(a.primary_output().is_none());
    }

    #[test]
    fn default_shell_env_carries_path() {
        // Canonical POSIX minimum — the bare-PATH baseline that
        // executors layer underneath user-supplied env when
        // `use_default_shell_env=True`. STUB-documented: Bazel's
        // real default also varies by platform / `--action_env`,
        // but the bare-PATH form is what consumers fall back to
        // until `--action_env` plumbing lands.
        let env = default_shell_env();
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some("/usr/local/bin:/usr/bin:/bin")
        );
        // No other keys at this baseline; explicit so the test fails
        // if someone silently grows the map.
        assert_eq!(env.len(), 1);
    }

    fn gen_file(name: &str) -> File {
        File::generated_bin(pkg(), name, "fake-cfg")
    }

    fn write_action(name: &str, content: &str) -> Action {
        Action {
            kind: ActionKind::Write {
                content: content.into(),
                is_executable: false,
            },
            mnemonic: Some("FileWrite".into()),
            progress_message: None,
            inputs: vec![],
            outputs: vec![gen_file(name)],
            env: BTreeMap::new(),
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        }
    }

    fn symlink_action(output: &str, target: File) -> Action {
        Action {
            kind: ActionKind::Symlink { target },
            mnemonic: Some("Symlink".into()),
            progress_message: None,
            inputs: vec![],
            outputs: vec![gen_file(output)],
            env: BTreeMap::new(),
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        }
    }

    #[test]
    fn resolve_single_symlink_to_write() {
        // O → W. Walker follows the one hop and lands on the Write's
        // content.
        let actions = vec![
            write_action("payload.txt", "hi"),
            symlink_action("link.txt", gen_file("payload.txt")),
        ];
        let idx = index_actions_by_output(&actions);
        let link_out = &actions[1].outputs[0];
        match resolve_symlink_chain(link_out, &idx) {
            TerminalContent::Write {
                content,
                is_executable,
            } => {
                assert_eq!(content, "hi");
                assert!(!is_executable);
            }
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn resolve_chain_of_two_symlinks_to_source() {
        // L2 → L1 → src. `src` is not produced by any indexed action;
        // walker reports `Source(src)`.
        let src = File::source(pkg(), "src.txt");
        let actions = vec![
            symlink_action("l1.txt", src.clone()),
            symlink_action("l2.txt", gen_file("l1.txt")),
        ];
        let idx = index_actions_by_output(&actions);
        let l2 = &actions[1].outputs[0];
        match resolve_symlink_chain(l2, &idx) {
            TerminalContent::Source(f) => {
                assert_eq!(f.path(), src.path());
            }
            other => panic!("expected Source, got {other:?}"),
        }
    }

    #[test]
    fn resolve_symlink_to_run_output_is_opaque() {
        // O → run output. Run output is opaque without execution.
        let run = Action {
            kind: ActionKind::Run {
                executable: File::source(pkg(), "tool"),
                arguments: vec![],
            },
            mnemonic: Some("Run".into()),
            progress_message: None,
            inputs: vec![],
            outputs: vec![gen_file("produced.bin")],
            env: BTreeMap::new(),
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        };
        let sl = symlink_action("link.bin", gen_file("produced.bin"));
        let actions = vec![run, sl];
        let idx = index_actions_by_output(&actions);
        let link_out = &actions[1].outputs[0];
        match resolve_symlink_chain(link_out, &idx) {
            TerminalContent::Opaque(f) => {
                assert_eq!(f.path(), "bazel-out/fake-cfg/bin/produced.bin");
            }
            other => panic!("expected Opaque, got {other:?}"),
        }
    }

    #[test]
    fn resolve_cycle_returns_opaque() {
        // A → B → A. Visited-set fires; result is Opaque pinned at the
        // walk's starting File (documented behaviour).
        let actions = vec![
            symlink_action("a.txt", gen_file("b.txt")),
            symlink_action("b.txt", gen_file("a.txt")),
        ];
        let idx = index_actions_by_output(&actions);
        let start = &actions[0].outputs[0];
        match resolve_symlink_chain(start, &idx) {
            TerminalContent::Opaque(f) => {
                assert_eq!(f.path(), start.path());
            }
            other => panic!("expected Opaque, got {other:?}"),
        }
    }

    #[test]
    fn resolve_symlink_to_unrecorded_target_is_source() {
        // O → src (source File not produced by any recorded action).
        // Walker reports the target as `Source`, matching the
        // "ctx.files.*" case.
        let src = File::source(pkg(), "vendor/data.bin");
        let actions = vec![symlink_action("link.bin", src.clone())];
        let idx = index_actions_by_output(&actions);
        let link_out = &actions[0].outputs[0];
        match resolve_symlink_chain(link_out, &idx) {
            TerminalContent::Source(f) => {
                assert_eq!(f.path(), src.path());
            }
            other => panic!("expected Source, got {other:?}"),
        }
    }

    #[test]
    fn resolve_chain_three_symlinks_to_write() {
        // L3 → L2 → L1 → W. Confirms multi-hop traversal lands on the
        // Write content rather than stopping early.
        let actions = vec![
            write_action("payload.txt", "deep"),
            symlink_action("l1.txt", gen_file("payload.txt")),
            symlink_action("l2.txt", gen_file("l1.txt")),
            symlink_action("l3.txt", gen_file("l2.txt")),
        ];
        let idx = index_actions_by_output(&actions);
        let l3 = &actions[3].outputs[0];
        match resolve_symlink_chain(l3, &idx) {
            TerminalContent::Write { content, .. } => assert_eq!(content, "deep"),
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn resolve_symlink_to_expand_template() {
        // O → ExpandTemplate. Walker surfaces template + subs.
        let template = File::source(pkg(), "tpl.in");
        let mut subs = BTreeMap::new();
        subs.insert("X".to_string(), "1".to_string());
        let et = Action {
            kind: ActionKind::ExpandTemplate {
                template: template.clone(),
                substitutions: subs.clone(),
                is_executable: false,
            },
            mnemonic: Some("ExpandTemplate".into()),
            progress_message: None,
            inputs: vec![template.clone()],
            outputs: vec![gen_file("expanded.out")],
            env: BTreeMap::new(),
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        };
        let sl = symlink_action("link.out", gen_file("expanded.out"));
        let actions = vec![et, sl];
        let idx = index_actions_by_output(&actions);
        let link_out = &actions[1].outputs[0];
        match resolve_symlink_chain(link_out, &idx) {
            TerminalContent::ExpandTemplate {
                template: t,
                substitutions,
                is_executable,
            } => {
                assert_eq!(t.path(), template.path());
                assert_eq!(substitutions.get("X").map(String::as_str), Some("1"));
                assert!(!is_executable);
            }
            other => panic!("expected ExpandTemplate, got {other:?}"),
        }
    }

    #[test]
    fn index_actions_by_output_keys_on_primary() {
        // Multi-output action indexes only under its primary, matching
        // the documented choice.
        let multi = Action {
            kind: ActionKind::Write {
                content: "x".into(),
                is_executable: false,
            },
            mnemonic: Some("Multi".into()),
            progress_message: None,
            inputs: vec![],
            outputs: vec![gen_file("primary.txt"), gen_file("secondary.txt")],
            env: BTreeMap::new(),
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        };
        let actions = vec![multi];
        let idx = index_actions_by_output(&actions);
        assert!(idx.contains_key("bazel-out/fake-cfg/bin/primary.txt"));
        assert!(!idx.contains_key("bazel-out/fake-cfg/bin/secondary.txt"));
    }
}
