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
use loupe_core::{
    CanonicalRepo, Configuration, Engine, FailedTarget, Label, PackageError, PackageId,
    PackagePath, SkippedTarget,
};

const USAGE: &str = "\
loupe — read-only Bazel-compatible analysis engine

USAGE:
    loupe analyze <WORKSPACE> [OPTIONS]
    loupe cquery  <WORKSPACE> --script <PATH> [OPTIONS]
    loupe fetch   <WORKSPACE> --cache <DIR> [--lockfile <PATH>] [--bcr | --registry <URL>]

ARGS:
    <WORKSPACE>   Path to a Bzlmod workspace root.

OPTIONS:
    --external-repo-root <PATH>   Directory holding external bzlmod
                                  repos (e.g. `<workspace>/vendor`
                                  from `bazel mod vendor`, or
                                  `<bazel info output_base>/external`).
                                  Repeatable: pass multiple roots and
                                  the resolver tries each in order,
                                  first match wins. Useful because
                                  `bazel mod vendor` skips
                                  `local = True` repository_rule
                                  outputs (e.g. `host_platform`),
                                  which only live in
                                  `<output_base>/external`.
    --lockfile <PATH>             Override the MODULE.bazel.lock path.
    --platforms <LABEL>           Target platform label (matches Bazel's
                                  `--platforms`). The label must resolve
                                  to a `platform(...)` target whose
                                  `constraint_values` are loadable.
                                  Canonical (`@@repo//pkg:name`),
                                  apparent (`@repo//pkg:name`), or main-
                                  repo (`//pkg:name`) forms accepted.
                                  Default: `@@platforms//host:host` —
                                  the same label Bazel uses. The
                                  workspace's vendor must include
                                  `@host_platform` (Bazel's repo_rule
                                  output); use `--platforms_expr` or
                                  a different `--platforms` if not.
    --skip-kind <KIND>            Rule kind to skip (repeatable). Use
                                  for v1-out-of-scope rules, e.g.
                                  --skip-kind=oci_image_index.
    --script <PATH>               (cquery only, required) Path to a
                                  Starlark file that must define
                                  `def main(targets): -> Any`. Loupe
                                  calls it once with the list of every
                                  analyzed Target (sorted by canonical
                                  label) and prints `str(...)` of the
                                  return value. The `targets` element
                                  type exposes `.label`, `.files`,
                                  `.actions[i].{mnemonic,inputs,
                                  outputs,argv,env,content}`,
                                  `.files_to_run`, `.output_groups`,
                                  and `[Provider]` indexing. No
                                  `load()` inside the script in v1.
    --repo_env <NAME=VAL|NAME=|NAME>
                                  Per-variable override for the
                                  environment seen by every
                                  `repository_rule` impl. Bazel-syntax:
                                  `NAME=VAL` sets, `NAME=` unsets,
                                  `NAME` pins the host value.
                                  Repeatable.
    --bzlmod[=BOOL], --no-bzlmod  Default: true. When on, the engine
                                  runs `use_extension`s and lazy-fetches
                                  external repos automatically:
                                    - `--cache` defaults to
                                      `<workspace>/.loupe/bzlmod` when
                                      unset.
                                    - `--bcr` is enabled when neither
                                      `--registry` nor `--bcr` was set.
                                  Pass `--no-bzlmod` (or
                                  `--bzlmod=false`) to opt out; you then
                                  supply `--cache`/`--bcr`/
                                  `--external-repo-root` yourself, as
                                  before. Explicit `--cache`,
                                  `--registry`, or `--bcr` always
                                  override the defaults.
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
        "cquery" => run_cquery(&args[1..]),
        "fetch" => run_fetch(&args[1..]),
        other => bail!("unknown subcommand `{other}`. Run `loupe --help`."),
    }
}

#[derive(Default)]
struct FetchArgs {
    workspace: Option<PathBuf>,
    cache: Option<PathBuf>,
    lockfile: Option<PathBuf>,
    /// Custom registry URL — when set, registers a BCR-format registry
    /// pointed at this URL. Mutually exclusive with `--bcr`.
    registry: Option<String>,
    /// Shortcut for `--registry https://bcr.bazel.build`.
    bcr: bool,
    repo_env: Vec<String>,
}

fn parse_fetch_args(args: &[String]) -> Result<FetchArgs> {
    let mut out = FetchArgs::default();
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
            "--cache" => out.cache = Some(PathBuf::from(take_value(&mut i, key)?)),
            "--lockfile" => out.lockfile = Some(PathBuf::from(take_value(&mut i, key)?)),
            "--registry" => out.registry = Some(take_value(&mut i, key)?),
            "--bcr" => out.bcr = true,
            "--repo_env" => out.repo_env.push(take_value(&mut i, key)?),
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

fn run_fetch(args: &[String]) -> Result<ExitCode> {
    let parsed = parse_fetch_args(args)?;
    let workspace = parsed
        .workspace
        .context("fetch requires a workspace path")?;
    let cache = parsed.cache.context("fetch requires --cache <DIR>")?;
    if parsed.bcr && parsed.registry.is_some() {
        bail!("--bcr and --registry are mutually exclusive");
    }

    let mut builder = Engine::builder(workspace, Configuration::default()).with_bzlmod_cache(cache);
    if let Some(p) = parsed.lockfile {
        builder = builder.bzlmod_lockfile_path(p);
    }
    if parsed.bcr {
        builder = builder.with_bcr();
    } else if let Some(url) = parsed.registry {
        builder = builder.with_registry(loupe_bzlmod::registry::Registry::custom(url));
    }
    if !parsed.repo_env.is_empty() {
        let mut overrides = std::collections::BTreeMap::new();
        for s in &parsed.repo_env {
            let (name, ov) = loupe_bzlmod::env_policy::parse_repo_env_flag(s);
            overrides.insert(name, ov);
        }
        builder = builder.with_repo_env(overrides);
    }
    let engine = builder.build()?;
    let report = engine.fetch_all()?;

    println!("materialised: {}", report.materialised.len());
    for (canonical, path) in &report.materialised {
        println!("  {} -> {}", canonical, path.display());
    }
    println!("errors: {}", report.errors.len());
    for (canonical, err) in &report.errors {
        println!("  {}: {}", canonical, indent_continuation(err));
    }

    if report.errors.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

#[derive(Default)]
struct AnalyzeArgs {
    workspace: Option<PathBuf>,
    external_repo_roots: Vec<PathBuf>,
    lockfile: Option<PathBuf>,
    platforms: Option<String>,
    platforms_expr: Option<String>,
    skip_kinds: Vec<String>,
    bzlmod_cache: Option<PathBuf>,
    registry: Option<String>,
    bcr: bool,
    repo_env: Vec<String>,
    /// `cquery` subcommand only — Starlark file with `def main(targets)`.
    script: Option<PathBuf>,
    /// Whether to auto-default cache + registry for bzlmod resolution.
    /// `None` = user didn't pass the flag (treat as on); `Some(true/false)`
    /// = explicit override.
    bzlmod: Option<bool>,
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
            "--external-repo-root" => out
                .external_repo_roots
                .push(PathBuf::from(take_value(&mut i, key)?)),
            "--lockfile" => out.lockfile = Some(PathBuf::from(take_value(&mut i, key)?)),
            "--platforms" => out.platforms = Some(take_value(&mut i, key)?),
            "--platforms_expr" => out.platforms_expr = Some(take_value(&mut i, key)?),
            "--skip-kind" => out.skip_kinds.push(take_value(&mut i, key)?),
            "--script" | "-s" => out.script = Some(PathBuf::from(take_value(&mut i, key)?)),
            "--cache" => out.bzlmod_cache = Some(PathBuf::from(take_value(&mut i, key)?)),
            "--registry" => out.registry = Some(take_value(&mut i, key)?),
            "--bcr" => out.bcr = true,
            "--repo_env" => out.repo_env.push(take_value(&mut i, key)?),
            "--bzlmod" => {
                out.bzlmod = Some(match inline_value.as_deref() {
                    None => true,
                    Some(v) => parse_bool(v, key)?,
                });
            }
            "--no-bzlmod" => {
                if inline_value.is_some() {
                    bail!("flag `--no-bzlmod` does not take a value");
                }
                out.bzlmod = Some(false);
            }
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

fn parse_bool(v: &str, name: &str) -> Result<bool> {
    match v {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        other => bail!("flag `{name}` expects a boolean, got `{other}`"),
    }
}

fn build_engine_from_args(
    mut parsed: AnalyzeArgs,
    subcommand: &str,
) -> Result<(Engine, Vec<String>)> {
    let workspace = parsed
        .workspace
        .clone()
        .with_context(|| format!("{subcommand} requires a workspace path"))?;

    // `--bzlmod` (default on) auto-supplies a predictable cache dir and
    // turns on BCR when the user hasn't picked their own. Explicit
    // `--cache`/`--registry`/`--bcr` always win. `--no-bzlmod` opts out
    // entirely — the user is back to today's vendor-or-bring-your-own
    // workflow.
    let bzlmod_on = parsed.bzlmod.unwrap_or(true);
    if bzlmod_on {
        if parsed.bzlmod_cache.is_none() {
            parsed.bzlmod_cache = Some(workspace.join(".loupe").join("bzlmod"));
        }
        if !parsed.bcr && parsed.registry.is_none() {
            parsed.bcr = true;
        }
    }

    let mut config = Configuration::default();
    let main_root = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());
    if parsed.platforms_expr.is_some() && parsed.platforms.is_some() {
        bail!("--platforms_expr and --platforms are mutually exclusive");
    }
    // Resolution precedence:
    //   1. `--platforms <label>` — verbatim.
    //   2. `--platforms_expr <body>` — target the synthesised
    //      `@@_loupe_platforms//:current`.
    //   3. Neither — default to Bazel's canonical
    //      `@@platforms//host:host`. The vendor must include
    //      `@host_platform`; if it doesn't, the run fails and
    //      the user can either pass `--platforms` or
    //      `--platforms_expr`.
    let label_str: &str = match (&parsed.platforms, parsed.platforms_expr.is_some()) {
        (Some(s), _) => s,
        (None, true) => "@@_loupe_platforms//:current",
        (None, false) => "@@platforms//host:host",
    };
    let label = Label::resolve(label_str, &main_root, &Default::default())
        .with_context(|| format!("target platform `{label_str}`"))?;
    config = config.with_target_platform(label);
    let mut builder = Engine::builder(workspace, config);
    if let Some(expr) = parsed.platforms_expr {
        builder = builder.platforms_expr(expr);
    }
    for p in parsed.external_repo_roots {
        builder = builder.external_repo_root(p);
    }
    if let Some(p) = parsed.lockfile {
        builder = builder.bzlmod_lockfile_path(p);
    }
    if let Some(cache) = parsed.bzlmod_cache {
        builder = builder.with_bzlmod_cache(cache);
    }
    if parsed.bcr && parsed.registry.is_some() {
        bail!("--bcr and --registry are mutually exclusive");
    }
    if parsed.bcr {
        builder = builder.with_bcr();
    } else if let Some(url) = parsed.registry {
        builder = builder.with_registry(loupe_bzlmod::registry::Registry::custom(url));
    }
    if !parsed.repo_env.is_empty() {
        let mut overrides = std::collections::BTreeMap::new();
        for s in &parsed.repo_env {
            let (name, ov) = loupe_bzlmod::env_policy::parse_repo_env_flag(s);
            overrides.insert(name, ov);
        }
        builder = builder.with_repo_env(overrides);
    }
    let engine = builder.build()?;
    Ok((engine, parsed.skip_kinds))
}

/// Write the analyzed/skipped/failed/package-errors summary block.
/// Shared between `analyze` (stdout) and `cquery` (stderr) so the two
/// stay in sync. Caller picks the destination by passing a writer.
fn write_summary(
    w: &mut impl std::io::Write,
    analyzed: usize,
    skipped: &[SkippedTarget],
    failed: &[FailedTarget],
    package_errors: &[PackageError],
) {
    let _ = writeln!(w, "analyzed: {analyzed}");
    let _ = writeln!(w, "skipped:  {}", skipped.len());
    for s in skipped {
        let _ = writeln!(w, "  {} ({})", s.label, s.kind);
    }
    let _ = writeln!(w, "failed:   {}", failed.len());
    for f in failed {
        let _ = writeln!(
            w,
            "  {} ({}): {}",
            f.label,
            f.kind,
            indent_continuation(&f.error)
        );
    }
    let _ = writeln!(w, "package errors: {}", package_errors.len());
    for pe in package_errors {
        let _ = writeln!(w, "  {}: {}", pe.package, indent_continuation(&pe.error));
    }
}

fn run_analyze(args: &[String]) -> Result<ExitCode> {
    let parsed = parse_analyze_args(args)?;
    if parsed.script.is_some() {
        bail!("--script applies to `cquery`, not `analyze`");
    }
    let (engine, skip_kinds) = build_engine_from_args(parsed, "analyze")?;
    let skip_refs: Vec<&str> = skip_kinds.iter().map(String::as_str).collect();
    let report = engine.analyze_workspace(&skip_refs)?;

    write_summary(
        &mut std::io::stdout(),
        report.analyzed.len(),
        &report.skipped,
        &report.failed,
        &report.package_errors,
    );

    if report.failed.is_empty() && report.package_errors.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

fn run_cquery(args: &[String]) -> Result<ExitCode> {
    let parsed = parse_analyze_args(args)?;
    let script = parsed
        .script
        .clone()
        .context("cquery requires --script <PATH>")?;
    let (engine, skip_kinds) = build_engine_from_args(parsed, "cquery")?;
    let skip_refs: Vec<&str> = skip_kinds.iter().map(String::as_str).collect();
    let report = engine.cquery_script(&script, &skip_refs)?;

    // Script output on stdout, summary on stderr — pipelines that
    // want only the script's return value can swallow stderr cleanly.
    println!("{}", report.output);
    write_summary(
        &mut std::io::stderr(),
        report.analyzed,
        &report.skipped,
        &report.failed,
        &report.package_errors,
    );

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
