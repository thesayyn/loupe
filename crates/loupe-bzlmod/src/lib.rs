//! In-process bzlmod implementation for loupe.
//!
//! Replaces loupe's runtime dependency on the `bazel` CLI (which today
//! must populate `vendor/` or `<output_base>/external/` before analysis).
//! See `~/.claude/plans/write-up-a-plan-lexical-papert.md` for the design
//! and DESIGN.md for the architectural constraints.

pub mod archive;
pub mod dice;
pub mod env_policy;
pub mod fetch;
pub mod label_resolver;
pub mod integrity;
pub mod lockfile_specs;
pub mod materialize;
pub mod recorded_input;
pub mod registry;
pub mod repo_marker;
pub mod repo_spec;
pub mod resolution;
pub mod starlark_eval;
pub mod version;

pub use crate::dice::BzlmodResult;
pub use crate::dice::ExtendRepoSpecs;
pub use crate::dice::HasBzlmodCache;
pub use crate::dice::HasHttpClient;
pub use crate::dice::HasRepoSpecs;
pub use crate::dice::MaterializedRepoKey;
pub use crate::dice::SetBzlmodCache;
pub use crate::dice::SetHttpClient;
pub use crate::dice::SetRepoSpecs;
pub use crate::repo_spec::RepoSpec;
pub use crate::repo_spec::RepoSpecKind;
pub use crate::starlark_eval::ExtensionRepoCall;
pub use crate::starlark_eval::FrozenModuleExtension;
pub use crate::starlark_eval::FrozenRepositoryRule;
pub use crate::starlark_eval::ModuleCtx;
pub use crate::starlark_eval::ModuleExtensionOutput;
pub use crate::starlark_eval::ModuleTagInvocation;
pub use crate::starlark_eval::ModuleView;
pub use crate::starlark_eval::RepoAttrValue;
pub use crate::starlark_eval::RepositoryRule;
pub use crate::starlark_eval::eval_module_extension;
pub use crate::starlark_eval::register_for_bzlmod;
