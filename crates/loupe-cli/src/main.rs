//! `loupe` CLI — debugging entry point.
//!
//! Today: one subcommand, `analyze`, which runs
//! [`Engine::analyze_workspace`] over a vendored Bzlmod workspace and
//! prints a structured report. Useful for pointing Loupe at e.g. a
//! `bazel mod vendor`'d copy of `rules_oci/examples` and seeing the
//! full failure surface in one shot.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use loupe_core::{Configuration, Engine};

const USAGE: &str = "\
loupe — read-only Bazel-compatible analysis engine

USAGE:
    loupe analyze <WORKSPACE> [OPTIONS]

ARGS:
    <WORKSPACE>   Path to a Bzlmod workspace root.

OPTIONS:
    --external-repo-root <PATH>   Directory holding vendored bzlmod deps
                                  (e.g. `<workspace>/vendor` from
                                  `bazel mod vendor`, or
                                  `<bazel info output_base>/external`).
    --lockfile <PATH>             Override the MODULE.bazel.lock path.
    --skip-kind <KIND>            Rule kind to skip (repeatable). Use
                                  for v1-out-of-scope rules, e.g.
                                  --skip-kind=oci_image_index.
    -h, --help                    Print this help.

EXIT CODE:
    0  every target analyzed cleanly (or was explicitly skipped).
    1  one or more targets failed analysis, or a package failed to load.
    2  CLI usage error.
";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("loupe=info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode> {
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    match args[0].as_str() {
        "analyze" => run_analyze(&args[1..]),
        other => bail!("unknown subcommand `{other}`. Run `loupe --help`."),
    }
}

#[derive(Default)]
struct AnalyzeArgs {
    workspace: Option<PathBuf>,
    external_repo_root: Option<PathBuf>,
    lockfile: Option<PathBuf>,
    skip_kinds: Vec<String>,
}

fn parse_analyze_args(args: &[String]) -> Result<AnalyzeArgs> {
    let mut out = AnalyzeArgs::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let (key, inline_value) = match a.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        let take_value = |i: &mut usize, name: &str| -> Result<String> {
            if let Some(v) = inline_value.clone() {
                Ok(v)
            } else {
                *i += 1;
                args.get(*i)
                    .cloned()
                    .with_context(|| format!("flag `{name}` requires a value"))
            }
        };
        match key {
            "--external-repo-root" => {
                out.external_repo_root = Some(PathBuf::from(take_value(&mut i, key)?))
            }
            "--lockfile" => out.lockfile = Some(PathBuf::from(take_value(&mut i, key)?)),
            "--skip-kind" => out.skip_kinds.push(take_value(&mut i, key)?),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other if other.starts_with("--") => bail!("unknown flag `{other}`"),
            other => {
                if out.workspace.is_some() {
                    bail!("unexpected positional argument `{other}`");
                }
                out.workspace = Some(PathBuf::from(other));
            }
        }
        i += 1;
    }
    Ok(out)
}

fn run_analyze(args: &[String]) -> Result<ExitCode> {
    let parsed = parse_analyze_args(args)?;
    let workspace = parsed
        .workspace
        .context("analyze requires a workspace path")?;

    let mut builder = Engine::builder(workspace, Configuration::default());
    if let Some(p) = parsed.external_repo_root {
        builder = builder.external_repo_root(p);
    }
    if let Some(p) = parsed.lockfile {
        builder = builder.bzlmod_lockfile_path(p);
    }
    let engine = builder.build()?;
    let skip_refs: Vec<&str> = parsed.skip_kinds.iter().map(String::as_str).collect();
    let report = engine.analyze_workspace(&skip_refs)?;

    println!("analyzed: {}", report.analyzed.len());
    println!("skipped:  {}", report.skipped.len());
    for s in &report.skipped {
        println!("  {} ({})", s.label, s.kind);
    }
    println!("failed:   {}", report.failed.len());
    for f in &report.failed {
        println!(
            "  {} ({}): {}",
            f.label,
            f.kind,
            indent_continuation(&f.error)
        );
    }
    println!("package errors: {}", report.package_errors.len());
    for pe in &report.package_errors {
        println!("  {}: {}", pe.package, indent_continuation(&pe.error));
    }

    if report.failed.is_empty() && report.package_errors.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

/// Indent any newlines in a multi-line error so the per-target prefix
/// lines up — keeps a long backtrace from masquerading as a new entry.
fn indent_continuation(s: &str) -> String {
    s.replace('\n', "\n    ")
}
