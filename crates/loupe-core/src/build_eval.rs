//! BUILD-file evaluator.
//!
//! Phase 2 scope per `PLAN.md`: "minimal Starlark environment (no
//! `rule()` yet, no `load()` yet)". The fixture's BUILD file *does*
//! use `load(":defs.bzl", "hello")`, so `load()` is wired but the
//! loaded modules are stubs — every imported symbol resolves to a
//! `PlaceholderRule` that records its call into the package's
//! unconfigured-target map. Phase 3 makes `load()` real (real
//! `BzlLoadKey`); Phase 5 introduces actual `rule()`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use allocative::Allocative;
use anyhow::{Context, Result, anyhow};
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Arguments, Evaluator, ReturnFileLoader};
use starlark::starlark_simple_value;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::{NoSerialize, StarlarkValue, StringValue, Value, starlark_value};

use crate::label::{Label, PackageId};
use crate::package::{AttrValue, Package, UnconfiguredTarget};

/// Stored in `Evaluator::extra` while a BUILD file evaluates.
/// Placeholder rule calls record themselves through this.
#[derive(Debug, ProvidesStaticType)]
struct BuildContext {
    package: PackageId,
    targets: RefCell<BTreeMap<String, UnconfiguredTarget>>,
}

/// A Starlark callable bound to a loaded symbol. When called from a
/// BUILD file it records `(kind, kwargs)` into the active
/// `BuildContext` rather than performing real analysis.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<loupe placeholder rule {kind} from {bzl_source}>")]
struct PlaceholderRule {
    kind: String,
    bzl_source: String,
}

starlark_simple_value!(PlaceholderRule);

#[starlark_value(type = "loupe_placeholder_rule")]
impl<'v> StarlarkValue<'v> for PlaceholderRule {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let ctx: &BuildContext = eval
            .extra
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow!(
                    "internal: placeholder rule called without a BuildContext"
                ))
            })?
            .downcast_ref::<BuildContext>()
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow!(
                    "internal: BUILD evaluator extra is not a BuildContext"
                ))
            })?;

        let named = args
            .names_map()
            .map_err(|e| starlark::Error::new_other(anyhow!("bad kwargs: {e}")))?;
        let attrs = unpack_named_args(&named);
        let name = match attrs.get("name") {
            Some(AttrValue::String(s)) => s.clone(),
            _ => {
                return Err(starlark::Error::new_other(anyhow!(
                    "rule call `{}` is missing required `name = ...`",
                    self.kind
                )));
            }
        };

        let label = Label::new(
            ctx.package.repo.clone(),
            ctx.package.path.clone(),
            name.clone(),
        );
        let mut targets = ctx.targets.borrow_mut();
        if targets.contains_key(&name) {
            return Err(starlark::Error::new_other(anyhow!(
                "duplicate target name `{}` in package {}",
                name,
                ctx.package
            )));
        }
        targets.insert(
            name,
            UnconfiguredTarget {
                label,
                kind: self.kind.clone(),
                bzl_source: self.bzl_source.clone(),
                attrs,
            },
        );
        Ok(Value::new_none())
    }
}

fn unpack_named_args<'v>(
    named: &SmallMap<StringValue<'v>, Value<'v>>,
) -> BTreeMap<String, AttrValue> {
    let mut out = BTreeMap::new();
    for (k, v) in named.iter() {
        out.insert(k.as_str().to_string(), unpack_attr(*v));
    }
    out
}

fn unpack_attr<'v>(v: Value<'v>) -> AttrValue {
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
    AttrValue::Opaque(v.to_string())
}

/// Build a stub `FrozenModule` whose globals are
/// `PlaceholderRule { kind, bzl_source: path }` for each requested
/// symbol. Used for every `load()` statement in Phase 2.
fn stub_loaded_module(path: &str, symbols: &[&str]) -> Result<FrozenModule> {
    Module::with_temp_heap(|module| -> Result<FrozenModule> {
        for &sym in symbols {
            let v = module.heap().alloc(PlaceholderRule {
                kind: sym.to_string(),
                bzl_source: path.to_string(),
            });
            module.set(sym, v);
        }
        module
            .freeze()
            .map_err(|e| anyhow!("freezing stub module {path}: {e:?}"))
    })
}

/// Evaluate the BUILD file for `package`. `workspace_root` is the
/// directory containing `MODULE.bazel`. Returns the resulting
/// `Package` with all captured `UnconfiguredTarget`s.
pub fn evaluate(workspace_root: &Path, package: PackageId) -> Result<Package> {
    let pkg_dir = workspace_root.join(package.path.as_str());
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

    let load_stmts: Vec<(String, Vec<String>)> = ast
        .loads()
        .into_iter()
        .map(|l| {
            (
                l.module_id.to_string(),
                l.symbols.values().map(|s| (*s).to_string()).collect(),
            )
        })
        .collect();
    let mut stub_modules: BTreeMap<String, FrozenModule> = BTreeMap::new();
    for (path, syms) in &load_stmts {
        if stub_modules.contains_key(path) {
            continue;
        }
        let sym_refs: Vec<&str> = syms.iter().map(|s| s.as_str()).collect();
        stub_modules.insert(path.clone(), stub_loaded_module(path, &sym_refs)?);
    }

    let ctx = BuildContext {
        package: package.clone(),
        targets: RefCell::new(BTreeMap::new()),
    };
    let globals = Globals::standard();
    let loader_modules: std::collections::HashMap<&str, &FrozenModule> =
        stub_modules.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let loader = ReturnFileLoader {
        modules: &loader_modules,
    };
    Module::with_temp_heap(|module| -> Result<()> {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&ctx);
        eval.set_loader(&loader);
        eval.eval_module(ast, &globals)
            .map_err(|e| anyhow!("evaluating {}: {e}", build_path.display()))?;
        Ok(())
    })?;

    Ok(Package {
        id: package,
        targets: ctx.targets.into_inner(),
    })
}

fn locate_build_file(pkg_dir: &Path) -> Result<std::path::PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{CanonicalRepo, PackagePath};
    use std::fs;

    fn write_fixture(dir: &Path, build: &str) {
        fs::write(
            dir.join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(dir.join("BUILD.bazel"), build).unwrap();
    }

    #[test]
    fn captures_a_loaded_rule_call() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(
            dir.path(),
            r#"
load(":defs.bzl", "hello")
hello(name = "world", greeting = "hi")
"#,
        );
        let pkg = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());
        let package = evaluate(dir.path(), pkg).unwrap();
        assert_eq!(package.targets.len(), 1);
        let t = &package.targets["world"];
        assert_eq!(t.label.to_string(), "@@//:world");
        assert_eq!(t.kind, "hello");
        assert_eq!(t.bzl_source, ":defs.bzl");
        assert_eq!(
            t.attrs.get("greeting"),
            Some(&AttrValue::String("hi".to_string()))
        );
    }

    #[test]
    fn rejects_rule_call_without_name() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(
            dir.path(),
            r#"
load(":defs.bzl", "hello")
hello(greeting = "no name")
"#,
        );
        let pkg = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());
        let err = evaluate(dir.path(), pkg).unwrap_err();
        assert!(err.to_string().contains("missing required `name"));
    }

    #[test]
    fn rejects_duplicate_target_names() {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(
            dir.path(),
            r#"
load(":defs.bzl", "hello")
hello(name = "x")
hello(name = "x")
"#,
        );
        let pkg = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());
        let err = evaluate(dir.path(), pkg).unwrap_err();
        assert!(err.to_string().contains("duplicate target name"));
    }
}
