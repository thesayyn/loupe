//! Loupe-side query — drives the engine for harness comparisons.
//!
//! `cquery` mirrors `bazel cquery <label>` (default output) and a
//! tiny subset of `--starlark:expr`. The engine returns plain Rust
//! data, so for the expressions our fixture suite uses we project
//! straight off `AnalyzedTarget` rather than running a Starlark
//! evaluator over a `target` value. Each expression is normalised
//! (whitespace stripped) and matched against a small table; novel
//! expressions return a `STUB:` marker so the test surfaces the
//! gap.

use anyhow::{Result, anyhow};

use loupe_core::{CanonicalRepo, Configuration, Engine, Label, PackagePath};

use crate::Fixture;

/// Run a cquery-equivalent query against the Loupe engine over `fx`.
///
/// `target` is a Bazel-style label (`//:world`, `//pkg:name`). `expr`
/// — Bazel's `--starlark:expr` — is not a real Starlark evaluator;
/// it pattern-matches a handful of shapes used by harness fixtures
/// and projects directly off the analyzed target. Unknown shapes
/// return `STUB: --starlark:expr unsupported (...)`.
pub fn cquery(fx: &Fixture, target: &str, expr: Option<&str>) -> Result<String> {
    let engine = Engine::new(fx.root().to_path_buf(), Configuration::default())?;
    let label = parse_user_label(target)?;
    let Some(e) = expr else {
        return engine.cquery(&label);
    };
    // Real `--output=starlark` evaluator: runs `e` with `target` bound to
    // the configured target (providers/files/actions all reachable), so
    // loupe-vs-Bazel output fidelity is directly comparable.
    engine.cquery_expr(&label, e)
}

/// Parse the small label dialect the harness accepts (`//pkg:name`,
/// `//pkg`, `:name`). Mirrors what `bazel cquery` itself takes on the
/// command line — `@@`-canonical inputs are also accepted.
fn parse_user_label(s: &str) -> Result<Label> {
    if let Some(rest) = s.strip_prefix("@@") {
        return Label::resolve(
            &format!("@@{rest}"),
            &loupe_core::PackageId::new(CanonicalRepo::main(), PackagePath::root()),
            &Default::default(),
        )
        .map_err(|e| anyhow!("parsing @@ label `{s}`: {e}"));
    }
    let (pkg_part, name) = match s.split_once(':') {
        Some((p, n)) => (p, n.to_string()),
        None => {
            // `//pkg` is shorthand for `//pkg:pkg-leaf`.
            let pkg = s.trim_start_matches("//");
            let leaf = pkg.rsplit('/').next().unwrap_or("").to_string();
            (s, leaf)
        }
    };
    let pkg_str = pkg_part.trim_start_matches("//");
    let path = if pkg_str.is_empty() {
        PackagePath::root()
    } else {
        PackagePath::new(pkg_str).map_err(|e| anyhow!("parsing package `{pkg_str}`: {e}"))?
    };
    Ok(Label::new(CanonicalRepo::main(), path, name))
}
