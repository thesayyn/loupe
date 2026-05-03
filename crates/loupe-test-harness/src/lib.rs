//! Cquery comparison harness — Phase 1.
//!
//! Wraps `bazel cquery --output=starlark` and a (currently stub) Loupe
//! query, then produces a structural diff. The harness exists *before*
//! engine code so that every Loupe feature lands with a comparison
//! against ground truth.
//!
//! See `PLAN.md` Phase 1 and `DESIGN.md` for context.

use std::path::{Path, PathBuf};

pub mod bazel;
pub mod diff;
pub mod loupe;

pub use diff::Report;

/// A Bazel workspace on disk that the harness can query.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub root: PathBuf,
}

impl Fixture {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Fixture { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Run both Bazel and Loupe against the same query and return a diff.
pub fn compare(fx: &Fixture, target: &str, expr: Option<&str>) -> anyhow::Result<Report> {
    let bazel_out = bazel::cquery(fx, target, expr)?;
    let loupe_out = loupe::cquery(fx, target, expr)?;
    Ok(diff::compare(&bazel_out, &loupe_out))
}
