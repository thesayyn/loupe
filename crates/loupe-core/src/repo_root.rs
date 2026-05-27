//! Repo-root abstraction.
//!
//! External-repo lookups go through a `RepoRoot` enum so the engine
//! can layer compiled-in Starlark (the embedded `@bazel_tools` tree)
//! over on-disk roots (today: the bzlmod cache directory the
//! in-process resolver materialises into).
//!
//! - [`RepoRoot::Embedded`] — backed by an `include_dir::Dir` compiled
//!   into the binary. The engine prepends one of these in
//!   `EngineBuilder::build` so `@bazel_tools//...` resolves without an
//!   on-disk root.
//! - [`RepoRoot::Filesystem`] — a directory on disk under which
//!   `<canonical>/...` lives. Used for the bzlmod cache.
//!
//! Layering precedence is iteration order — first match wins.
//! Convention: `[Embedded, ...Materialised lazily by loupe-bzlmod, ...Filesystem in CLI order]`.
//! Embedded shadows on-disk versions of the same canonical so
//! vendored Bazel-side fixups stay reproducible regardless of what
//! the user has on disk.
//!
//! [`RepoRoot::Materialised`] is what loupe-bzlmod produces after a
//! lazy fetch. Shape mirrors [`RepoRoot::Embedded`] — one canonical
//! per variant — but the bytes live on disk under the bzlmod cache
//! rather than baked into the binary. Reads delegate to filesystem.
//!
//! ## No trait, no `dyn`
//!
//! `RepoRoot` is a closed enum with inherent methods that match on
//! the variant. DICE storage and call sites use plain `[RepoRoot]`
//! slices; lookups compose through iterator combinators (`find`,
//! `flat_map`). Keeps the type fully sized, avoids `Arc<dyn>` in
//! DICE values, and the variant set is small enough that adding a
//! new backing kind is a one-arm edit at the few inherent methods.

use std::path::{Path, PathBuf};

/// One source of external-repo content. See module docs for layering.
#[derive(Debug, Clone)]
pub enum RepoRoot {
    /// One repo's tree, baked into the binary. `canonical` is the
    /// repo's name; `dir` is the *contents* of that repo (its
    /// top-level entries are `MODULE.bazel`, `BUILD.bazel`, etc.).
    /// One `RepoRoot::Embedded` per repo — the engine registers one
    /// per top-level directory under `crates/loupe-core/src/starlark/`.
    Embedded {
        canonical: &'static str,
        dir: &'static include_dir::Dir<'static>,
    },
    /// On-disk directory containing one or more `<canonical>/...`
    /// subdirectories.
    Filesystem(PathBuf),
    /// Lazily-fetched single repo. `dir` is the *contents* of the
    /// repo (top-level: `MODULE.bazel`, `BUILD.bazel`, …). Produced
    /// by `loupe-bzlmod`'s `MaterializedRepoKey`.
    Materialised { canonical: String, dir: PathBuf },
}

impl RepoRoot {
    /// True iff this root provides a directory named `canonical`.
    pub fn has(&self, canonical: &str) -> bool {
        // The empty canonical is the *main* repo, which is never served
        // by an external root — its files live at the workspace root.
        // Without this guard, `Filesystem(root).has("")` evaluates
        // `root.join("").is_dir()`, which is `true` for any existing
        // root (the join appends nothing): a main-repo lookup would then
        // match the bzlmod-cache root and `glob()` / BUILD reads would be
        // mis-routed into the cache dir.
        if canonical.is_empty() {
            return false;
        }
        match self {
            Self::Embedded { canonical: c, .. } => *c == canonical,
            Self::Filesystem(root) => root.join(canonical).is_dir(),
            Self::Materialised { canonical: c, .. } => c == canonical,
        }
    }

    /// Read `<canonical>/<rel>` as bytes. `None` if the file is
    /// missing under this root (or if `canonical` doesn't match).
    pub fn read(&self, canonical: &str, rel: &Path) -> Option<Vec<u8>> {
        match self {
            Self::Embedded { canonical: c, dir } => {
                if *c != canonical {
                    return None;
                }
                dir.get_file(rel).map(|f| f.contents().to_vec())
            }
            Self::Filesystem(root) => {
                let path = root.join(canonical).join(rel);
                std::fs::read(path).ok()
            }
            Self::Materialised { canonical: c, dir } => {
                if c != canonical {
                    return None;
                }
                std::fs::read(dir.join(rel)).ok()
            }
        }
    }

    /// Every canonical this root knows about, sorted. Embedded roots
    /// serve exactly one; filesystem roots enumerate their immediate
    /// subdirs. Returned as `std::vec::IntoIter` so the type unifies
    /// across enum arms without `Box<dyn>`.
    pub fn canonicals(&self) -> std::vec::IntoIter<String> {
        let mut names: Vec<String> = match self {
            Self::Embedded { canonical, .. } => vec![(*canonical).to_string()],
            Self::Filesystem(root) => std::fs::read_dir(root)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect(),
            Self::Materialised { canonical, .. } => vec![canonical.clone()],
        };
        names.sort();
        names.into_iter()
    }

    /// On-disk parent directory holding `<canonical>/...` subdirs, if
    /// this root is filesystem-backed. Embedded and Materialised roots
    /// return `None`. Used by `workspace::discover_packages` to know
    /// which directories to skip when walking the main module.
    pub fn filesystem_root(&self) -> Option<&Path> {
        match self {
            Self::Embedded { .. } | Self::Materialised { .. } => None,
            Self::Filesystem(p) => Some(p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_root_reads_and_lists() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("repo_a/sub")).unwrap();
        std::fs::write(tmp.path().join("repo_a/MODULE.bazel"), "module(name='a')").unwrap();
        std::fs::write(tmp.path().join("repo_a/sub/BUILD.bazel"), "# x").unwrap();
        let root = RepoRoot::Filesystem(tmp.path().to_path_buf());

        assert!(root.has("repo_a"));
        assert!(!root.has("repo_b"));
        assert_eq!(root.canonicals().collect::<Vec<_>>(), vec!["repo_a"]);
        assert_eq!(
            root.read("repo_a", Path::new("MODULE.bazel")).unwrap(),
            b"module(name='a')"
        );
        assert!(root.read("repo_a", Path::new("missing")).is_none());
        assert_eq!(root.filesystem_root(), Some(tmp.path()));
    }

    #[test]
    fn empty_canonical_never_matches_a_root() {
        // The empty canonical is the main repo, which is never external.
        // A `Filesystem` root must NOT claim it: `root.join("").is_dir()`
        // is `true` for any existing dir, so without the guard the
        // bzlmod-cache root would shadow the main repo and `glob()` /
        // BUILD reads would be mis-routed into the cache (regression:
        // `write_source_file`'s existence check reported checked-in
        // source files as missing).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("some_repo")).unwrap();
        let fs_root = RepoRoot::Filesystem(tmp.path().to_path_buf());
        assert!(!fs_root.has(""), "Filesystem root must not match main repo");

        let embedded = RepoRoot::Embedded {
            canonical: "",
            dir: &EMPTY_DIR,
        };
        assert!(!embedded.has(""), "Embedded root must not match main repo");

        let materialised = RepoRoot::Materialised {
            canonical: String::new(),
            dir: tmp.path().to_path_buf(),
        };
        assert!(
            !materialised.has(""),
            "Materialised root must not match main repo"
        );
    }

    static EMPTY_DIR: include_dir::Dir<'static> = include_dir::Dir::new("", &[]);
}
