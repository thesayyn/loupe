//! BUILD-file evaluator.
//!
//! Phase 3 wires real `load()`: each load resolves to a real
//! `FrozenModule` produced by `BzlLoadKey`. Loaded `RuleClass` values
//! are wrapped in `Rule`s so the BUILD-side import alias becomes the
//! recorded rule kind. `provider()`, `attr.*`, `rule()`, and
//! `aspect()` live in `crate::starlark_types`.
//!
//! BUILD evaluation itself is sync: `PackageKey::compute` resolves
//! every load statement asynchronously through DICE, then calls
//! `evaluate` with the pre-loaded modules in hand.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use starlark::any::ProvidesStaticType;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::{Evaluator, ReturnFileLoader};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::{Value, ValueLike};

use crate::label::{Label, PackageId};
use crate::package::{AttrValue, Package, UnconfiguredTarget};
use crate::starlark_types::{self, rule::Rule};

/// Stored in `Evaluator::extra` while a BUILD file evaluates. Loaded
/// rule wrappers (`Rule`) record `UnconfiguredTarget`s through
/// `record_target`; native helpers like `glob()` reach the workspace
/// disk through `workspace_root()`. `repo_mapping` lets `Rule::invoke`
/// resolve label-typed attr strings into canonical `Label`s without
/// re-parsing them later.
#[derive(Debug, ProvidesStaticType)]
pub struct BuildContext {
    /// On-disk root of the package's repo. For main-repo packages this
    /// equals the workspace root; for external packages it's
    /// `<external_root>/<canonical>`. `glob()` and other package-local
    /// IO compose this with `package.path` to reach the actual files.
    repo_root: std::path::PathBuf,
    package: PackageId,
    repo_mapping: BTreeMap<String, crate::label::CanonicalRepo>,
    targets: RefCell<BTreeMap<String, UnconfiguredTarget>>,
}

impl BuildContext {
    fn new(
        repo_root: std::path::PathBuf,
        package: PackageId,
        repo_mapping: BTreeMap<String, crate::label::CanonicalRepo>,
    ) -> Self {
        BuildContext {
            repo_root,
            package,
            repo_mapping,
            targets: RefCell::new(BTreeMap::new()),
        }
    }

    /// Root of the package's repo on disk. Used by package-local IO
    /// (e.g. `glob()`) to locate files.
    pub fn repo_root(&self) -> &std::path::Path {
        &self.repo_root
    }

    /// Backwards-compatible alias for [`Self::repo_root`]. Renamed
    /// after Phase 6 added external-repo support — workspace_root is
    /// still right for main-repo packages.
    pub fn workspace_root(&self) -> &std::path::Path {
        &self.repo_root
    }

    pub fn package(&self) -> &PackageId {
        &self.package
    }

    pub fn repo_mapping(&self) -> &BTreeMap<String, crate::label::CanonicalRepo> {
        &self.repo_mapping
    }

    pub fn record_target(&self, target: UnconfiguredTarget) -> Result<()> {
        let mut targets = self.targets.borrow_mut();
        if targets.contains_key(target.label.name()) {
            return Err(anyhow!(
                "duplicate target name `{}` in package {}",
                target.label.name(),
                self.package
            ));
        }
        targets.insert(target.label.name().to_string(), target);
        Ok(())
    }

    fn into_targets(self) -> BTreeMap<String, UnconfiguredTarget> {
        self.targets.into_inner()
    }
}

/// Convert a Starlark value into our captured `AttrValue` shape. Public
/// so `Rule::invoke` can use it through the same code path as the
/// universal kwargs (`name`, `visibility`, `tags`).
pub fn unpack_attr<'v>(v: Value<'v>) -> AttrValue {
    if v.is_none() {
        return AttrValue::None;
    }
    if let Some(s) = v.unpack_str() {
        return AttrValue::String(s.to_string());
    }
    if let Some(b) = v.unpack_bool() {
        return AttrValue::Bool(b);
    }
    if let Some(i) = v.unpack_i32() {
        return AttrValue::Int(i as i64);
    }
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        return AttrValue::List(list.iter().map(unpack_attr).collect());
    }
    if let Some(dict) = starlark::values::dict::DictRef::from_value(v) {
        let mut out = BTreeMap::new();
        for (k, val) in dict.iter() {
            let key = match k.unpack_str() {
                Some(s) => s.to_string(),
                None => k.to_string(),
            };
            out.insert(key, unpack_attr(val));
        }
        return AttrValue::Dict(out);
    }
    // `select(...)` returns a SelectorList; preserve the full branch
    // dict so the analysis phase can resolve against the configured
    // platform / config_settings.
    if let Some(sl) = v.downcast_ref::<crate::starlark_types::selector::SelectorList>() {
        return AttrValue::Selector(sl.clone());
    }
    AttrValue::Opaque(v.to_string())
}

/// `(load_module_id, [requested_symbols])` extracted from the BUILD AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadStmt {
    pub module_id: String,
    pub symbols: Vec<(String, String)>, // (local_name, source_name)
}

pub fn extract_loads(ast: &AstModule) -> Vec<LoadStmt> {
    ast.loads()
        .into_iter()
        .map(|l| LoadStmt {
            module_id: l.module_id.to_string(),
            symbols: l
                .symbols
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        })
        .collect()
}

pub fn locate_build_file(pkg_dir: &Path) -> Result<std::path::PathBuf> {
    for name in ["BUILD.bazel", "BUILD"] {
        let p = pkg_dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "no BUILD or BUILD.bazel file in {}",
        pkg_dir.display()
    ))
}

/// Read and parse the BUILD file for `package`. Returns `(filename,
/// source, ast)` so callers can extract loads before evaluation.
///
/// External repos resolve under `external_repo_root/<canonical>` —
/// matches the layout of `<bazel info output_base>/external/` and
/// `bazel mod vendor`'s vendor directory.
pub fn parse_build(
    workspace_root: &Path,
    external_repo_root: Option<&Path>,
    package: &PackageId,
) -> Result<(String, AstModule)> {
    let repo_root: std::path::PathBuf = if package.repo.is_main() {
        workspace_root.to_path_buf()
    } else {
        let ext = external_repo_root.ok_or_else(|| {
            anyhow!(
                "external package `{}` referenced but no external_repo_root configured",
                package
            )
        })?;
        ext.join(package.repo.as_str())
    };
    let pkg_dir = repo_root.join(package.path.as_str());
    let build_path = locate_build_file(&pkg_dir)
        .with_context(|| format!("locating BUILD file in {}", pkg_dir.display()))?;
    let src = std::fs::read_to_string(&build_path)
        .with_context(|| format!("reading {}", build_path.display()))?;
    let filename = build_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "BUILD".to_string());
    let ast = AstModule::parse(&filename, src, &Dialect::Standard)
        .map_err(|e| anyhow!("parsing {}: {e}", build_path.display()))?;
    Ok((filename, ast))
}

/// Run the BUILD evaluator with already-resolved loaded modules. Each
/// load statement's `module_id` must appear as a key in `loaded`,
/// paired with the canonical `Label` of the resolved `.bzl` (so the
/// recorded `UnconfiguredTarget`s can carry it for Phase 5 analysis).
///
/// `repo_mapping` is the active module's apparent → canonical repo
/// map, used by `Rule::invoke` to resolve label-typed attr strings.
pub fn evaluate(
    workspace_root: &Path,
    external_repo_root: Option<&Path>,
    package: PackageId,
    repo_mapping: BTreeMap<String, crate::label::CanonicalRepo>,
    ast: AstModule,
    load_stmts: &[LoadStmt],
    loaded: &HashMap<String, (Label, FrozenModule)>,
) -> Result<Package> {
    let repo_root: std::path::PathBuf = if package.repo.is_main() {
        workspace_root.to_path_buf()
    } else {
        let ext = external_repo_root.ok_or_else(|| {
            anyhow!(
                "external package `{}` referenced but no external_repo_root configured",
                package
            )
        })?;
        ext.join(package.repo.as_str())
    };
    let ctx = BuildContext::new(repo_root, package.clone(), repo_mapping);
    let globals = build_globals();

    Module::with_temp_heap(|module| -> Result<()> {
        // For each load statement, build a per-load wrap module whose
        // exported symbols are `Rule`s pointing at the underlying
        // `RuleClass` (or pass-through for non-rule symbols). The wrap
        // module is allocated in `module`'s heap and referenced via
        // `add_reference` so the underlying frozen heap stays alive.
        let mut wrap_modules: HashMap<&str, FrozenModule> = HashMap::new();
        // Owned storage for the freshly-frozen wrap modules (so the
        // ReturnFileLoader can borrow them).
        let mut owned_wraps: Vec<FrozenModule> = Vec::with_capacity(load_stmts.len());
        for stmt in load_stmts {
            let (bzl_label, frozen) = loaded.get(&stmt.module_id).ok_or_else(|| {
                anyhow!(
                    "internal: load `{}` was not pre-resolved by PackageKey",
                    stmt.module_id
                )
            })?;
            let wrap = build_wrap_module(&stmt.module_id, bzl_label, frozen, &stmt.symbols)?;
            owned_wraps.push(wrap);
        }
        for (stmt, wrap) in load_stmts.iter().zip(owned_wraps.iter()) {
            wrap_modules.insert(stmt.module_id.as_str(), wrap.dupe_frozen());
        }
        let loader_modules: HashMap<&str, &FrozenModule> =
            wrap_modules.iter().map(|(k, v)| (*k, v)).collect();
        let loader = ReturnFileLoader {
            modules: &loader_modules,
        };

        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&ctx);
        eval.set_loader(&loader);
        eval.eval_module(ast, &globals)
            .map_err(|e| anyhow!("evaluating BUILD for {}: {e}", package))?;
        Ok(())
    })?;

    Ok(Package {
        id: package,
        targets: ctx.into_targets(),
    })
}

fn build_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(starlark_types::register_for_build)
        .build()
}

/// Wrap each requested symbol from a loaded `.bzl` module so that
/// `RuleClass` values become `Rule(kind=local_name)` and other values
/// pass through unchanged. `bzl_label` is the canonical Label of the
/// resolved `.bzl` — recorded into each `Rule` for Phase 5 analysis to
/// recover the impl through `BzlLoadKey`.
fn build_wrap_module(
    module_id: &str,
    bzl_label: &Label,
    frozen: &FrozenModule,
    symbols: &[(String, String)],
) -> Result<FrozenModule> {
    Module::with_temp_heap(|module| -> Result<FrozenModule> {
        // Reference the source frozen heap so the wrapped FrozenValue
        // pointers remain valid after our own freeze.
        module.frozen_heap().add_reference(frozen.frozen_heap());
        for (local, source) in symbols {
            let value = frozen
                .get_option(source)
                .map_err(|e| anyhow!("loading `{source}` from `{module_id}`: {e}"))?
                .ok_or_else(|| anyhow!("`{module_id}` does not export symbol `{source}`"))?;
            // Safe: we held `add_reference(frozen.frozen_heap())` on
            // our wrap module's frozen heap above, so the FrozenValue
            // stays alive for as long as the wrap module does.
            let raw_frozen = unsafe { value.unchecked_frozen_value() };
            let frozen_as_value = raw_frozen.to_value();
            if crate::starlark_types::rule::RuleClass::from_value(frozen_as_value).is_some() {
                let wrapped = module.heap().alloc(Rule {
                    kind: local.clone(),
                    bzl_source: module_id.to_string(),
                    bzl_label: bzl_label.clone(),
                    source_symbol: source.clone(),
                    rule_class: raw_frozen,
                });
                module.set(local, wrapped);
            } else {
                module.set(local, frozen_as_value);
            }
        }
        module
            .freeze()
            .map_err(|e| anyhow!("freezing wrap module for `{module_id}`: {e:?}"))
    })
}

trait DupeFrozenModule {
    fn dupe_frozen(&self) -> FrozenModule;
}

impl DupeFrozenModule for FrozenModule {
    fn dupe_frozen(&self) -> FrozenModule {
        // FrozenModule clone is cheap (Arc/Ref bump).
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{CanonicalRepo, PackagePath};
    use std::collections::HashMap;

    fn empty_loaded() -> HashMap<String, (Label, FrozenModule)> {
        HashMap::new()
    }

    #[test]
    fn evaluates_a_build_file_with_no_loads() {
        // No load statements, no rule calls — just standard Starlark.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("BUILD.bazel"), "x = 1 + 1").unwrap();
        let pkg = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());
        let (_, ast) = parse_build(dir.path(), None, &pkg).unwrap();
        let package = evaluate(
            dir.path(),
            None,
            pkg,
            Default::default(),
            ast,
            &[],
            &empty_loaded(),
        )
        .unwrap();
        assert!(package.targets.is_empty());
    }
}
