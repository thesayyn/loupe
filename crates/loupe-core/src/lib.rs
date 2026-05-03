//! `loupe-core` — the engine.
//!
//! See `DESIGN.md` and `PLAN.md` at the repo root.

pub mod build_eval;
pub mod engine;
pub mod label;
pub mod module_graph;
pub mod package;
pub mod workspace;

pub use engine::{Configuration, Engine};
pub use label::{CanonicalRepo, Label, PackageId, PackagePath, ParseLabelError};
pub use module_graph::{ModuleDecl, ModuleGraph};
pub use package::{AttrValue, Package, UnconfiguredTarget};
