//! `MODULE.bazel` parsing and the workspace-wide repo mapping.
//!
//! Each `bazel_dep(name=X, version=Y)` call is recorded and contributes
//! one entry to the workspace's apparent → canonical repo mapping.
//! Loupe doesn't fetch — DESIGN.md mandates "all deps assumed vendored
//! on disk." The engine's external-repo root supplies the on-disk path
//! for each canonical repo (typically derived from `bazel info
//! output_base` or a `bazel mod vendor` output directory).
//!
//! ## Canonical naming
//!
//! Bazel 7's bzlmod uses the convention `<apparent>+` for canonical
//! repo names when a single version of a module is in the graph
//! (`rules_oci+`, `aspect_bazel_lib+`, …). Older bzlmod versions used
//! `<apparent>~<version>` and Bazel 6's WORKSPACE used the apparent
//! name directly. v1 mirrors the Bazel 7 `+` convention; if a future
//! caller needs a different scheme we add an override hook then.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use starlark::any::ProvidesStaticType;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::none::NoneType;

use crate::label::CanonicalRepo;

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleDecl {
    pub name: String,
    pub version: String,
    pub canonical: CanonicalRepo,
}

/// One `bazel_dep(...)` declaration from `MODULE.bazel`.
#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleDep {
    /// `name=` arg — what BUILD/`.bzl` files reference as `@<name>`.
    pub apparent: String,
    /// `version=` arg. Loupe doesn't enforce version selection at v1
    /// (deps are assumed vendored), but the field rides along so a
    /// future MVS pass can land without re-parsing.
    pub version: String,
    /// Canonical repo name. Matches Bazel 7's `<apparent>+` convention
    /// so on-disk paths line up with `<output_base>/external/<canonical>`
    /// (or a vendored equivalent).
    pub canonical: CanonicalRepo,
}

/// Compute the canonical repo name Bazel 7 bzlmod assigns to a
/// `bazel_dep` call. Public so callers wiring an external-repo root
/// can predict the directory name without re-parsing `MODULE.bazel`.
pub fn canonical_for_apparent(apparent: &str) -> CanonicalRepo {
    CanonicalRepo::new(format!("{apparent}+"))
}

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleGraph {
    pub main: ModuleDecl,
    /// Every `bazel_dep` declaration in declaration order.
    pub deps: Vec<ModuleDep>,
    /// apparent name → canonical repo. Contains the main module and
    /// every `bazel_dep`.
    pub repo_mapping: BTreeMap<String, CanonicalRepo>,
    /// Per-repo apparent → canonical mappings sourced from
    /// `MODULE.bazel.lock`. Each external repo carries its own view of
    /// transitive deps; resolving a load from inside repo X consults
    /// `per_repo_mapping[X]` first, then falls back to the main
    /// `repo_mapping`. See [`crate::lockfile`].
    pub per_repo_mapping: BTreeMap<CanonicalRepo, BTreeMap<String, CanonicalRepo>>,
    /// Raw label strings passed to `register_toolchains(...)` in
    /// `MODULE.bazel`, in declaration order. Stored unparsed; the
    /// toolchain resolver canonicalises them against the main repo at
    /// lookup time. v1 doesn't expand `:all` patterns.
    pub registered_toolchains: Vec<String>,
    /// Raw label strings passed to `register_execution_platforms(...)`.
    /// Captured for completeness; v1 doesn't run platform resolution.
    pub registered_execution_platforms: Vec<String>,
}

impl ModuleGraph {
    pub fn resolve_apparent(&self, apparent: &str) -> Option<&CanonicalRepo> {
        self.repo_mapping.get(apparent)
    }
}

#[derive(Debug, Default, ProvidesStaticType)]
struct ModuleBuilder {
    main: RefCell<Option<ModuleDecl>>,
    deps: RefCell<Vec<ModuleDep>>,
    registered_toolchains: RefCell<Vec<String>>,
    registered_execution_platforms: RefCell<Vec<String>>,
}

fn builder_from_eval<'a, 'v>(eval: &'a Evaluator<'v, '_, '_>) -> anyhow::Result<&'a ModuleBuilder> {
    eval.extra
        .ok_or_else(|| anyhow::anyhow!("internal: no ModuleBuilder on evaluator"))?
        .downcast_ref::<ModuleBuilder>()
        .ok_or_else(|| anyhow::anyhow!("internal: extra is not a ModuleBuilder"))
}

fn unpack_label_args<'v>(
    args: &starlark::eval::Arguments<'v, '_>,
    eval: &Evaluator<'v, '_, '_>,
    fn_name: &str,
) -> anyhow::Result<Vec<String>> {
    let positions = args
        .positions(eval.heap())
        .map_err(|e| anyhow::anyhow!("{fn_name}: {e}"))?;
    let mut out = Vec::new();
    for v in positions {
        let s = v.unpack_str().ok_or_else(|| {
            anyhow::anyhow!(
                "{fn_name}: positional arg must be a string label, got {}",
                v.get_type()
            )
        })?;
        out.push(s.to_string());
    }
    Ok(out)
}

#[starlark_module]
fn module_bazel_globals(b: &mut GlobalsBuilder) {
    fn module<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = "")] version: &str,
        #[starlark(require = named, default = "")] compatibility_level: &str,
        #[starlark(require = named, default = "")] repo_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (compatibility_level, repo_name); // accepted; not used in v1
        let builder = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("internal: no ModuleBuilder on evaluator"))?
            .downcast_ref::<ModuleBuilder>()
            .ok_or_else(|| anyhow::anyhow!("internal: extra is not a ModuleBuilder"))?;
        let mut slot = builder.main.borrow_mut();
        if slot.is_some() {
            return Err(anyhow::anyhow!("MODULE.bazel called module(...) twice"));
        }
        *slot = Some(ModuleDecl {
            name: name.to_string(),
            version: version.to_string(),
            canonical: CanonicalRepo::MAIN,
        });
        Ok(NoneType)
    }

    /// `register_toolchains(*labels)` — Bazel's bzlmod toolchain
    /// registration. v1 captures the labels in declaration order; the
    /// toolchain resolver walks this list at `ctx.toolchains[T]`
    /// lookup time. STUB: doesn't expand `:all`-style patterns; each
    /// label is treated as exact.
    fn register_toolchains<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let labels = unpack_label_args(args, eval, "register_toolchains")?;
        let builder = builder_from_eval(eval)?;
        builder.registered_toolchains.borrow_mut().extend(labels);
        Ok(NoneType)
    }

    /// `register_execution_platforms(*labels)`. v1 captures the
    /// labels; platform resolution itself isn't run.
    fn register_execution_platforms<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let labels = unpack_label_args(args, eval, "register_execution_platforms")?;
        let builder = builder_from_eval(eval)?;
        builder
            .registered_execution_platforms
            .borrow_mut()
            .extend(labels);
        Ok(NoneType)
    }

    /// `use_extension(bzl_label, extension_name, dev_dependency=False, isolate=False)`.
    /// STUB: returns a placeholder; bzlmod extension semantics aren't
    /// modelled. The lockfile already gives us per-repo mappings and
    /// generated repos, so we don't need to execute the extension.
    fn use_extension<'v>(
        #[starlark(require = pos)] _bzl_label: &str,
        #[starlark(require = pos)] _extension_name: &str,
        #[starlark(require = named, default = false)] _dev_dependency: bool,
        #[starlark(require = named, default = false)] _isolate: bool,
    ) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `use_repo(extension_proxy, *names)`. STUB.
    fn use_repo<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `single_version_override(...)`. STUB — v1 doesn't run MVS.
    fn single_version_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `archive_override(...)`. STUB.
    fn archive_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `local_path_override(module_name, path)`. STUB.
    fn local_path_override<'v>(
        #[starlark(require = named)] _module_name: Option<&str>,
        #[starlark(require = named)] _path: Option<&str>,
    ) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `git_override(...)`. STUB.
    fn git_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `bazel_dep(name, version=…, repo_name=…, dev_dependency=…)`.
    /// Recorded into the module graph; `repo_name` (the apparent-name
    /// override) lets `MODULE.bazel` re-bind a dep under a different
    /// short name. v1 honors `repo_name` if set, falls back to `name`.
    fn bazel_dep<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = "")] version: &str,
        #[starlark(require = named, default = "")] repo_name: &str,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = dev_dependency; // accepted; v1 doesn't distinguish
        let builder = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("internal: no ModuleBuilder on evaluator"))?
            .downcast_ref::<ModuleBuilder>()
            .ok_or_else(|| anyhow::anyhow!("internal: extra is not a ModuleBuilder"))?;
        let apparent = if repo_name.is_empty() {
            name
        } else {
            repo_name
        };
        builder.deps.borrow_mut().push(ModuleDep {
            apparent: apparent.to_string(),
            version: version.to_string(),
            canonical: canonical_for_apparent(name),
        });
        Ok(NoneType)
    }
}

pub fn parse(workspace_root: &Path) -> Result<ModuleGraph> {
    parse_with_lockfile_override(workspace_root, None)
}

/// Same as [`parse`] but lets the caller override the
/// `MODULE.bazel.lock` path. Bazel 7 writes the rich lockfile to
/// `<output_base>/MODULE.bazel.lock` rather than the workspace, so
/// callers needing the per-repo records point this at the
/// `output_base` copy.
pub fn parse_with_lockfile_override(
    workspace_root: &Path,
    lockfile_path: Option<std::path::PathBuf>,
) -> Result<ModuleGraph> {
    let path = workspace_root.join("MODULE.bazel");
    let src =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let ast = AstModule::parse("MODULE.bazel", src, &Dialect::Standard)
        .map_err(|e| anyhow::anyhow!("parsing MODULE.bazel: {e}"))?;
    let globals = GlobalsBuilder::new().with(module_bazel_globals).build();
    let builder = ModuleBuilder::default();
    Module::with_temp_heap(|module| -> Result<()> {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&builder);
        eval.eval_module(ast, &globals)
            .map_err(|e| anyhow!("evaluating MODULE.bazel: {e}"))?;
        Ok(())
    })?;
    let main = builder
        .main
        .into_inner()
        .ok_or_else(|| anyhow::anyhow!("MODULE.bazel did not call module(...)"))?;
    let deps = builder.deps.into_inner();
    let registered_toolchains = builder.registered_toolchains.into_inner();
    let registered_execution_platforms = builder.registered_execution_platforms.into_inner();
    let mut repo_mapping = BTreeMap::new();
    repo_mapping.insert(main.name.clone(), main.canonical.clone());
    for dep in &deps {
        repo_mapping.insert(dep.apparent.clone(), dep.canonical.clone());
    }
    let lockfile =
        match lockfile_path {
            Some(p) => crate::lockfile::load_from(p)
                .with_context(|| "loading overridden MODULE.bazel.lock")?,
            None => crate::lockfile::load(workspace_root)
                .with_context(|| "loading MODULE.bazel.lock")?,
        };
    Ok(ModuleGraph {
        main,
        deps,
        repo_mapping,
        per_repo_mapping: lockfile.per_repo_mapping,
        registered_toolchains,
        registered_execution_platforms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_minimal_module_bazel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "1.2.3")"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        assert_eq!(graph.main.name, "demo");
        assert_eq!(graph.main.version, "1.2.3");
        assert!(graph.main.canonical.is_main());
        assert_eq!(
            graph.resolve_apparent("demo").unwrap(),
            &CanonicalRepo::MAIN
        );
    }

    #[test]
    fn bazel_dep_populates_repo_mapping_with_canonical_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
bazel_dep(name = "rules_python", version = "0.31.0")
bazel_dep(name = "rules_oci", version = "2.2.6", repo_name = "oci")
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        assert_eq!(graph.deps.len(), 2);
        // Default: apparent equals name.
        assert_eq!(
            graph.resolve_apparent("rules_python").unwrap(),
            &CanonicalRepo::new("rules_python+")
        );
        // `repo_name` overrides the apparent: BUILD code can reach
        // `@oci//...` even though the canonical is still `rules_oci+`.
        assert_eq!(
            graph.resolve_apparent("oci").unwrap(),
            &CanonicalRepo::new("rules_oci+")
        );
        assert!(graph.resolve_apparent("rules_oci").is_none());
    }

    #[test]
    fn register_toolchains_captures_labels_in_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
register_toolchains(
    "//toolchains:tc_first",
    "@aspect_bazel_lib//toolchains:tar_toolchain",
    ":tc_local",
)
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        assert_eq!(
            graph.registered_toolchains,
            vec![
                "//toolchains:tc_first".to_string(),
                "@aspect_bazel_lib//toolchains:tar_toolchain".to_string(),
                ":tc_local".to_string(),
            ],
        );
    }

    #[test]
    fn register_execution_platforms_captures_labels() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
register_execution_platforms("//platforms:linux_x86_64")
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        assert_eq!(
            graph.registered_execution_platforms,
            vec!["//platforms:linux_x86_64".to_string()],
        );
    }

    #[test]
    fn register_toolchains_rejects_non_string_arg() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
register_toolchains(123)
"#,
        )
        .unwrap();
        let err = parse(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("must be a string label"),
            "expected string-label error, got: {err}"
        );
    }

    #[test]
    fn missing_module_call_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MODULE.bazel"), "").unwrap();
        let err = parse(dir.path()).unwrap_err();
        assert!(err.to_string().contains("did not call module"));
    }
}
