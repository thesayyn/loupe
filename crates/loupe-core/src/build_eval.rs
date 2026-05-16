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

use loupe_labels::{Label, PackageId};
use crate::package::{AttrValue, Package, UnconfiguredTarget};
use crate::repo_root::RepoRoot;
use crate::starlark_types::{self, rule::Rule};

/// Stored in `Evaluator::extra` while a BUILD file evaluates. Loaded
/// rule wrappers (`Rule`) record `UnconfiguredTarget`s through
/// `record_target`; native helpers like `glob()` reach package files
/// through [`Self::repo_root`] when external and `workspace_root` for
/// the main repo. `repo_mapping` lets `Rule::invoke` resolve label-
/// typed attr strings into canonical `Label`s without re-parsing them.
#[derive(Debug, ProvidesStaticType)]
pub struct BuildContext {
    /// Source of the package's files. `None` for main-repo packages —
    /// those read relative to `workspace_root`. `Some` for external
    /// packages (filesystem or embedded).
    repo_root: Option<RepoRoot>,
    workspace_root: std::path::PathBuf,
    package: PackageId,
    repo_mapping: BTreeMap<String, loupe_labels::CanonicalRepo>,
    targets: RefCell<BTreeMap<String, UnconfiguredTarget>>,
}

impl BuildContext {
    fn new(
        repo_root: Option<RepoRoot>,
        workspace_root: std::path::PathBuf,
        package: PackageId,
        repo_mapping: BTreeMap<String, loupe_labels::CanonicalRepo>,
    ) -> Self {
        BuildContext {
            repo_root,
            workspace_root,
            package,
            repo_mapping,
            targets: RefCell::new(BTreeMap::new()),
        }
    }

    /// External-repo root, if this package is external. `None` for
    /// main-repo packages.
    pub fn repo_root(&self) -> Option<&RepoRoot> {
        self.repo_root.as_ref()
    }

    /// Workspace root path. Always set; used by package-local IO
    /// (`glob()`) when [`Self::repo_root`] is `None`.
    pub fn workspace_root(&self) -> &std::path::Path {
        &self.workspace_root
    }

    pub fn package(&self) -> &PackageId {
        &self.package
    }

    pub fn repo_mapping(&self) -> &BTreeMap<String, loupe_labels::CanonicalRepo> {
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
/// ast)`.
///
/// `repo_root` is the [`RepoRoot`] the caller resolved via
/// `HasExternalRepoRoots::external_repo_root_for`. For main-repo
/// packages it's `None` — we read from `workspace_root` directly.
pub fn parse_build(
    workspace_root: &Path,
    repo_root: Option<&RepoRoot>,
    package: &PackageId,
) -> Result<(String, AstModule)> {
    let (filename, src, origin) = if package.repo.is_main() {
        let pkg_dir = workspace_root.join(package.path.as_str());
        let build_path = locate_build_file(&pkg_dir)
            .with_context(|| format!("locating BUILD file in {}", pkg_dir.display()))?;
        let src = std::fs::read_to_string(&build_path)
            .with_context(|| format!("reading {}", build_path.display()))?;
        let filename = build_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "BUILD".to_string());
        (filename, src, build_path.display().to_string())
    } else {
        let root = repo_root.ok_or_else(|| {
            anyhow!(
                "external package `{}` referenced but no `--external-repo-root` \
                 contains it",
                package
            )
        })?;
        let canonical = package.repo.as_str();
        let (filename, bytes) = read_build_via_root(root, canonical, package.path.as_str())
            .ok_or_else(|| {
                anyhow!(
                    "no BUILD or BUILD.bazel under `{}//{}`",
                    canonical,
                    package.path.as_str()
                )
            })?;
        let src = String::from_utf8(bytes)
            .with_context(|| format!("{canonical}//{} is not UTF-8", package.path.as_str()))?;
        let origin = format!("{canonical}//{}:{filename}", package.path.as_str());
        (filename, src, origin)
    };
    let ast = AstModule::parse(&filename, src, &Dialect::Standard)
        .map_err(|e| anyhow!("parsing {origin}: {e}"))?;
    Ok((filename, ast))
}

/// Probe `BUILD.bazel` then `BUILD` under `<canonical>/<pkg_path>` via
/// a [`RepoRoot`]. Returns `(filename, bytes)`.
fn read_build_via_root(
    root: &RepoRoot,
    canonical: &str,
    pkg_path: &str,
) -> Option<(String, Vec<u8>)> {
    for name in ["BUILD.bazel", "BUILD"] {
        let rel = if pkg_path.is_empty() {
            std::path::PathBuf::from(name)
        } else {
            Path::new(pkg_path).join(name)
        };
        if let Some(bytes) = root.read(canonical, &rel) {
            return Some((name.to_string(), bytes));
        }
    }
    None
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
    repo_root: Option<RepoRoot>,
    package: PackageId,
    repo_mapping: BTreeMap<String, loupe_labels::CanonicalRepo>,
    ast: AstModule,
    load_stmts: &[LoadStmt],
    loaded: &HashMap<String, (Label, FrozenModule)>,
) -> Result<Package> {
    // `repo_root == None` for synthesised packages without a backing
    // tree (`@@_loupe_platforms//:` — see `compute_platforms_expr_package`).
    // Filesystem-backed external packages produce a real root via
    // `HasExternalRepoRoots::external_repo_root_for`; the caller is
    // responsible for surfacing the missing-root error before getting here.
    let ctx = BuildContext::new(
        repo_root,
        workspace_root.to_path_buf(),
        package.clone(),
        repo_mapping,
    );
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
        // Bazel-compat surface authored in user-space Starlark
        // (see `crate::builtins`). Inject into BUILD eval too so
        // BUILD-side macros that reach for `coverage_common` /
        // `InstrumentedFilesInfo` resolve.
        crate::builtins::inject_builtins_into(&module)?;

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
            // Bind under the SOURCE name. Starlark's `load(...)`
            // implementation looks up `source` (i.e. the right-hand
            // side of the `local = "source"` form) in the loaded
            // module and then assigns to `local` in the consumer's
            // module — the consumer-side rename is its concern, not
            // ours. Setting under `local` here breaks renamed loads
            // (`load(":x.bzl", new_name = "old_name")`) because the
            // wrap module ends up with `new_name` and Starlark asks
            // for `old_name`.
            if crate::starlark_types::rule::RuleClass::from_value(frozen_as_value).is_some() {
                let wrapped = module.heap().alloc(Rule {
                    kind: local.clone(),
                    bzl_source: module_id.to_string(),
                    bzl_label: bzl_label.clone(),
                    source_symbol: source.clone(),
                    rule_class: raw_frozen,
                });
                module.set(source, wrapped);
            } else {
                module.set(source, frozen_as_value);
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
    use loupe_labels::{CanonicalRepo, PackagePath};
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
