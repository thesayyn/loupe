//! In-process bzlmod implementation for loupe.
//!
//! Drives BCR resolution and module-extension evaluation against the
//! workspace's `MODULE.bazel`, materialises archives into the engine's
//! bzlmod cache, and surfaces `RepoSpec` / `BzlmodGraph` records so
//! downstream `ModuleGraphKey` consumers can read the resolved state
//! without parsing canonical-name strings.

pub mod archive;
pub mod bzlmod_graph;
pub mod dice;
pub mod env_policy;
pub mod fetch;
pub mod integrity;
pub mod label_resolver;
pub mod lockfile_specs;
pub mod materialize;
pub mod patch;
pub mod recorded_input;
pub mod registry;
pub mod repo_marker;
pub mod repo_spec;
pub mod resolution;
pub mod starlark_eval;
pub mod version;

pub use crate::bzlmod_graph::BzlmodGraphKey;
pub use crate::bzlmod_graph::force_bzlmod_graph;
pub use crate::bzlmod_graph::inject_bzlmod_graph;
pub use crate::dice::BzlmodResult;
pub use crate::dice::HasBzlmodCache;
pub use crate::dice::HasHttpClient;
pub use crate::dice::MaterializedRepoKey;
pub use crate::dice::RepoSpecKey;
pub use crate::dice::RepoSpecsKey;
pub use crate::dice::SetBzlmodCache;
pub use crate::dice::SetHttpClient;
pub use crate::dice::SetRepoSpecs;
pub use crate::dice::force_repo_spec;
pub use crate::dice::force_repo_specs_map;
pub use crate::dice::inject_repo_specs;
pub use crate::repo_spec::RepoSpec;
pub use crate::repo_spec::RepoSpecKind;
pub use crate::starlark_eval::ExtensionRepoCall;
pub use crate::starlark_eval::FrozenModuleExtension;
pub use crate::starlark_eval::FrozenRepositoryRule;
pub use crate::starlark_eval::FrozenTagClass;
pub use crate::starlark_eval::ModuleCtx;
pub use crate::starlark_eval::ModuleExtensionOutput;
pub use crate::starlark_eval::ModuleTagInvocation;
pub use crate::starlark_eval::ModuleView;
pub use crate::starlark_eval::RepoAttrValue;
pub use crate::starlark_eval::RepositoryRule;
pub use crate::starlark_eval::eval_module_extension;
pub use crate::starlark_eval::register_for_bzlmod;
