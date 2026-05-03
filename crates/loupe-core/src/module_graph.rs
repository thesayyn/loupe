//! `MODULE.bazel` parsing and the workspace-wide repo mapping.
//!
//! Phase 2 only needs the main module's name + version and a trivial
//! repo mapping (apparent → canonical) where apparent names from the
//! main module's `module(name=…)` resolve to the main canonical repo
//! (empty canonical name in bzlmod). `bazel_dep` is accepted but not
//! resolved — Phase 3+ adds real dependency resolution.

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

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleGraph {
    pub main: ModuleDecl,
    /// apparent name → canonical repo. v1 only contains the main module.
    pub repo_mapping: BTreeMap<String, CanonicalRepo>,
}

impl ModuleGraph {
    pub fn resolve_apparent(&self, apparent: &str) -> Option<&CanonicalRepo> {
        self.repo_mapping.get(apparent)
    }
}

#[derive(Debug, Default, ProvidesStaticType)]
struct ModuleBuilder {
    main: RefCell<Option<ModuleDecl>>,
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

    fn bazel_dep(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = "")] version: &str,
        #[starlark(require = named, default = "")] repo_name: &str,
        #[starlark(require = named, default = false)] dev_dependency: bool,
    ) -> anyhow::Result<NoneType> {
        // STUB: bazel_dep is a no-op for v1; deps are assumed vendored.
        // Phase 3+ will populate the repo mapping from these calls.
        let _ = (name, version, repo_name, dev_dependency);
        Ok(NoneType)
    }
}

pub fn parse(workspace_root: &Path) -> Result<ModuleGraph> {
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
    let mut repo_mapping = BTreeMap::new();
    repo_mapping.insert(main.name.clone(), main.canonical.clone());
    Ok(ModuleGraph { main, repo_mapping })
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
    fn bazel_dep_is_accepted_but_does_not_populate_mapping() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
bazel_dep(name = "rules_python", version = "0.31.0")
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        // STUB behaviour: bazel_dep is accepted, but mapping is empty.
        assert!(graph.resolve_apparent("rules_python").is_none());
    }

    #[test]
    fn missing_module_call_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MODULE.bazel"), "").unwrap();
        let err = parse(dir.path()).unwrap_err();
        assert!(err.to_string().contains("did not call module"));
    }
}
