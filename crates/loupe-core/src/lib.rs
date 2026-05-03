//! `loupe-core` — the engine.
//!
//! See `DESIGN.md` and `PLAN.md` at the repo root.

pub mod action;
pub mod analysis;
pub mod build_eval;
pub mod bzl_eval;
pub mod dice_keys;
pub mod engine;
pub mod label;
pub mod module_graph;
pub mod package;
pub mod starlark_types;
pub mod workspace;

pub use action::{Action, ActionKind};
pub use analysis::{AnalyzedTarget, CapturedProvider, Configuration};
pub use engine::Engine;
pub use label::{CanonicalRepo, Label, PackageId, PackagePath, ParseLabelError};
pub use module_graph::{ModuleDecl, ModuleGraph};
pub use package::{AttrValue, Package, UnconfiguredTarget};
