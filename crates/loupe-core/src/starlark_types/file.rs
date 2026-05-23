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

use loupe_labels::PackageId;

pub use crate::analysis::RepositoryLayout;

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
    /// Build-status File — `bazel-out/volatile-status.txt` or
    /// `bazel-out/stable-status.txt`. Emitted by Bazel from
    /// `--workspace_status_command`. Sits at the `bazel-out/` root
    /// (no config segment, no `bin/`). Loupe doesn't run the
    /// status command, so the File serves as a path-only handle —
    /// `ctx.version_file.path` reads the canonical path. Contents are
    /// outside the analysis graph and only matter when Loupe grows
    /// an execution graph.
    BuildStatus,
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
    /// Execroot layout for external-repo files. Pinned at construction
    /// time from the active [`crate::analysis::Configuration`]. Only
    /// observable for external-repo files; main-repo paths are
    /// layout-independent. New constructors default to
    /// `RepositoryLayout::Default` (Bazel's
    /// `--noexperimental_sibling_repository_layout` default); call
    /// [`File::with_layout`] when the active config asks for sibling
    /// layout.
    repository_layout: RepositoryLayout,
    /// REAPI `FileNode.is_executable`. Flips a single varint in the
    /// encoded protobuf and propagates up the merkle tree; any
    /// toolchain binary with `+x` on disk has the bit set in Bazel's
    /// own AC-key computation. Wire from the explicit knobs rule
    /// authors set (`ctx.actions.write(..., is_executable=True)`,
    /// `ctx.actions.expand_template(..., is_executable=True)`).
    ///
    /// STUB: source-file POSIX `+x` is **not** read from disk —
    /// `File::source` honours DESIGN.md's "fail closed; user-space
    /// first" rule and does not touch the filesystem. Source files
    /// declared via `attr.label(allow_files=True)` that point at a
    /// shell script or compiled binary on disk will report
    /// `is_executable == False`. Two acceptable follow-ups:
    /// (a) `FsSourceResolver` in `ac_key.rs` reads the mode at
    /// resolver time, or (b) `File::source` plumbs the bit via the
    /// loader. Either way, REAPI input merkle trees Loupe computes
    /// for toolchain scripts will diverge from Bazel's until that
    /// gap closes.
    is_executable: bool,
}

impl File {
    pub fn source(package: PackageId, short: impl Into<String>) -> Self {
        File {
            kind: FileKind::Source,
            package,
            short: short.into(),
            config_segment: String::new(),
            repository_layout: RepositoryLayout::Default,
            // STUB: source files default to non-executable. POSIX `+x`
            // on disk is not consulted — see the field-level note on
            // `is_executable`. Toolchain scripts declared as source
            // inputs will mis-report until either the resolver or
            // `File::source` learns to read the mode.
            is_executable: false,
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
            repository_layout: RepositoryLayout::Default,
            is_executable: false,
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
            repository_layout: RepositoryLayout::Default,
            is_executable: false,
        }
    }

    /// Build-status File at `bazel-out/<basename>`. `basename` is
    /// `"volatile-status.txt"` or `"stable-status.txt"` —
    /// `ctx.version_file` / `ctx.info_file` respectively. The `package`
    /// is set to the workspace root so `.short_path()` works the same
    /// way it does for source files.
    pub fn build_status(workspace_repo: loupe_labels::CanonicalRepo, basename: &str) -> Self {
        File {
            kind: FileKind::BuildStatus,
            package: PackageId::new(workspace_repo, loupe_labels::PackagePath::root()),
            short: basename.to_string(),
            config_segment: String::new(),
            repository_layout: RepositoryLayout::Default,
            is_executable: false,
        }
    }

    /// Pin the execroot layout for this File. Wire from the active
    /// `Configuration::repository_layout()` at the construction call
    /// site (`ctx.actions.declare_file`, `synthesise_*` paths, etc.).
    /// For main-repo files the layout is observably a no-op — Bazel's
    /// own `repository_exec_path()` returns the empty string for the
    /// main repo regardless. Stored anyway so a File round-tripped
    /// across configurations stays self-describing.
    pub fn with_layout(mut self, layout: RepositoryLayout) -> Self {
        self.repository_layout = layout;
        self
    }

    /// Active execroot layout. See [`RepositoryLayout`].
    pub fn repository_layout(&self) -> RepositoryLayout {
        self.repository_layout
    }

    /// Pin the REAPI `is_executable` bit on this File. Used by
    /// `ctx.actions.write(..., is_executable=True)` and
    /// `ctx.actions.expand_template(..., is_executable=True)` to flow
    /// the rule author's explicit knob onto the recorded output File.
    pub fn with_is_executable(mut self, is_executable: bool) -> Self {
        self.is_executable = is_executable;
        self
    }

    /// REAPI `FileNode.is_executable`. See the field-level STUB note
    /// for the source-file disk-mode caveat.
    pub fn is_executable(&self) -> bool {
        self.is_executable
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

    /// `short_path` mirrors Bazel: workspace-relative path, with the
    /// package prefix included and an `../<canonical>/` prefix for
    /// external-repo files. Identical in both repository layouts —
    /// `short_path` is the *logical* view, independent of execroot.
    pub fn short_path(&self) -> String {
        let pkg_rel = self.package_relative_short();
        if self.package.repo.is_main() {
            pkg_rel
        } else {
            // Bazel's `Artifact.getRunfilesPathString()` / "logical
            // short path" for external sources uses `../<canonical>/`
            // regardless of `--experimental_sibling_repository_layout`.
            format!("../{}/{}", self.package.repo.as_str(), pkg_rel)
        }
    }

    /// `path` mirrors Bazel's `Artifact.getExecPathString()`. For
    /// source files: `<repo_exec_prefix><pkg>/<short>`. For generated
    /// files (regular or tree): `<repo_exec_prefix>bazel-out/<cfg>/bin/<pkg>/<short>`
    /// (default layout — Bazel collates every repo's bazel-out under
    /// one tree) or `../<canonical>/bazel-out/<cfg>/bin/<pkg>/<short>`
    /// (sibling layout — each repo gets its own bazel-out next to its
    /// execroot). For build-status files: `bazel-out/<basename>` (no
    /// config segment, no bin/, no repo prefix — Bazel emits the
    /// volatile/stable status files at the bazel-out root).
    ///
    /// The `<repo_exec_prefix>` is computed by
    /// [`Self::repository_exec_prefix`] and matches what vendored
    /// `cc_helper_internal.bzl::repository_exec_path()` produces — i.e.
    /// the same string Bazel uses when constructing REAPI input root
    /// merkle trees.
    pub fn path(&self) -> String {
        let pkg_rel = self.package_relative_short();
        match self.kind {
            FileKind::Source => match self.repository_exec_prefix() {
                None => pkg_rel,
                Some(prefix) => format!("{prefix}/{pkg_rel}"),
            },
            // Bazel's layout-dependent placement for generated files:
            //
            // Default layout — one shared `bazel-out/` tree under the
            // main execroot. External-repo generated artifacts sit at
            // `bazel-out/<cfg>/bin/external/<canonical>/<pkg>/<short>`:
            // the `external/<canonical>/` segment lives INSIDE the
            // bazel-out tree.
            //
            // Sibling layout — every repo has its own execroot, so each
            // gets its own bazel-out next to it. From the main repo's
            // POV an external generated artifact resolves at
            // `../<canonical>/bazel-out/<cfg>/bin/<pkg>/<short>`: the
            // `../<canonical>/` prefix is OUTSIDE the bazel-out tree.
            FileKind::GeneratedBin | FileKind::GeneratedBinDir => {
                let bazel_out_body = match self.repository_exec_prefix() {
                    None => pkg_rel,
                    Some(prefix) => match self.repository_layout {
                        RepositoryLayout::Default => {
                            // prefix sits between `bin/` and `<pkg>/<short>`.
                            format!("{prefix}/{pkg_rel}")
                        }
                        RepositoryLayout::Sibling => {
                            // prefix wraps the whole bazel-out tree.
                            return format!(
                                "{prefix}/bazel-out/{}/bin/{pkg_rel}",
                                self.config_segment
                            );
                        }
                    },
                };
                format!("bazel-out/{}/bin/{bazel_out_body}", self.config_segment)
            }
            FileKind::BuildStatus => format!("bazel-out/{}", self.short),
        }
    }

    /// `<pkg>/<short>` if the package is non-root, else `<short>`.
    /// Building block for both `path()` and `short_path()`.
    fn package_relative_short(&self) -> String {
        if self.package.path.is_root() {
            self.short.clone()
        } else {
            format!("{}/{}", self.package.path.as_str(), self.short)
        }
    }

    /// Bazel-aligned exec-root-relative repo prefix.
    ///
    /// `None` for main-repo files (under either layout); Bazel itself
    /// drops the prefix there because the execroot already sits under
    /// the workspace name.
    ///
    /// For external repos: `external/<canonical>` (default layout) or
    /// `../<canonical>` (sibling layout). Matches vendored
    /// `cc_helper_internal.bzl::repository_exec_path()` byte-for-byte.
    fn repository_exec_prefix(&self) -> Option<String> {
        if self.package.repo.is_main() {
            return None;
        }
        let prefix = match self.repository_layout {
            RepositoryLayout::Default => "external",
            RepositoryLayout::Sibling => "..",
        };
        Some(format!("{prefix}/{}", self.package.repo.as_str()))
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
    /// `short_path`, `dirname`, `is_source`, `is_directory`, `owner`,
    /// and `root`. v1 surfaces all of them; `owner` is a stub —
    /// Bazel's real `File.owner` is the label of the rule that
    /// registered the file's action (or the source file's target),
    /// which Loupe doesn't track per-file. v1 synthesises
    /// `<repo>//<pkg>:<basename>` so `args.add(file.owner)` gets a
    /// sensible-looking label string. `root` is the bazel-out path
    /// prefix (or empty for source files).
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
            // Bazel's `File.is_executable` is not part of the public
            // Starlark surface — it's only set on outputs of
            // `ctx.actions.write`/`expand_template`. Loupe exposes it
            // anyway so rules / tests can introspect the recorded bit
            // and so the AC-key path can read it without re-deriving
            // from the action kind.
            "is_executable" => Some(starlark::values::Value::new_bool(self.is_executable())),
            // `File.owner` is the canonical Bazel-side accessor.
            // The vendored `wrappers.bzl::ctx_expand_location` reads
            // `o.label` while iterating `ctx.outputs.<attr>` to build
            // the directive lookup map — Loupe doesn't model
            // per-action owner labels, so reuse the same synthesised
            // value for both names.
            "owner" | "label" => {
                let label_str = format!(
                    "@@{}//{}:{}",
                    self.package.repo.as_str(),
                    self.package.path.as_str(),
                    self.basename()
                );
                Some(heap.alloc(loupe_labels::LabelValue::parse(&label_str)))
            }
            "root" => {
                let root_path = match self.kind {
                    FileKind::Source => String::new(),
                    FileKind::GeneratedBin | FileKind::GeneratedBinDir => {
                        format!("bazel-out/{}/bin", self.config_segment)
                    }
                    FileKind::BuildStatus => "bazel-out".to_string(),
                };
                let pairs: [(&str, starlark::values::Value<'v>); 1] =
                    [("path", heap.alloc(root_path))];
                Some(heap.alloc(starlark::values::structs::AllocStruct(pairs.into_iter())))
            }
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
            "is_executable",
            "owner",
            "label",
            "root",
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
    use loupe_labels::{CanonicalRepo, PackagePath};

    fn pkg(path: &str) -> PackageId {
        PackageId::new(CanonicalRepo::MAIN, PackagePath::new(path).unwrap())
    }

    fn ext_pkg(repo: &str, path: &str) -> PackageId {
        PackageId::new(CanonicalRepo::new(repo), PackagePath::new(path).unwrap())
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
    fn external_repo_source_uses_external_prefix_in_default_layout() {
        // Bazel's `repository_exec_path()` for default
        // `--noexperimental_sibling_repository_layout` prepends
        // `external/<canonical>/` to external-repo files. Matches the
        // string Bazel uses when building the REAPI input root merkle
        // tree.
        let f = File::source(ext_pkg("rules_oci+", "oci"), "pull.bzl");
        assert_eq!(f.path(), "external/rules_oci+/oci/pull.bzl");
        // short_path is the logical view — `../<canonical>/` regardless
        // of layout. Same as `Artifact.getRunfilesPathString` in Bazel.
        assert_eq!(f.short_path(), "../rules_oci+/oci/pull.bzl");
    }

    #[test]
    fn external_repo_source_uses_dotdot_prefix_in_sibling_layout() {
        // `--experimental_sibling_repository_layout` moves each repo's
        // execroot up one level; from the main repo's POV external
        // files resolve via `../<canonical>/`.
        let f = File::source(ext_pkg("rules_oci+", "oci"), "pull.bzl")
            .with_layout(RepositoryLayout::Sibling);
        assert_eq!(f.path(), "../rules_oci+/oci/pull.bzl");
        // short_path stays the same — it's layout-independent.
        assert_eq!(f.short_path(), "../rules_oci+/oci/pull.bzl");
    }

    #[test]
    fn external_repo_generated_default_layout_keeps_external_under_bazel_out() {
        // Default layout: one shared bazel-out tree. External-repo
        // generated artifacts live at
        // `bazel-out/<cfg>/bin/external/<canonical>/<pkg>/<short>`.
        let f = File::generated_bin(
            ext_pkg("rules_oci+", "oci"),
            "manifest.json",
            "k8-fastbuild",
        );
        assert_eq!(
            f.path(),
            "bazel-out/k8-fastbuild/bin/external/rules_oci+/oci/manifest.json"
        );
        assert_eq!(f.short_path(), "../rules_oci+/oci/manifest.json");
    }

    #[test]
    fn external_repo_generated_sibling_layout_floats_bazel_out_per_repo() {
        // Sibling layout: every repo gets its own bazel-out next to
        // its execroot. From the main repo's POV that's
        // `../<canonical>/bazel-out/<cfg>/bin/<pkg>/<short>`.
        let f = File::generated_bin(
            ext_pkg("rules_oci+", "oci"),
            "manifest.json",
            "k8-fastbuild",
        )
        .with_layout(RepositoryLayout::Sibling);
        assert_eq!(
            f.path(),
            "../rules_oci+/bazel-out/k8-fastbuild/bin/oci/manifest.json"
        );
    }

    #[test]
    fn main_repo_paths_are_layout_independent() {
        // Bazel's `repository_exec_path()` returns "" for the main
        // repo under either layout; the execroot already sits at the
        // workspace root. Confirm both source and generated.
        let src = File::source(pkg("src/lib"), "main.cc");
        assert_eq!(src.path(), "src/lib/main.cc");
        assert_eq!(
            src.clone().with_layout(RepositoryLayout::Sibling).path(),
            "src/lib/main.cc",
            "sibling layout must not affect main-repo files"
        );
        let genf = File::generated_bin(pkg("src/lib"), "out.o", "k8-fastbuild");
        assert_eq!(genf.path(), "bazel-out/k8-fastbuild/bin/src/lib/out.o");
        assert_eq!(
            genf.clone().with_layout(RepositoryLayout::Sibling).path(),
            "bazel-out/k8-fastbuild/bin/src/lib/out.o"
        );
    }

    #[test]
    fn external_repo_with_root_package_does_not_emit_trailing_slash() {
        // External repo + root package: path should be
        // `external/<canonical>/<short>` (no double slash).
        let f = File::source(ext_pkg("aspect_bazel_lib+", ""), "WORKSPACE");
        assert_eq!(f.path(), "external/aspect_bazel_lib+/WORKSPACE");
        let g = File::generated_bin(ext_pkg("aspect_bazel_lib+", ""), "out.txt", "fake-cfg");
        assert_eq!(
            g.path(),
            "bazel-out/fake-cfg/bin/external/aspect_bazel_lib+/out.txt"
        );
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

    #[test]
    fn is_executable_defaults_to_false_and_with_is_executable_flips_it() {
        // Every constructor produces a non-executable File — mirrors
        // REAPI's default-zero `FileNode.is_executable` varint.
        let src = File::source(pkg(""), "tool.sh");
        assert!(!src.is_executable());
        let gen_bin = File::generated_bin(pkg(""), "out.bin", "k8-fastbuild");
        assert!(!gen_bin.is_executable());
        let tree = File::generated_bin_dir(pkg(""), "tree", "k8-fastbuild");
        assert!(!tree.is_executable());

        // Chainable setter — same shape as `with_layout`.
        let exe = File::generated_bin(pkg(""), "tool", "k8-fastbuild").with_is_executable(true);
        assert!(exe.is_executable());
        // The setter is purely a knob on this field; everything else
        // about the File stays identical.
        assert_eq!(exe.path(), "bazel-out/k8-fastbuild/bin/tool");
        // Flipping back works too.
        assert!(!exe.clone().with_is_executable(false).is_executable());
    }

    #[test]
    fn is_executable_participates_in_equality_and_hash() {
        // The merkle-tree-on-AC-key invariant depends on equality
        // tracking `is_executable` — two Files differing only in the
        // bit must NOT compare equal, or REAPI-key dedupe would over-
        // collapse and silently drift from Bazel.
        let a = File::generated_bin(pkg(""), "tool", "k8-fastbuild");
        let b = File::generated_bin(pkg(""), "tool", "k8-fastbuild").with_is_executable(true);
        assert_ne!(a, b);

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h1 = DefaultHasher::new();
        a.hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        b.hash(&mut h2);
        assert_ne!(h1.finish(), h2.finish());
    }

    #[test]
    fn starlark_is_executable_attribute_reads_the_field() {
        // Mirror the `is_source` / `is_directory` attribute test shape.
        // Drives the rule-author surface: `f.is_executable` in Starlark
        // must read the field that REAPI key derivation will later
        // consume — same place, no re-derivation from action kind.
        starlark::values::Heap::temp(|heap| {
            let exe = File::generated_bin(pkg(""), "tool", "k8-fastbuild").with_is_executable(true);
            let v = exe.get_attr("is_executable", heap).unwrap();
            assert_eq!(v.unpack_bool(), Some(true));

            let plain = File::generated_bin(pkg(""), "tool", "k8-fastbuild");
            let v = plain.get_attr("is_executable", heap).unwrap();
            assert_eq!(v.unpack_bool(), Some(false));

            // dir() must surface the attribute so Starlark's `dir(f)`
            // discovery / vendored introspection stays consistent.
            assert!(plain.dir_attr().iter().any(|s| s == "is_executable"));
        });
    }
}
