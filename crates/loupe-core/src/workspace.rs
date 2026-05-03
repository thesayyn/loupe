//! Walk a workspace on disk to discover packages.
//!
//! v1 = main module only: every directory containing a `BUILD` or
//! `BUILD.bazel` file (excluding the `bazel-*` symlinks bazel writes
//! into the workspace) is a package whose canonical repo is the main
//! module's repo. Bzlmod-style external repos under `bazel-out/external`
//! are out of scope — Phase 3+ adds external-repo discovery.

use std::path::Path;

use anyhow::{Context, Result};

use crate::label::{CanonicalRepo, PackageId, PackagePath};

/// Walk `workspace_root` and return every package present, sorted by
/// path for determinism (DESIGN.md cross-cutting requirement).
pub fn discover_packages(workspace_root: &Path) -> Result<Vec<PackageId>> {
    let mut packages = Vec::new();
    walk(workspace_root, workspace_root, &mut packages)?;
    packages.sort();
    Ok(packages)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PackageId>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?;
    let mut subdirs = Vec::new();
    let mut has_build = false;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "BUILD" || name == "BUILD.bazel" {
            has_build = true;
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_dir() && !is_skip(&name) {
            subdirs.push(entry.path());
        }
    }
    if has_build {
        let rel = dir.strip_prefix(root).unwrap();
        let path_str = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let path = PackagePath::new(path_str)
            .with_context(|| format!("invalid package path at {}", dir.display()))?;
        out.push(PackageId::new(CanonicalRepo::MAIN, path));
    }
    for sub in subdirs {
        walk(root, &sub, out)?;
    }
    Ok(())
}

fn is_skip(name: &str) -> bool {
    // Bazel symlinks (`bazel-bin`, `bazel-out`, `bazel-<workspace>`,
    // `bazel-testlogs`), version-control dirs, IDE caches.
    name.starts_with("bazel-") || matches!(name, ".git" | ".idea" | ".vscode" | "node_modules")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, "").unwrap();
    }

    #[test]
    fn discovers_top_level_and_nested_packages() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("MODULE.bazel"));
        touch(&dir.path().join("BUILD.bazel"));
        touch(&dir.path().join("foo/BUILD"));
        touch(&dir.path().join("foo/bar/BUILD.bazel"));
        // Non-package directory.
        fs::create_dir_all(dir.path().join("docs")).unwrap();

        let pkgs = discover_packages(dir.path()).unwrap();
        let paths: Vec<_> = pkgs.iter().map(|p| p.path.as_str().to_string()).collect();
        assert_eq!(paths, vec!["", "foo", "foo/bar"]);
    }

    #[test]
    fn skips_bazel_symlinks_and_dotdirs() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("BUILD"));
        touch(&dir.path().join("bazel-bin/should/not/be/seen/BUILD"));
        touch(&dir.path().join(".git/BUILD")); // in case
        let pkgs = discover_packages(dir.path()).unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].path.as_str(), "");
    }
}
