//! Loupe-side query — drives the engine for harness comparisons.
//!
//! Phase 5: a real `Engine` runs against the fixture. The harness only
//! handles the default cquery shape (no `--starlark:expr`) for now.
//! Anything passing `expr` returns an empty string with a `STUB:`
//! marker, and the calling test is responsible for either skipping or
//! comparing structurally — Phase 5+ will grow the eval.

use anyhow::{Result, anyhow};

use loupe_core::{CanonicalRepo, Configuration, Engine, Label, PackagePath};

use crate::Fixture;

/// Run a cquery-equivalent query against the Loupe engine over `fx`.
///
/// `target` is a Bazel-style label (`//:world`, `//pkg:name`). `expr`
/// — Bazel's `--starlark:expr` — is not implemented yet; passing
/// `Some(_)` returns a `STUB:` marker so harness comparisons see the
/// gap explicitly rather than silently passing.
pub fn cquery(fx: &Fixture, target: &str, expr: Option<&str>) -> Result<String> {
    let engine = Engine::new(fx.root().to_path_buf(), Configuration::default())?;
    let label = parse_user_label(target)?;
    if let Some(e) = expr {
        // STUB: --starlark:expr eval. Phase 5+ will implement.
        return Ok(format!("STUB: --starlark:expr unsupported (`{e}`)"));
    }
    engine.cquery(&label)
}

/// Parse the small label dialect the harness accepts (`//pkg:name`,
/// `//pkg`, `:name`). Mirrors what `bazel cquery` itself takes on the
/// command line — `@@`-canonical inputs are also accepted.
fn parse_user_label(s: &str) -> Result<Label> {
    if let Some(rest) = s.strip_prefix("@@") {
        return Label::resolve(
            &format!("@@{rest}"),
            &loupe_core::PackageId::new(CanonicalRepo::MAIN, PackagePath::root()),
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
    Ok(Label::new(CanonicalRepo::MAIN, path, name))
}
