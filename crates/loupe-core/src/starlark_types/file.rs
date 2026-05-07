//! `File` — source and generated artifacts.
//!
//! From DETAILS.md:
//! > Bazel's outputs land under `bazel-out/<configuration>/bin/<package>/<file>`
//! > (and `bazel-out/<configuration>/genfiles/...` historically, mostly
//! > merged with `bin/` now). The `<configuration>` segment is a
//! > hash-derived string like `k8-fastbuild` or `darwin_arm64-opt`.
//! > Match Bazel's paths exactly — `bazel info output_path` on the
//! > fixture tells you what to use.
//!
//! From DESIGN.md:
//! > Use Bazel's own conventions throughout. Paths are
//! > `bazel-out/<config>/bin/<package>/<file>` with the config segment
//! > matching what Bazel produces.
//!
//! ## Phase 4 model
//!
//! - Source files live at `<workspace>/<package>/<short_path>`. Their
//!   `path` (Bazel's `File.path`) and `short_path` are equal — both
//!   relative to the workspace root.
//! - Generated artifacts live under `bazel-out/<config>/bin/...`. Their
//!   `path` includes the `bazel-out/...` prefix; `short_path` strips it
//!   so consumers can reason about logical layout.
//! - The `<config>` segment is supplied at construction time. v1 uses
//!   a single configuration so this is degenerate, but the field is
//!   plumbed because DESIGN.md mandates it.

use std::fmt;
use std::hash::Hash;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::collections::StarlarkHasher;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, StarlarkValue, ValueLike, starlark_value};

use crate::label::PackageId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Allocative)]
pub enum FileKind {
    Source,
    /// `bazel-out/<config>/bin/...`. The historical `genfiles` tree is
    /// merged into `bin/` in modern Bazel; we don't model it
    /// separately.
    GeneratedBin,
    /// `bazel-out/<config>/bin/...` but the path names a tree
    /// artifact (`ctx.actions.declare_directory`). v1 doesn't model
    /// the contents — we record the declaration and consumers see
    /// `is_directory == True`. Path layout is identical to
    /// [`FileKind::GeneratedBin`]; only the flag differs.
    GeneratedBinDir,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Allocative, NoSerialize, ProvidesStaticType)]
pub struct File {
    kind: FileKind,
    package: PackageId,
    /// File path relative to the package, e.g. `"src/foo.cc"`.
    short: String,
    /// For [`FileKind::GeneratedBin`], the `<config>` segment in
    /// `bazel-out/<config>/bin/...`. Ignored for source files.
    config_segment: String,
}

impl File {
    pub fn source(package: PackageId, short: impl Into<String>) -> Self {
        File {
            kind: FileKind::Source,
            package,
            short: short.into(),
            config_segment: String::new(),
        }
    }

    pub fn generated_bin(
        package: PackageId,
        short: impl Into<String>,
        config_segment: impl Into<String>,
    ) -> Self {
        File {
            kind: FileKind::GeneratedBin,
            package,
            short: short.into(),
            config_segment: config_segment.into(),
        }
    }

    /// Tree artifact — `ctx.actions.declare_directory`. Same path
    /// layout as a generated file; the kind flag is what distinguishes
    /// it. STUB: v1 records the declaration but doesn't model the
    /// directory contents.
    pub fn generated_bin_dir(
        package: PackageId,
        short: impl Into<String>,
        config_segment: impl Into<String>,
    ) -> Self {
        File {
            kind: FileKind::GeneratedBinDir,
            package,
            short: short.into(),
            config_segment: config_segment.into(),
        }
    }

    pub fn kind(&self) -> FileKind {
        self.kind
    }

    pub fn is_source(&self) -> bool {
        matches!(self.kind, FileKind::Source)
    }

    /// Bazel's `File.is_directory`. True for tree artifacts declared
    /// via `ctx.actions.declare_directory`; false for everything else.
    pub fn is_directory(&self) -> bool {
        matches!(self.kind, FileKind::GeneratedBinDir)
    }

    pub fn package(&self) -> &PackageId {
        &self.package
    }

    /// `short_path` mirrors Bazel: relative to the workspace, with the
    /// package prefix included. For both source and generated files,
    /// it's `<package>/<short>` (no `bazel-out` prefix).
    pub fn short_path(&self) -> String {
        if self.package.path.is_root() {
            self.short.clone()
        } else {
            format!("{}/{}", self.package.path.as_str(), self.short)
        }
    }

    /// `path` mirrors Bazel: for source files, equals `short_path()`;
    /// for generated files (regular or tree), prepends
    /// `bazel-out/<config>/bin/`.
    pub fn path(&self) -> String {
        match self.kind {
            FileKind::Source => self.short_path(),
            FileKind::GeneratedBin | FileKind::GeneratedBinDir => format!(
                "bazel-out/{}/bin/{}",
                self.config_segment,
                self.short_path()
            ),
        }
    }

    /// Directory portion of [`path`] — everything before the final
    /// `/`. Empty when the file is at the workspace root.
    pub fn dirname(&self) -> String {
        let p = self.path();
        match p.rsplit_once('/') {
            Some((dir, _)) => dir.to_string(),
            None => String::new(),
        }
    }

    /// Filename component (everything after the final `/`).
    pub fn basename(&self) -> &str {
        match self.short.rsplit_once('/') {
            Some((_, name)) => name,
            None => &self.short,
        }
    }

    /// File extension (everything after the final `.` in the basename),
    /// or `""` if there is none.
    pub fn extension(&self) -> &str {
        match self.basename().rsplit_once('.') {
            Some((_, ext)) => ext,
            None => "",
        }
    }
}

impl fmt::Display for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<File {}>", self.path())
    }
}

starlark_simple_value!(File);

#[starlark_value(type = "File")]
impl<'v> StarlarkValue<'v> for File {
    /// Bazel `File` exposes `basename`, `extension`, `path`,
    /// `short_path`, `dirname`, `is_source`, `is_directory`, plus a
    /// few internal-ish things (`owner`, `root`) that v1 doesn't
    /// surface yet.
    fn get_attr(
        &self,
        attribute: &str,
        heap: starlark::values::Heap<'v>,
    ) -> Option<starlark::values::Value<'v>> {
        match attribute {
            "basename" => Some(heap.alloc(self.basename())),
            "extension" => Some(heap.alloc(self.extension())),
            "path" => Some(heap.alloc(self.path())),
            "short_path" => Some(heap.alloc(self.short_path())),
            "dirname" => Some(heap.alloc(self.dirname())),
            "is_source" => Some(starlark::values::Value::new_bool(self.is_source())),
            "is_directory" => Some(starlark::values::Value::new_bool(self.is_directory())),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        [
            "basename",
            "extension",
            "path",
            "short_path",
            "dirname",
            "is_source",
            "is_directory",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Two files compare equal in Starlark when they're equal in Rust
    /// (same kind / package / short / config segment). Mirrors
    /// `derive(PartialEq)` so `depset(File)` dedupe behaves as
    /// rulesets expect.
    fn equals(&self, other: starlark::values::Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<File>()
            .map(|o| self == o)
            .unwrap_or(false))
    }

    /// Hash by the components that drive equality. Required for
    /// `Hashed<Value>` dedupe inside `depset.to_list()`.
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        Hash::hash(self, hasher);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{CanonicalRepo, PackagePath};

    fn pkg(path: &str) -> PackageId {
        PackageId::new(CanonicalRepo::MAIN, PackagePath::new(path).unwrap())
    }

    #[test]
    fn source_file_in_root_package_has_bare_short_path() {
        let f = File::source(pkg(""), "foo.txt");
        assert!(f.is_source());
        assert_eq!(f.short_path(), "foo.txt");
        assert_eq!(f.path(), "foo.txt");
        assert_eq!(f.basename(), "foo.txt");
        assert_eq!(f.extension(), "txt");
    }

    #[test]
    fn source_file_in_nested_package_prepends_package_path() {
        let f = File::source(pkg("src/lib"), "main.cc");
        assert_eq!(f.short_path(), "src/lib/main.cc");
        assert_eq!(f.path(), "src/lib/main.cc");
        assert_eq!(f.basename(), "main.cc");
        assert_eq!(f.extension(), "cc");
    }

    #[test]
    fn generated_file_uses_bazel_out_bin_prefix() {
        // Bazel: `bazel-out/<config>/bin/<package>/<file>` — DETAILS.md
        // is explicit about matching the layout.
        let f = File::generated_bin(pkg("src/lib"), "out.o", "k8-fastbuild");
        assert_eq!(f.short_path(), "src/lib/out.o");
        assert_eq!(f.path(), "bazel-out/k8-fastbuild/bin/src/lib/out.o");
        assert!(!f.is_source());
    }

    #[test]
    fn generated_file_in_root_package_has_no_extra_slash() {
        let f = File::generated_bin(pkg(""), "out.o", "darwin_arm64-opt");
        assert_eq!(f.path(), "bazel-out/darwin_arm64-opt/bin/out.o");
    }

    #[test]
    fn extension_is_empty_for_files_without_dots() {
        let f = File::source(pkg(""), "Makefile");
        assert_eq!(f.extension(), "");
        assert_eq!(f.basename(), "Makefile");
    }

    #[test]
    fn extension_only_uses_last_dot() {
        let f = File::source(pkg(""), "archive.tar.gz");
        assert_eq!(f.extension(), "gz");
    }

    #[test]
    fn equality_includes_kind_package_and_short() {
        let a = File::source(pkg("foo"), "x");
        let b = File::source(pkg("foo"), "x");
        let c = File::source(pkg("bar"), "x");
        let d = File::generated_bin(pkg("foo"), "x", "k8-fastbuild");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d, "source vs generated differ even with same path");
    }
}
