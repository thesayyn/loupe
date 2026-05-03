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

use std::collections::BTreeMap;

use allocative::Allocative;

use crate::starlark_types::file::File;

/// One declared action.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
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
}

impl Action {
    /// Convenience: an action's "primary output" — the first listed.
    /// Used by aquery's mnemonic-matching logic and by humans.
    pub fn primary_output(&self) -> Option<&File> {
        self.outputs.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{CanonicalRepo, PackageId, PackagePath};

    fn pkg() -> PackageId {
        PackageId::new(CanonicalRepo::MAIN, PackagePath::root())
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
                File::generated_bin(pkg(), "first.txt", "loupe"),
                File::generated_bin(pkg(), "second.txt", "loupe"),
            ],
            env: BTreeMap::new(),
        };
        assert_eq!(
            a.primary_output().map(|f| f.path()),
            Some("bazel-out/loupe/bin/first.txt".to_string())
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
        };
        assert!(a.primary_output().is_none());
    }
}
