//! `runfiles` — the runtime data fileset rules attach to executables.
//!
//! From DETAILS.md:
//! > Runfiles aren't just "the files in the runfiles tree" — they're a
//! > forest of (transitive_files, root_symlinks, symlinks) tuples. Bazel
//! > merges them by key with a precedence order. Get the data shape
//! > right; the layout-on-disk happens at execution, which is out of
//! > scope.
//!
//! Phase 5 ships the data shape: a [`Runfiles`] value carrying
//! `transitive_files` (a depset of `File`) plus `symlinks` and
//! `root_symlinks` maps. The `merge`/`merge_all` Bazel-side helpers
//! land when a ruleset surfaces them — for now `Runfiles::merge`
//! (Rust-side) produces a deterministic fold.

use std::collections::BTreeMap;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::AllocDict;
use starlark::values::list::UnpackList;
use starlark::values::{NoSerialize, StarlarkValue, starlark_value};

use crate::starlark_types::file::File;

/// `Runfiles` value — what `ctx.runfiles(...)` returns and what rules
/// stash on `DefaultInfo.default_runfiles` / `data_runfiles`.
///
/// Loupe v1 keeps everything as flat ordered lists of `File`. Bazel
/// uses a `depset(File)` for `transitive_files`; the lazy-tree
/// flavor isn't required for correctness and we skip it until a
/// ruleset's runfiles get big enough to matter.
#[derive(
    Debug, Clone, PartialEq, Eq, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display,
)]
#[display(
    "<runfiles {} files, {} symlinks, {} root_symlinks>",
    files.len(),
    symlinks.len(),
    root_symlinks.len()
)]
pub struct Runfiles {
    /// Files that should appear in the runfiles tree at their
    /// natural workspace-relative path. Order is the merge-fold order;
    /// duplicates are kept (Bazel runtime dedupes by path).
    pub files: Vec<File>,
    /// `path-in-runfiles` → `File`. Bazel's `runfiles.symlinks`.
    /// A `BTreeMap` for stable iteration.
    pub symlinks: BTreeMap<String, File>,
    /// Like `symlinks` but the path is rooted at the runfiles tree
    /// itself, not at the workspace. Bazel's `runfiles.root_symlinks`.
    pub root_symlinks: BTreeMap<String, File>,
}

impl Runfiles {
    pub fn empty() -> Self {
        Runfiles {
            files: Vec::new(),
            symlinks: BTreeMap::new(),
            root_symlinks: BTreeMap::new(),
        }
    }

    /// Merge `other` into `self`. Files concatenate; symlink and
    /// root_symlink maps prefer the existing entry (matching Bazel's
    /// "first-wins" merge for runfiles).
    pub fn merge(mut self, other: Runfiles) -> Self {
        self.files.extend(other.files);
        for (k, v) in other.symlinks {
            self.symlinks.entry(k).or_insert(v);
        }
        for (k, v) in other.root_symlinks {
            self.root_symlinks.entry(k).or_insert(v);
        }
        self
    }
}

starlark_simple_value!(Runfiles);

#[starlark_value(type = "runfiles")]
impl<'v> StarlarkValue<'v> for Runfiles {
    fn get_attr(
        &self,
        attribute: &str,
        heap: starlark::values::Heap<'v>,
    ) -> Option<starlark::values::Value<'v>> {
        match attribute {
            "files" => {
                // STUB: should be a `depset(File)`. v1 surfaces a flat
                // list — rulesets that introspect the result type
                // (rare) will see the difference.
                let files: Vec<starlark::values::Value<'v>> =
                    self.files.iter().map(|f| heap.alloc(f.clone())).collect();
                Some(heap.alloc(files))
            }
            "symlinks" => {
                let entries: SmallMap<&str, starlark::values::Value<'v>> = self
                    .symlinks
                    .iter()
                    .map(|(k, v)| (k.as_str(), heap.alloc(v.clone())))
                    .collect();
                Some(heap.alloc(AllocDict(entries)))
            }
            "root_symlinks" => {
                let entries: SmallMap<&str, starlark::values::Value<'v>> = self
                    .root_symlinks
                    .iter()
                    .map(|(k, v)| (k.as_str(), heap.alloc(v.clone())))
                    .collect();
                Some(heap.alloc(AllocDict(entries)))
            }
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["files", "symlinks", "root_symlinks"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(runfiles_methods)
    }
}

#[starlark_module]
fn runfiles_methods(builder: &mut MethodsBuilder) {
    /// `runfiles.merge(other)` — Bazel's runfiles merger. Returns a
    /// new `runfiles` combining `self` and `other` with first-wins
    /// precedence on symlink-key collisions.
    fn merge<'v>(
        #[starlark(this)] this: &Runfiles,
        #[starlark(require = pos)] other: &Runfiles,
    ) -> anyhow::Result<Runfiles> {
        Ok(this.clone().merge(other.clone()))
    }

    /// `runfiles.merge_all(others)` — fold-merge a list of runfiles
    /// in declaration order. Bazel's signature.
    fn merge_all<'v>(
        #[starlark(this)] this: &Runfiles,
        #[starlark(require = pos)] others: UnpackList<&'v Runfiles>,
    ) -> anyhow::Result<Runfiles> {
        let mut acc = this.clone();
        for r in others.items {
            acc = acc.merge(r.clone());
        }
        Ok(acc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{CanonicalRepo, PackageId, PackagePath};

    fn pkg() -> PackageId {
        PackageId::new(CanonicalRepo::MAIN, PackagePath::root())
    }

    fn file(name: &str) -> File {
        File::source(pkg(), name)
    }

    #[test]
    fn empty_runfiles_is_empty_in_all_three_collections() {
        let r = Runfiles::empty();
        assert!(r.files.is_empty());
        assert!(r.symlinks.is_empty());
        assert!(r.root_symlinks.is_empty());
    }

    #[test]
    fn merge_concatenates_files_in_order() {
        let a = Runfiles {
            files: vec![file("a.txt")],
            ..Runfiles::empty()
        };
        let b = Runfiles {
            files: vec![file("b.txt")],
            ..Runfiles::empty()
        };
        let merged = a.merge(b);
        let names: Vec<&str> = merged.files.iter().map(|f| f.basename()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn merge_symlinks_first_wins() {
        // Bazel: runfiles symlink merges keep the existing entry on
        // collision. Confirm the same fold here so users testing
        // against Bazel see the same precedence.
        let mut a_syms = BTreeMap::new();
        a_syms.insert("foo".into(), file("a-foo"));
        let a = Runfiles {
            symlinks: a_syms,
            ..Runfiles::empty()
        };
        let mut b_syms = BTreeMap::new();
        b_syms.insert("foo".into(), file("b-foo"));
        let b = Runfiles {
            symlinks: b_syms,
            ..Runfiles::empty()
        };
        let merged = a.merge(b);
        assert_eq!(
            merged.symlinks.get("foo").unwrap().basename(),
            "a-foo",
            "first-wins on symlink collision"
        );
    }
}
