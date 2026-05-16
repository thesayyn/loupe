//! `exports_files()` and source-target resolution.
//!
//! `exports_files(["foo.txt", "bar.txt"], visibility = [...])` declares
//! the listed source files as targets in the current package, making
//! them referenceable from other packages by label
//! (`@@//pkg:foo.txt`). The `visibility` kwarg is parsed but not
//! enforced — DESIGN.md: "everything public" in v1.
//!
//! Each exported file becomes an `UnconfiguredTarget` with
//! `kind = "source_file"`. Phase 5 turns these into resolvable `File`
//! values when a rule attr references them by label.

use anyhow::{Result, anyhow};
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;
use starlark::values::none::NoneType;

use std::collections::BTreeMap;

use loupe_labels::Label;
use crate::package::{AttrValue, UnconfiguredTarget};

/// Synthetic rule-kind for source files declared via `exports_files`.
/// Bazel uses the same string in cquery output for source-file targets.
pub const SOURCE_FILE_KIND: &str = "source_file";

#[starlark_module]
pub fn exports_globals(b: &mut GlobalsBuilder) {
    /// `exports_files(srcs, visibility = ..., licenses = ...)`. Records
    /// each entry in `srcs` as a `source_file` target in the current
    /// package.
    fn exports_files<'v>(
        #[starlark(require = pos)] srcs: Value<'v>,
        #[starlark(require = named)] visibility: Option<Value<'v>>,
        #[starlark(require = named)] licenses: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let ctx: &crate::build_eval::BuildContext = eval
            .extra
            .ok_or_else(|| anyhow!("internal: exports_files outside a BuildContext"))?
            .downcast_ref::<crate::build_eval::BuildContext>()
            .ok_or_else(|| anyhow!("internal: extra is not a BuildContext"))?;

        let names = unpack_string_list(srcs)?;
        // Recorded but not enforced. Phase 4+ visibility lands later.
        let visibility_attr = visibility
            .map(|v| crate::build_eval::unpack_attr(v))
            .unwrap_or(AttrValue::None);
        let licenses_attr = licenses
            .map(|v| crate::build_eval::unpack_attr(v))
            .unwrap_or(AttrValue::None);

        for short in names {
            let label = Label::new(
                ctx.package().repo.clone(),
                ctx.package().path.clone(),
                short.clone(),
            );
            let mut attrs: BTreeMap<String, AttrValue> = BTreeMap::new();
            attrs.insert("name".into(), AttrValue::String(short));
            attrs.insert("visibility".into(), visibility_attr.clone());
            attrs.insert("licenses".into(), licenses_attr.clone());
            ctx.record_target(UnconfiguredTarget {
                label,
                kind: SOURCE_FILE_KIND.to_string(),
                bzl_source: "<native>".to_string(),
                bzl_label: None,
                source_symbol: None,
                attrs,
            })?;
        }
        Ok(NoneType)
    }
}

fn unpack_string_list<'v>(v: Value<'v>) -> Result<Vec<String>> {
    let list = starlark::values::list::ListRef::from_value(v)
        .ok_or_else(|| anyhow!("exports_files() expects a list, got {}", v.get_type()))?;
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        let s = item
            .unpack_str()
            .ok_or_else(|| anyhow!("exports_files() entries must be strings"))?
            .to_string();
        out.push(s);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_labels::{CanonicalRepo, PackageId, PackagePath};
    use std::collections::HashMap;
    use std::fs;

    fn pkg() -> PackageId {
        PackageId::new(CanonicalRepo::MAIN, PackagePath::root())
    }

    /// Drive the BUILD evaluator end-to-end so we exercise the
    /// `Evaluator::extra → BuildContext` plumbing along with the
    /// builtin itself.
    fn evaluate_build(build_src: &str) -> crate::package::Package {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(dir.path().join("BUILD.bazel"), build_src).unwrap();
        let (_, ast) = crate::build_eval::parse_build(dir.path(), None, &pkg()).unwrap();
        let load_stmts = crate::build_eval::extract_loads(&ast);
        let loaded: HashMap<String, (loupe_labels::Label, starlark::environment::FrozenModule)> =
            HashMap::new();
        crate::build_eval::evaluate(
            dir.path(),
            None,
            pkg(),
            Default::default(),
            ast,
            &load_stmts,
            &loaded,
        )
        .unwrap()
    }

    #[test]
    fn exports_files_records_one_target_per_entry() {
        let pkg = evaluate_build(r#"exports_files(["a.txt", "b.txt"])"#);
        let names: Vec<&String> = pkg.targets.keys().collect();
        assert_eq!(names, vec![&"a.txt".to_string(), &"b.txt".to_string()]);
        for t in pkg.targets.values() {
            assert_eq!(t.kind, SOURCE_FILE_KIND);
            assert_eq!(t.bzl_source, "<native>");
        }
    }

    #[test]
    fn exports_files_records_canonical_label_per_entry() {
        let pkg = evaluate_build(r#"exports_files(["foo.txt"])"#);
        let t = pkg.targets.get("foo.txt").unwrap();
        assert_eq!(t.label.to_string(), "@@//:foo.txt");
    }

    #[test]
    fn exports_files_visibility_kwarg_is_recorded_not_enforced() {
        let pkg =
            evaluate_build(r#"exports_files(["foo.txt"], visibility = ["//visibility:public"])"#);
        let t = pkg.targets.get("foo.txt").unwrap();
        match t.attrs.get("visibility") {
            Some(AttrValue::List(items)) => {
                assert_eq!(items.len(), 1);
                match &items[0] {
                    AttrValue::String(s) => assert_eq!(s, "//visibility:public"),
                    other => panic!("expected string, got {other:?}"),
                }
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_exports_files_entries_collide_with_existing_targets() {
        // Bazel rejects two declarations of the same target in a
        // package; we surface the same kind of error via the
        // BuildContext duplicate check.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("BUILD.bazel"),
            r#"
exports_files(["a.txt"])
exports_files(["a.txt"])
"#,
        )
        .unwrap();
        let (_, ast) = crate::build_eval::parse_build(dir.path(), None, &pkg()).unwrap();
        let load_stmts = crate::build_eval::extract_loads(&ast);
        let loaded: HashMap<String, (loupe_labels::Label, starlark::environment::FrozenModule)> =
            HashMap::new();
        let err = crate::build_eval::evaluate(
            dir.path(),
            None,
            pkg(),
            Default::default(),
            ast,
            &load_stmts,
            &loaded,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate target name"));
    }
}
