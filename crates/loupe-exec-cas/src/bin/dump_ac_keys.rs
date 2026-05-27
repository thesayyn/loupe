//! Dump Bazel-identical AC keys for every recorded action in a
//! workspace.
//!
//! Companion to `grpc_log_inspect`: feed both this and a Bazel
//! `--remote_grpc_log` through their respective decoders and you can
//! diff "what Bazel computed" against "what Loupe computes" to prove
//! byte-for-byte identity.
//!
//! Usage:
//!   dump_ac_keys <workspace> <target> [--external-repo-root <PATH>]...
//!
//! No DICE-keys-driven enumerator yet (the [`HydratedTargetKey`]
//! follow-up). For now we drive the engine directly, walk the single
//! `AnalyzedTarget`'s recorded action list, and call
//! `compute_action_cache_key` per action.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use loupe_core::ac_key::{AcKeyError, Digest, compute_action_cache_key};
use loupe_core::action::{
    ActionKind, TerminalContent, index_actions_by_output, resolve_symlink_chain,
};
use loupe_core::starlark_types::file::File;
use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

#[cfg(unix)]
fn read_unix_exec_bit(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn read_unix_exec_bit(_path: &std::path::Path) -> bool {
    false
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: dump_ac_keys <workspace> <target> \
             [--external-repo-root <PATH>]... \
             [--target-platform <LABEL>]"
        );
        return ExitCode::from(2);
    }
    let workspace = PathBuf::from(&args[1]);
    let target_str = &args[2];
    let mut external_repo_roots: Vec<PathBuf> = Vec::new();
    let mut target_platform: Option<String> = None;
    let mut i = 3;
    while i + 1 < args.len() {
        if args[i] == "--external-repo-root" {
            external_repo_roots.push(PathBuf::from(&args[i + 1]));
            i += 2;
        } else if args[i] == "--target-platform" {
            target_platform = Some(args[i + 1].clone());
            i += 2;
        } else {
            eprintln!("unknown arg: {}", args[i]);
            return ExitCode::from(2);
        }
    }

    let external_repo_roots_for_resolve = external_repo_roots.clone();
    let mut config = Configuration::default();
    if let Some(s) = target_platform.as_deref() {
        let host = match Label::resolve(
            s,
            &loupe_labels::PackageId::new(CanonicalRepo::MAIN, PackagePath::root()),
            &Default::default(),
        ) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("--target-platform {s}: {e}");
                return ExitCode::from(2);
            }
        };
        config = config.with_target_platform(host);
    }
    let builder = Engine::builder(workspace.clone(), config);
    let engine = match builder.build() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("engine: {e}");
            return ExitCode::from(1);
        }
    };
    // Hand-parse a simple `//pkg:name` main-repo label. Good enough
    // for fixture-driven comparison; richer label parsing lives in
    // `Label::resolve` but needs a repo-mapping context we don't have
    // here.
    let label = if let Some(rest) = target_str.strip_prefix("//") {
        let (pkg, name) = match rest.rsplit_once(':') {
            Some((p, n)) => (p, n),
            None => (rest, rest.rsplit_once('/').map_or(rest, |(_, n)| n)),
        };
        let pkg_path = match PackagePath::new(pkg.to_string()) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bad package path {pkg:?}: {e}");
                return ExitCode::from(2);
            }
        };
        Label::new(CanonicalRepo::MAIN, pkg_path, name)
    } else {
        eprintln!("only main-repo labels (//pkg:name) supported");
        return ExitCode::from(2);
    };
    let analyzed = match engine.analyze_target(&label) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("analyze: {e}");
            return ExitCode::from(1);
        }
    };

    // Resolve every source-file input by reading from disk. We don't
    // route through the DICE-backed SourceFileDigestKey here — this
    // tool runs outside any DICE session — but the digests are
    // identical: both are `sha256(file_bytes)`. Source paths starting
    // with `external/<canonical>/...` get re-rooted under one of the
    // --external-repo-root dirs.
    let mut source_bytes = BTreeMap::new();
    let mut resolve_source = |f: &File| {
        if !f.is_source() {
            return;
        }
        let path = f.path();
        if source_bytes.contains_key(&path) {
            return;
        }
        let mut found: Option<(Vec<u8>, bool)> = None;
        if let Some(rest) = path.strip_prefix("external/") {
            for root in &external_repo_roots_for_resolve {
                let abs = root.join(rest);
                if let Ok(b) = std::fs::read(&abs) {
                    found = Some((b, read_unix_exec_bit(&abs)));
                    break;
                }
            }
        } else {
            let abs = workspace.join(&path);
            if let Ok(b) = std::fs::read(&abs) {
                found = Some((b, read_unix_exec_bit(&abs)));
            }
        }
        match found {
            Some(v) => {
                source_bytes.insert(path, v);
            }
            None => {
                eprintln!("warn: source not found on disk: {path}");
            }
        }
    };
    for action in &analyzed.actions {
        for f in &action.inputs {
            resolve_source(f);
        }
        // Also pre-resolve `executable` for Run actions: AC-key
        // projection auto-includes it in the input set (matching
        // Bazel's `SpawnAction` behaviour), so the resolver needs
        // its bytes too. Skips absolute-path "synthetic" executables
        // (`/bin/bash` etc. from the run_shell rewrite).
        if let ActionKind::Run { executable, .. } = &action.kind
            && !executable.path().starts_with('/')
        {
            resolve_source(executable);
        }
    }

    // Pre-walk the action graph to resolve Write/ExpandTemplate/
    // Symlink outputs into the known-generated map. Run/RunShell
    // outputs aren't resolved here (those are loupe-exec-cas's job
    // against a cache) — they'll surface as AcKeyError::UnknownGenerated
    // and print as "no-digest" rows.
    let mut generated = BTreeMap::new();
    let idx = index_actions_by_output(&analyzed.actions);
    for action in &analyzed.actions {
        match &action.kind {
            ActionKind::Write {
                content,
                is_executable,
            } => {
                if let Some(out) = action.outputs.first() {
                    if let Ok(dump_base) = std::env::var("LOUPE_DUMP_AC_PROTOS") {
                        let safe = out.path().replace('/', "_");
                        let _ =
                            std::fs::write(format!("{dump_base}.write.{safe}"), content.as_bytes());
                    }
                    generated.insert(
                        out.path(),
                        (Digest::of_bytes(content.as_bytes()), *is_executable),
                    );
                }
            }
            ActionKind::ExpandTemplate { .. } => {
                // Template body isn't in the recorded Action shape;
                // skip (would need engine surface to materialise).
            }
            ActionKind::Symlink { .. } => {
                if let Some(out) = action.outputs.first()
                    && let TerminalContent::Write {
                        content,
                        is_executable,
                    } = resolve_symlink_chain(out, &idx)
                {
                    generated.insert(
                        out.path(),
                        (Digest::of_bytes(content.as_bytes()), is_executable),
                    );
                }
            }
            _ => {}
        }
    }

    let cfg = analyzed.configuration.clone();

    println!("target: {}", analyzed.label);
    println!("actions: {}", analyzed.actions.len());
    for (i, action) in analyzed.actions.iter().enumerate() {
        let mnemonic = action.mnemonic.as_deref().unwrap_or("?");
        let kind = match &action.kind {
            ActionKind::Run { .. } => "Run",
            ActionKind::RunShell { .. } => "RunShell",
            ActionKind::Write { .. } => "Write",
            ActionKind::Symlink { .. } => "Symlink",
            ActionKind::ExpandTemplate { .. } => "ExpandTemplate",
        };
        let outputs: Vec<String> = action.outputs.iter().map(|f| f.path()).collect();

        // Build `inputs_by_path` for *this* action: every recorded
        // input plus the auto-included executable. Sources read from
        // `source_bytes`; generated paths from `generated`. The
        // recorded action's `inputs` list mirrors what
        // `project_action(action).inputs` will iterate, so the map
        // covers everything `compute_action_cache_key` looks up
        // (Run/RunShell only).
        let mut inputs_by_path: BTreeMap<String, (Digest, bool)> = BTreeMap::new();
        for f in &action.inputs {
            let path = f.path();
            if f.is_source() {
                if let Some((bytes, x)) = source_bytes.get(&path) {
                    inputs_by_path.insert(path, (Digest::of_bytes(bytes), *x));
                }
            } else if let Some(entry) = generated.get(&path) {
                inputs_by_path.insert(path, entry.clone());
            }
        }
        if let ActionKind::Run { executable, .. } = &action.kind
            && !executable.path().starts_with('/')
        {
            let path = executable.path();
            if let Some((bytes, x)) = source_bytes.get(&path) {
                inputs_by_path
                    .entry(path)
                    .or_insert_with(|| (Digest::of_bytes(bytes), *x));
            }
        }

        match compute_action_cache_key(action, &cfg, &inputs_by_path) {
            Ok(d) => println!(
                "[{i:02}] {kind:12} {mnemonic:18} digest={}/{}",
                d.hash, d.size_bytes
            ),
            Err(AcKeyError::NotSpawnAction(_)) => {
                println!("[{i:02}] {kind:12} {mnemonic:18} (spawn-less; no REAPI digest)")
            }
            Err(e) => println!("[{i:02}] {kind:12} {mnemonic:18} error: {e}"),
        }
        for path in outputs {
            println!("        output:    {path}");
        }
        if std::env::var("VERBOSE").is_ok() {
            for input in &action.inputs {
                println!(
                    "        input:     {} ({})",
                    input.path(),
                    if input.is_source() { "source" } else { "gen" }
                );
            }
            if let ActionKind::Run {
                executable,
                arguments,
            } = &action.kind
            {
                println!(
                    "        argv:      {} {}",
                    executable.path(),
                    arguments.join(" ")
                );
            }
            for (k, v) in &action.env {
                println!("        env:       {k}={v}");
            }
            println!(
                "        use_default_shell_env: {}",
                action.uses_default_shell_env
            );
            if !action.execution_requirements.is_empty() {
                for (k, v) in &action.execution_requirements {
                    println!("        exec_req:  {k}={v}");
                }
            }
        }
    }

    ExitCode::SUCCESS
}
