//! Shared label types for the loupe workspace.
//!
//! Splits the `Label` family out of `loupe-core` so `loupe-bzlmod`
//! (which lives below `loupe-core` in the dep DAG) can talk about
//! `LabelValue` natively in repo-rule `ctx` methods without a string
//! protocol.
//!
//! - [`types`] — analysis-time canonical labels (`Label`, `PackageId`,
//!   `CanonicalRepo`, `PackagePath`, `ParseLabelError`).
//! - [`starlark`] — the `Label("...")` Starlark value type users
//!   construct at rule-eval time.

pub mod repo_mapping;
pub mod starlark;
pub mod types;

pub use repo_mapping::RepoMapping;
pub use starlark::LabelValue;
pub use types::{CanonicalRepo, Label, PackageId, PackagePath, ParseLabelError};
