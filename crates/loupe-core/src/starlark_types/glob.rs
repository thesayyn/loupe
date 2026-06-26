//! `glob()` — package-bounded source discovery.
//!
//! From DETAILS.md:
//! > `native.glob` is the legacy macro API. Pre-`.bzl`, BUILD files
//! > used built-in functions like `glob`. […] Implement `native.glob`,
//! > `native.package_name`, `native.repository_name`.
//!
//! From DESIGN.md cross-cutting requirements:
//! > Sort glob results and directory listings. Anything user-visible
//! > has stable order.
//!
//! ## Pattern semantics
//!
//! Bazel's glob patterns:
//! - `*` matches any sequence of characters within a single path
//!   segment (no `/`).
//! - `**` matches any number of path segments (including zero) and
//!   may be the entirety of a segment. We accept it both as a literal
//!   segment (`**`) and embedded (`a**b` is treated like `a*b` with a
//!   `**` semantics — Bazel rejects this; we conservatively reject too).
//! - Other characters match literally.
//!
//! ## Package boundaries
//!
//! `glob()` only sees files **in the current package**. A subdirectory
//! that contains its own `BUILD` (or `BUILD.bazel`) starts a new
//! package and is invisible to the parent's glob — DESIGN.md
//! invariant: "Real `glob()` with package boundary respect."

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::Value;

/// Run a glob against a real package on disk.
///
/// `package_dir` is the absolute path to the package; `include` and
/// `exclude` are the BUILD-level patterns. Returns short paths
/// (relative to the package), sorted for determinism.
pub fn run(
    package_dir: &Path,
    include: &[&str],
    exclude: &[&str],
    allow_empty: bool,
) -> Result<Vec<String>> {
    for pat in include.iter().chain(exclude.iter()) {
        validate_pattern(pat)?;
    }

    // First, walk the package tree honoring the package-boundary rule.
    // We collect every file's short path (relative to `package_dir`).
    let mut files: Vec<String> = Vec::new();
    walk(package_dir, package_dir, &mut files, 0)
        .with_context(|| format!("walking package at {}", package_dir.display()))?;
    files.sort();

    // Apply include patterns; then strip excludes; then de-dupe and
    // re-sort (the order matters less now, but keep it for tests).
    let mut included: BTreeSet<String> = BTreeSet::new();
    for pat in include {
        for f in &files {
            if matches(pat, f) {
                included.insert(f.clone());
            }
        }
    }
    for pat in exclude {
        included.retain(|f| !matches(pat, f));
    }

    if included.is_empty() && !allow_empty {
        return Err(anyhow!(
            "glob(include={:?}, exclude={:?}) matched no files; \
             pass `allow_empty = True` to allow",
            include,
            exclude
        ));
    }
    Ok(included.into_iter().collect())
}

/// Bound on symlinked-directory recursion. Bazel's glob follows
/// symlinks (rules_java's `local_jdk` symlinks `bin`/`lib` dirs in, then
/// `glob(["bin/java"])`), but a symlink pointing at an ancestor would
/// loop forever. ponytail: fixed depth cap rather than canonical-path
/// cycle tracking — repo-rule symlink trees are shallow, and the cap
/// only triggers on pathological cycles a real workspace wouldn't have.
const MAX_GLOB_DEPTH: u32 = 64;

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>, depth: u32) -> Result<()> {
    if depth > MAX_GLOB_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).with_context(|| format!("read_dir({})", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        let path = entry.path();
        let ft = entry.file_type()?;
        // `file_type()` describes the symlink itself, not its target —
        // a symlinked directory reports neither `is_dir` nor `is_file`.
        // Bazel's glob follows symlinks, so resolve the target type via
        // `metadata()` (which follows). A broken symlink errors → skip.
        let (is_dir, is_file) = if ft.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(m) => (m.is_dir(), m.is_file()),
                Err(_) => (false, false),
            }
        } else {
            (ft.is_dir(), ft.is_file())
        };
        if is_dir {
            // Package boundary: a subdirectory containing its own
            // BUILD becomes a separate package and is invisible.
            if is_subpackage(&path) {
                continue;
            }
            walk(root, &path, out, depth + 1)?;
        } else if is_file {
            // Skip BUILD files themselves — Bazel doesn't expose them.
            if name_str == "BUILD" || name_str == "BUILD.bazel" {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(|e| anyhow!("strip_prefix: {e}"))?
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            out.push(rel);
        }
    }
    Ok(())
}

fn is_subpackage(dir: &Path) -> bool {
    dir.join("BUILD").is_file() || dir.join("BUILD.bazel").is_file()
}

fn validate_pattern(pat: &str) -> Result<()> {
    if pat.is_empty() {
        return Err(anyhow!("glob pattern is empty"));
    }
    // `**` must be its own path segment — Bazel rejects mixed forms
    // like `a**b` and `**.foo`.
    for seg in pat.split('/') {
        if seg.contains("**") && seg != "**" {
            return Err(anyhow!(
                "glob pattern `{pat}` mixes `**` with other characters in a segment; \
                 use `**` as its own segment"
            ));
        }
    }
    Ok(())
}

/// Match `pattern` against `path` (a slash-separated short path).
fn matches(pattern: &str, path: &str) -> bool {
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let path_segs: Vec<&str> = path.split('/').collect();
    matches_segs(&pat_segs, &path_segs)
}

fn matches_segs(pat: &[&str], path: &[&str]) -> bool {
    match (pat.first(), path.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(&"**"), _) => {
            // `**` matches zero or more *path segments*. Try every
            // possible split.
            let rest_pat = &pat[1..];
            // Match zero segments:
            if matches_segs(rest_pat, path) {
                return true;
            }
            // Match one segment, then recurse with `**` retained:
            for i in 1..=path.len() {
                if matches_segs(pat, &path[i..]) {
                    return true;
                }
            }
            false
        }
        (Some(&seg_pat), Some(&seg_path)) => {
            if !match_segment(seg_pat, seg_path) {
                return false;
            }
            matches_segs(&pat[1..], &path[1..])
        }
        (Some(_), None) => false,
    }
}

/// Single-segment glob: `*` is any non-`/` chars, anything else is
/// literal. (Character classes / `?` aren't a Bazel glob feature.)
fn match_segment(pat: &str, name: &str) -> bool {
    // Split pat on `*`. The pieces must appear in order in `name`,
    // first piece anchored at start, last piece anchored at end.
    let pieces: Vec<&str> = pat.split('*').collect();
    if pieces.len() == 1 {
        return pieces[0] == name;
    }
    if !name.starts_with(pieces[0]) {
        return false;
    }
    if !name.ends_with(pieces[pieces.len() - 1]) {
        return false;
    }
    let mut idx = pieces[0].len();
    let end = name.len() - pieces[pieces.len() - 1].len();
    if idx > end {
        return false;
    }
    for piece in &pieces[1..pieces.len() - 1] {
        if piece.is_empty() {
            continue;
        }
        match name[idx..end].find(piece) {
            Some(at) => idx += at + piece.len(),
            None => return false,
        }
    }
    true
}

#[starlark_module]
pub fn glob_globals(b: &mut GlobalsBuilder) {
    /// `glob(include, exclude = [], allow_empty = False)`. Returns a
    /// list of file short-paths (relative to the package).
    fn glob<'v>(
        // Positional-or-named: Bazel accepts `glob([...])` and
        // `glob(include = [...], exclude = [...])` (the latter used by
        // e.g. rules_python's hermetic_runtime_repo_setup.bzl). Forcing
        // positional-only rejected the named form and broke loading
        // those toolchain repos.
        include: Value<'v>,
        #[starlark(require = named)] exclude: Option<Value<'v>>,
        // STUB: Bazel defaults `allow_empty=False`, which trips when
        // Loupe's filesystem walk doesn't find files in vendored
        // external repos that have content elsewhere (e.g.
        // @platforms//os does `glob(["**"])` and expects symlinked
        // children). v1 defaults to True so the load chain doesn't
        // fail on cosmetic glob mismatches; a future phase tightens
        // this once the test fixture itself is wired.
        #[starlark(require = named, default = true)] allow_empty: bool,
        // `exclude_directories` is Bazel's "0 or 1" toggle for whether
        // directory entries appear in glob results. Loupe's walk
        // never yields directories (only files), so the toggle is a
        // no-op — accepted to match the call surface.
        #[starlark(require = named)] exclude_directories: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Vec<String>> {
        let _ = exclude_directories;
        let ctx: &crate::build_eval::BuildContext = eval
            .extra
            .ok_or_else(|| anyhow!("internal: glob() outside a BuildContext"))?
            .downcast_ref::<crate::build_eval::BuildContext>()
            .ok_or_else(|| anyhow!("internal: extra is not a BuildContext"))?;
        let include = unpack_string_list(include)?;
        let exclude = match exclude {
            Some(v) => unpack_string_list(v)?,
            None => Vec::new(),
        };
        let inc_refs: Vec<&str> = include.iter().map(|s| s.as_str()).collect();
        let exc_refs: Vec<&str> = exclude.iter().map(|s| s.as_str()).collect();
        // External packages run their glob through the RepoRoot
        // (embedded or filesystem); main-repo packages walk the
        // workspace directly.
        match ctx.repo_root() {
            Some(root) => run_via_repo_root(
                root,
                ctx.package().repo.as_str(),
                ctx.package().path.as_str(),
                &inc_refs,
                &exc_refs,
                allow_empty,
            ),
            None => {
                let pkg_dir = ctx.workspace_root().join(ctx.package().path.as_str());
                run(&pkg_dir, &inc_refs, &exc_refs, allow_empty)
            }
        }
    }
}

/// Glob against a package served through a [`RepoRoot`]. Same
/// semantics as [`run`] but file enumeration goes through the enum,
/// so embedded packages compose.
pub fn run_via_repo_root(
    root: &crate::repo_root::RepoRoot,
    canonical: &str,
    pkg_path: &str,
    include: &[&str],
    exclude: &[&str],
    allow_empty: bool,
) -> Result<Vec<String>> {
    for pat in include.iter().chain(exclude.iter()) {
        validate_pattern(pat)?;
    }
    let mut files = Vec::new();
    walk_via_root(root, canonical, pkg_path, Path::new(""), &mut files);
    files.sort();
    let mut included: BTreeSet<String> = BTreeSet::new();
    for pat in include {
        for f in &files {
            if matches(pat, f) {
                included.insert(f.clone());
            }
        }
    }
    for pat in exclude {
        included.retain(|f| !matches(pat, f));
    }
    if included.is_empty() && !allow_empty {
        return Err(anyhow!(
            "glob(include={:?}, exclude={:?}) matched no files in {canonical}//{pkg_path}; \
             pass `allow_empty = True` to allow",
            include,
            exclude
        ));
    }
    Ok(included.into_iter().collect())
}

/// Recursive walker honoring package boundaries (`BUILD`/`BUILD.bazel`
/// in a subdir starts a new package and stops the walk).
fn walk_via_root(
    root: &crate::repo_root::RepoRoot,
    canonical: &str,
    pkg_path: &str,
    rel: &Path,
    out: &mut Vec<String>,
) {
    let here = if rel.as_os_str().is_empty() {
        std::path::PathBuf::from(pkg_path)
    } else {
        Path::new(pkg_path).join(rel)
    };
    let Some(entries) = list_dir(root, canonical, &here) else {
        return;
    };
    // Sub-package boundary: a subdir with its own BUILD is invisible
    // (unless we're already at the package root).
    let has_build = entries
        .iter()
        .any(|(n, is_dir)| !is_dir && (n == "BUILD" || n == "BUILD.bazel"));
    if has_build && !rel.as_os_str().is_empty() {
        return;
    }
    for (name, is_dir) in entries {
        let child = rel.join(&name);
        if is_dir {
            walk_via_root(root, canonical, pkg_path, &child, out);
        } else if name == "BUILD" || name == "BUILD.bazel" {
            continue;
        } else {
            out.push(
                child
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
            );
        }
    }
}

/// `(name, is_dir)` listing of `<canonical>/<rel>` via a [`RepoRoot`].
fn list_dir(
    root: &crate::repo_root::RepoRoot,
    canonical: &str,
    rel: &Path,
) -> Option<Vec<(String, bool)>> {
    use crate::repo_root::RepoRoot;
    match root {
        RepoRoot::Embedded { canonical: c, dir } => {
            if *c != canonical {
                return None;
            }
            let d = if rel.as_os_str().is_empty() {
                *dir
            } else {
                dir.get_dir(rel)?
            };
            let mut out: Vec<(String, bool)> = d
                .entries()
                .iter()
                .filter_map(|e| {
                    let name = e.path().file_name()?.to_str()?.to_string();
                    Some((name, e.as_dir().is_some()))
                })
                .collect();
            out.sort();
            Some(out)
        }
        RepoRoot::Filesystem(p) => list_filesystem_dir(&p.join(canonical).join(rel)),
        RepoRoot::Materialised { canonical: c, dir } => {
            if c != canonical {
                return None;
            }
            list_filesystem_dir(&dir.join(rel))
        }
    }
}

fn list_filesystem_dir(path: &Path) -> Option<Vec<(String, bool)>> {
    let entries = std::fs::read_dir(path).ok()?;
    let mut out: Vec<(String, bool)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let ft = e.file_type().ok()?;
            // Follow symlinks (Bazel glob does) — a symlinked directory
            // reports `is_dir() == false` on the link's own type, so
            // resolve the target via `metadata()`. Broken symlink → file.
            let is_dir = if ft.is_symlink() {
                std::fs::metadata(e.path())
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
            } else {
                ft.is_dir()
            };
            Some((name, is_dir))
        })
        .collect();
    out.sort();
    Some(out)
}

fn unpack_string_list<'v>(v: Value<'v>) -> Result<Vec<String>> {
    let list = starlark::values::list::ListRef::from_value(v)
        .ok_or_else(|| anyhow!("glob() expects a list, got {}", v.get_type()))?;
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        out.push(
            item.unpack_str()
                .ok_or_else(|| anyhow!("glob() entries must be strings"))?
                .to_string(),
        );
    }
    Ok(out)
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

    // --- segment-level matcher ----------------------------------------

    #[test]
    fn segment_literal_matches_exactly() {
        assert!(match_segment("foo.cc", "foo.cc"));
        assert!(!match_segment("foo.cc", "foo.cpp"));
    }

    #[test]
    fn segment_star_matches_any_non_slash_chars() {
        assert!(match_segment("*.cc", "foo.cc"));
        assert!(match_segment("foo*.cc", "foobar.cc"));
        assert!(match_segment("*", "anything"));
        assert!(!match_segment("*.cc", "foo.h"));
    }

    #[test]
    fn segment_multiple_stars_in_one_segment() {
        assert!(match_segment("a*b*c", "axbyc"));
        assert!(match_segment("a*b*c", "abc"));
        assert!(!match_segment("a*b*c", "abxd"));
    }

    // --- multi-segment matcher ---------------------------------------

    #[test]
    fn double_star_matches_zero_or_more_segments() {
        assert!(matches("**/*.cc", "foo.cc"));
        assert!(matches("**/*.cc", "a/foo.cc"));
        assert!(matches("**/*.cc", "a/b/foo.cc"));
        assert!(!matches("**/*.cc", "foo.h"));
    }

    #[test]
    fn glob_does_not_cross_segment_with_single_star() {
        assert!(!matches("*.cc", "a/foo.cc"));
    }

    #[test]
    fn bare_double_star_matches_everything() {
        assert!(matches("**", "x"));
        assert!(matches("**", "a/b/c"));
    }

    #[test]
    fn rejects_mixed_double_star_segment() {
        assert!(validate_pattern("a**b/foo").is_err());
        assert!(validate_pattern("**.cc").is_err());
        assert!(validate_pattern("**/foo").is_ok());
        assert!(validate_pattern("foo/**").is_ok());
    }

    // --- filesystem walk ---------------------------------------------

    #[test]
    fn glob_returns_sorted_short_paths() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("BUILD"));
        touch(&dir.path().join("zeta.txt"));
        touch(&dir.path().join("alpha.txt"));
        touch(&dir.path().join("sub/inner.txt"));

        let out = run(dir.path(), &["**/*.txt"], &[], false).unwrap();
        // BUILD itself is excluded; results sorted.
        assert_eq!(out, vec!["alpha.txt", "sub/inner.txt", "zeta.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn glob_follows_symlinked_directory() {
        // rules_java's `local_jdk` symlinks `$JAVA_HOME`'s entries into the
        // repo root (so `bin` is a symlink to a real dir), then its BUILD
        // does `glob(["bin/java"])[0]`. Glob must follow the symlinked dir,
        // else the glob is empty and `[0]` panics with "Index out of bound".
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("BUILD"));
        let real = dir.path().join("real_jdk");
        touch(&real.join("bin/java"));
        std::os::unix::fs::symlink(real.join("bin"), dir.path().join("bin")).unwrap();

        let out = run(dir.path(), &["bin/java"], &[], true).unwrap();
        assert_eq!(
            out,
            vec!["bin/java"],
            "glob should follow the symlinked bin/ dir"
        );
    }

    #[test]
    fn glob_respects_package_boundaries() {
        // `sub/` has its own BUILD → separate package, invisible to
        // parent's glob.
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("BUILD"));
        touch(&dir.path().join("a.txt"));
        touch(&dir.path().join("sub/BUILD"));
        touch(&dir.path().join("sub/b.txt"));

        let out = run(dir.path(), &["**/*.txt"], &[], false).unwrap();
        assert_eq!(out, vec!["a.txt"], "sub/b.txt belongs to sub package");
    }

    #[test]
    fn glob_excludes_apply_after_includes() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.cc"));
        touch(&dir.path().join("a_test.cc"));
        touch(&dir.path().join("b.cc"));

        let out = run(dir.path(), &["*.cc"], &["*_test.cc"], false).unwrap();
        assert_eq!(out, vec!["a.cc", "b.cc"]);
    }

    #[test]
    fn empty_glob_without_allow_empty_errors() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.txt"));
        let err = run(dir.path(), &["*.cc"], &[], false).unwrap_err();
        assert!(err.to_string().contains("matched no files"));
    }

    #[test]
    fn empty_glob_with_allow_empty_returns_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.txt"));
        let out = run(dir.path(), &["*.cc"], &[], true).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn includes_dedupe_when_multiple_patterns_match_same_file() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("foo.cc"));
        // Both patterns match foo.cc; result lists it once.
        let out = run(dir.path(), &["foo.cc", "*.cc"], &[], false).unwrap();
        assert_eq!(out, vec!["foo.cc"]);
    }
}
