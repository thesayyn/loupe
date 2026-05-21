//! Walk a workspace on disk to discover packages.
//!
//! v1 = main module only: every directory containing a `BUILD` or
//! `BUILD.bazel` file (excluding the `bazel-*` symlinks bazel writes
//! into the workspace) is a package whose canonical repo is the main
//! module's repo. Bzlmod-style external repos under `bazel-out/external`
//! are out of scope — Phase 3+ adds external-repo discovery.
//!
//! Two user-controlled exclusion mechanisms:
//!
//! - **`.bazelignore`** — Bazel-canonical opt-out file at the workspace
//!   root. Each non-comment, non-blank line names a workspace-relative
//!   directory the walker skips. Trailing slashes optional. Matches
//!   Bazel's interpretation: prefix-equal, not glob.
//! - **External-root skip list** — paths the engine builder supplied
//!   via [`crate::engine::EngineBuilder::external_repo_root`]. Walking
//!   into `<workspace>/vendor` would otherwise attribute every vendored
//!   repo's BUILD files to the main module (`@@//vendor/<canonical>/...`),
//!   producing ghost-target failures. Skipping those paths during main-
//!   workspace discovery prevents the duplication.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use loupe_labels::{CanonicalRepo, PackageId, PackagePath};

/// Walk `workspace_root` and return every package present, sorted by
/// path for determinism (DESIGN.md cross-cutting requirement).
///
/// `skip_absolute` is a set of absolute paths the walker won't descend
/// into. Each entry should be a directory; non-existent entries are
/// silently ignored. Typical use: the engine's external-repo roots so
/// the vendor subdir doesn't get attributed to the main repo.
///
/// `skip_relative` is a set of workspace-relative directory paths to
/// skip — matches Bazel's `.bazelignore` semantics (parsed by
/// [`load_bazelignore`]).
pub fn discover_packages(
    workspace_root: &Path,
    skip_absolute: &[PathBuf],
    skip_relative: &[String],
) -> Result<Vec<PackageId>> {
    // Canonicalise the workspace root once. The walker calls
    // `canonicalize` on each subdir before checking it against
    // `skip_absolute` so users can pass either symlinked or real
    // paths and get consistent matching.
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let canonical_skip: Vec<PathBuf> = skip_absolute
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    // Normalise `.bazelignore` entries: strip trailing `/`, collapse
    // any leading `./`. Comparison happens on slash-joined relative
    // path strings so we can sidestep `Path` separator quirks.
    let skip_relative_norm: Vec<String> = skip_relative
        .iter()
        .map(|s| s.trim_end_matches('/').trim_start_matches("./").to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut packages = Vec::new();
    walk(
        &canonical_root,
        &canonical_root,
        &canonical_skip,
        &skip_relative_norm,
        &mut packages,
    )?;
    packages.sort();
    Ok(packages)
}

/// Parse `<workspace>/.bazelignore` into a list of workspace-relative
/// paths. Returns an empty list when the file is missing.
///
/// Format mirrors Bazel: one entry per line, blank lines and `#`
/// comments ignored, no glob expansion (each entry is a literal
/// directory path).
pub fn load_bazelignore(workspace_root: &Path) -> Vec<String> {
    let path = workspace_root.join(".bazelignore");
    let Ok(src) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    src.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn walk(
    root: &Path,
    dir: &Path,
    skip_absolute: &[PathBuf],
    skip_relative: &[String],
    out: &mut Vec<PackageId>,
) -> Result<()> {
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
        if !ft.is_dir() || is_skip(&name) {
            continue;
        }
        let sub = entry.path();
        if subdir_is_excluded(root, &sub, skip_absolute, skip_relative) {
            continue;
        }
        subdirs.push(sub);
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
        walk(root, &sub, skip_absolute, skip_relative, out)?;
    }
    Ok(())
}

/// Return `true` if `subdir` is in `skip_absolute` or matches one of
/// the workspace-relative `.bazelignore` entries.
fn subdir_is_excluded(
    root: &Path,
    subdir: &Path,
    skip_absolute: &[PathBuf],
    skip_relative: &[String],
) -> bool {
    // Absolute-path match. Canonicalise the candidate so symlinked
    // and direct paths compare equal.
    let canon = subdir
        .canonicalize()
        .unwrap_or_else(|_| subdir.to_path_buf());
    if skip_absolute.iter().any(|p| p == &canon) {
        return true;
    }
    // Relative-path match (`.bazelignore`). Compare on slash-joined
    // strings so platform separators don't trip the equality.
    if let Ok(rel) = subdir.strip_prefix(root) {
        let rel_str = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if skip_relative.iter().any(|s| *s == rel_str) {
            return true;
        }
    }
    false
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

        let pkgs = discover_packages(dir.path(), &[], &[]).unwrap();
        let paths: Vec<_> = pkgs.iter().map(|p| p.path.as_str().to_string()).collect();
        assert_eq!(paths, vec!["", "foo", "foo/bar"]);
    }

    #[test]
    fn skips_bazel_symlinks_and_dotdirs() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("BUILD"));
        touch(&dir.path().join("bazel-bin/should/not/be/seen/BUILD"));
        touch(&dir.path().join(".git/BUILD")); // in case
        let pkgs = discover_packages(dir.path(), &[], &[]).unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].path.as_str(), "");
    }

    #[test]
    fn skips_absolute_paths_matching_skip_list() {
        // Setup: a workspace with a `vendor/` subdir whose contents
        // look like vendored repos containing BUILD files. The
        // walker should skip `vendor/` when it's on the absolute
        // skip list (the engine builder passes external roots that
        // way).
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("BUILD"));
        touch(&dir.path().join("vendor/fake_dep~/BUILD"));
        touch(&dir.path().join("vendor/another~/BUILD.bazel"));

        let vendor = dir.path().join("vendor");
        let pkgs = discover_packages(dir.path(), &[vendor], &[]).unwrap();
        assert_eq!(pkgs.len(), 1, "only the root BUILD should be discovered");
        assert_eq!(pkgs[0].path.as_str(), "");
    }

    #[test]
    fn skips_relative_paths_from_bazelignore() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("BUILD"));
        touch(&dir.path().join("e2e/BUILD"));
        touch(&dir.path().join("e2e/deep/BUILD"));
        touch(&dir.path().join("examples/foo/BUILD"));
        touch(&dir.path().join("keep/me/BUILD"));

        // Mirror what `load_bazelignore` would have produced from
        // `e2e/` and `examples/` entries.
        let ignore = vec!["e2e".to_string(), "examples".to_string()];
        let pkgs = discover_packages(dir.path(), &[], &ignore).unwrap();
        let paths: Vec<_> = pkgs.iter().map(|p| p.path.as_str().to_string()).collect();
        assert_eq!(paths, vec!["", "keep/me"]);
    }

    #[test]
    fn load_bazelignore_strips_comments_and_blanks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".bazelignore"),
            "# nested modules\ne2e/\nexamples/\n\n   # blank above\nnode_modules/\n",
        )
        .unwrap();
        let entries = load_bazelignore(dir.path());
        assert_eq!(
            entries,
            vec!["e2e/", "examples/", "node_modules/"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn load_bazelignore_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_bazelignore(dir.path()).is_empty());
    }

    #[test]
    fn bazelignore_trailing_slash_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("BUILD"));
        touch(&dir.path().join("e2e/BUILD"));
        // Without the trailing slash:
        let pkgs = discover_packages(dir.path(), &[], &["e2e".to_string()]).unwrap();
        assert_eq!(pkgs.len(), 1);
        // With the trailing slash:
        let pkgs = discover_packages(dir.path(), &[], &["e2e/".to_string()]).unwrap();
        assert_eq!(pkgs.len(), 1);
    }
}
