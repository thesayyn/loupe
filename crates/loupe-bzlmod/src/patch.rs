//! Minimal unified-diff (`patch`) applier for BCR `source.json`
//! `patches`.
//!
//! BCR modules whose upstream source carries no Bazel `BUILD` ship one
//! or more `patches/<f>.patch` files in the registry; `zlib` is the
//! canonical case — its `add_build_file.patch` is a new-file diff
//! (`--- /dev/null` / `+++ <path>`) that creates `BUILD.bazel`.
//!
//! Scope is deliberately narrow: enough to apply real BCR patches
//! correctly, not a general `patch(1)`. We handle:
//!
//! * New-file patches (`--- /dev/null`): the target file is created
//!   from the hunk's `+` lines.
//! * Existing-file hunks (`@@ -old,n +new,m @@`) applied with context
//!   verification — a context/removed line that doesn't match the file
//!   is a hard error (a mis-applied patch is worse than none).
//!
//! Out of scope (raises rather than guessing): file deletion
//! (`+++ /dev/null`), renames (`rename from`/`rename to`), binary
//! diffs, and fuzz/offset matching. No real BCR overlay-substitute
//! patch needs these; if one shows up we want a loud error, not a
//! silent half-apply.
//!
//! `-p<strip>` mirrors `patch --strip=N` / Bazel's `patch_strip`: strip
//! that many leading path components from the `+++`/`---` header paths.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("io applying patch to {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed patch: {0}")]
    Malformed(String),

    #[error("unsupported patch feature: {0}")]
    Unsupported(String),

    #[error(
        "hunk did not apply to {path}: expected `{expected}` at line {line}, found `{found}`"
    )]
    ContextMismatch {
        path: String,
        line: usize,
        expected: String,
        found: String,
    },

    #[error("patch target path `{0}` escapes the repository root")]
    PathEscape(String),
}

/// Apply a single unified-diff `patch` to the tree rooted at `root`,
/// stripping `strip` leading path components from header paths. A patch
/// file may contain multiple `diff`/file sections; each is applied in
/// order.
pub fn apply_patch(root: &Path, patch: &str, strip: u32) -> Result<(), PatchError> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut i = 0usize;
    let mut applied_any = false;

    while i < lines.len() {
        // Seek the next `--- ` file header, skipping `diff --git`,
        // `index`, mode lines, and any preamble.
        if !lines[i].starts_with("--- ") {
            i += 1;
            continue;
        }
        let old_header = lines[i];
        if i + 1 >= lines.len() || !lines[i + 1].starts_with("+++ ") {
            return Err(PatchError::Malformed(format!(
                "`--- ` header at line {} not followed by `+++ `",
                i + 1
            )));
        }
        let new_header = lines[i + 1];
        i += 2;

        let old_path = header_path(old_header.trim_start_matches("--- "));
        let new_path = header_path(new_header.trim_start_matches("+++ "));
        let is_new_file = old_path == "/dev/null";
        if new_path == "/dev/null" {
            return Err(PatchError::Unsupported(
                "file deletion (`+++ /dev/null`)".to_string(),
            ));
        }

        // Collect this file section's hunks: every run of `@@ ... @@`
        // plus its body, until the next file header or EOF.
        let target_rel = strip_components(&new_path, strip)?;
        let target = safe_join(root, &target_rel)?;

        let mut hunks: Vec<Hunk> = Vec::new();
        while i < lines.len() {
            if lines[i].starts_with("--- ") {
                break;
            }
            if !lines[i].starts_with("@@") {
                // Tolerate stray lines between file header and first
                // hunk (e.g. a trailing blank from a prior section).
                i += 1;
                continue;
            }
            let old_start = parse_hunk_header(lines[i])?;
            i += 1;
            // Read the hunk body until the next hunk (`@@`), the next
            // file header (`--- `), or EOF. We can't use the `@@`
            // header's old/new line counts as the terminator: a
            // pure-addition hunk (`@@ -0,0 +1,N @@`, the new-file case)
            // has an old count of 0, which would stop us before reading
            // any `+` line.
            let mut body: Vec<(LineKind, String)> = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if l.starts_with("--- ") || l.starts_with("@@") {
                    break;
                }
                // `\ No newline at end of file` markers carry no line.
                if l.starts_with('\\') {
                    i += 1;
                    continue;
                }
                body.push(classify_body_line(l));
                i += 1;
            }
            hunks.push(Hunk { old_start, body });
        }

        if is_new_file {
            apply_new_file(&target, &hunks)?;
        } else {
            apply_to_existing(&target, &hunks)?;
        }
        applied_any = true;
    }

    if !applied_any {
        return Err(PatchError::Malformed(
            "no `--- `/`+++ ` file header found in patch".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Context,
    Added,
    Removed,
}

struct Hunk {
    /// 1-based starting line in the *old* file (from `@@ -old,n ...`).
    old_start: usize,
    body: Vec<(LineKind, String)>,
}

/// Create a brand-new file from a new-file patch: concatenate every
/// `+` line. Any context/removed line in a `/dev/null` patch is
/// malformed.
fn apply_new_file(target: &Path, hunks: &[Hunk]) -> Result<(), PatchError> {
    let mut out: Vec<String> = Vec::new();
    for h in hunks {
        for (kind, text) in &h.body {
            match kind {
                LineKind::Added => out.push(text.clone()),
                LineKind::Context if text.is_empty() => out.push(text.clone()),
                _ => {
                    return Err(PatchError::Malformed(format!(
                        "new-file patch for {} contains a non-added line",
                        target.display()
                    )));
                }
            }
        }
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| PatchError::Io {
            path: parent.display().to_string(),
            source: e,
        })?;
    }
    let mut content = out.join("\n");
    // Unified diffs describe line content; a trailing newline is
    // conventional for text files and BCR BUILD patches expect one.
    content.push('\n');
    std::fs::write(target, content).map_err(|e| PatchError::Io {
        path: target.display().to_string(),
        source: e,
    })
}

/// Apply hunks to an existing file, verifying context + removed lines
/// against the current content.
fn apply_to_existing(target: &Path, hunks: &[Hunk]) -> Result<(), PatchError> {
    let original = std::fs::read_to_string(target).map_err(|e| PatchError::Io {
        path: target.display().to_string(),
        source: e,
    })?;
    let had_trailing_newline = original.ends_with('\n');
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();

    // Apply hunks bottom-up so earlier edits don't shift later hunks'
    // line numbers.
    let mut ordered: Vec<&Hunk> = hunks.iter().collect();
    ordered.sort_by_key(|h| std::cmp::Reverse(h.old_start));

    for h in ordered {
        // `old_start` is 1-based; an empty file / start-of-file hunk
        // uses 0 in some diffs, clamp to a 0-based index.
        let mut idx = h.old_start.saturating_sub(1);
        for (kind, text) in &h.body {
            match kind {
                LineKind::Context => {
                    let found = lines.get(idx).map(String::as_str).unwrap_or("");
                    if found != text {
                        return Err(PatchError::ContextMismatch {
                            path: target.display().to_string(),
                            line: idx + 1,
                            expected: text.clone(),
                            found: found.to_string(),
                        });
                    }
                    idx += 1;
                }
                LineKind::Removed => {
                    let found = lines.get(idx).map(String::as_str).unwrap_or("");
                    if found != text {
                        return Err(PatchError::ContextMismatch {
                            path: target.display().to_string(),
                            line: idx + 1,
                            expected: text.clone(),
                            found: found.to_string(),
                        });
                    }
                    lines.remove(idx);
                }
                LineKind::Added => {
                    lines.insert(idx, text.clone());
                    idx += 1;
                }
            }
        }
    }

    let mut content = lines.join("\n");
    if had_trailing_newline {
        content.push('\n');
    }
    std::fs::write(target, content).map_err(|e| PatchError::Io {
        path: target.display().to_string(),
        source: e,
    })
}

fn classify_body_line(l: &str) -> (LineKind, String) {
    match l.as_bytes().first() {
        Some(b'+') => (LineKind::Added, l[1..].to_string()),
        Some(b'-') => (LineKind::Removed, l[1..].to_string()),
        Some(b' ') => (LineKind::Context, l[1..].to_string()),
        // A fully blank line inside a hunk is context with empty text.
        _ => (LineKind::Context, String::new()),
    }
}

/// Parse `@@ -oldStart,oldCount +newStart,newCount @@ [section]` and
/// return `oldStart` (1-based; 0 for a new-file hunk). The line counts
/// aren't needed — hunk bodies are delimited by the next `@@`/`--- `.
fn parse_hunk_header(l: &str) -> Result<usize, PatchError> {
    let inner = l
        .strip_prefix("@@")
        .and_then(|r| r.find("@@").map(|end| &r[..end]))
        .ok_or_else(|| PatchError::Malformed(format!("bad hunk header `{l}`")))?;
    let old_spec = inner
        .split_whitespace()
        .find(|t| t.starts_with('-'))
        .ok_or_else(|| PatchError::Malformed(format!("hunk header missing `-` range `{l}`")))?;
    let nums = old_spec.trim_start_matches('-');
    let start = match nums.split_once(',') {
        Some((s, _)) => parse_usize(s, l)?,
        None => parse_usize(nums, l)?,
    };
    Ok(start)
}

fn parse_usize(s: &str, ctx: &str) -> Result<usize, PatchError> {
    s.parse::<usize>()
        .map_err(|_| PatchError::Malformed(format!("non-numeric hunk range in `{ctx}`")))
}

/// Take the path token from a `---`/`+++` header line, dropping a
/// trailing tab-delimited timestamp (`path\t2024-01-01 ...`) that
/// some diff tools append.
fn header_path(rest: &str) -> String {
    let token = rest.split('\t').next().unwrap_or(rest).trim();
    token.to_string()
}

/// Strip `strip` leading components from a header path, mirroring
/// `patch -p<strip>`. With `strip == 0` the path is used verbatim
/// (BCR `patch_strip: 0` patches carry bare paths like `BUILD.bazel`).
fn strip_components(path: &str, strip: u32) -> Result<PathBuf, PatchError> {
    let p = Path::new(path);
    let mut comps: Vec<&std::ffi::OsStr> = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => comps.push(s),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(PatchError::PathEscape(path.to_string()));
            }
        }
    }
    if (strip as usize) > comps.len() {
        return Err(PatchError::Malformed(format!(
            "patch_strip {strip} exceeds path depth of `{path}`"
        )));
    }
    let mut out = PathBuf::new();
    for s in comps.into_iter().skip(strip as usize) {
        out.push(s);
    }
    Ok(out)
}

/// Join `rel` onto `root`, rejecting any component that would escape
/// the root. Mirrors `archive::safe_join`.
fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf, PatchError> {
    let mut joined = root.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(s) => joined.push(s),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PatchError::PathEscape(rel.display().to_string()));
            }
        }
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_file_patch_creates_build_file() {
        // zlib-shaped `add_build_file.patch`: `--- /dev/null` creates a
        // BUILD.bazel from the `+` lines. patch_strip = 0 → bare path.
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
--- /dev/null
+++ BUILD.bazel
@@ -0,0 +1,3 @@
+filegroup(
+    name = \"z\",
+)
";
        apply_patch(dir.path(), patch, 0).unwrap();
        let got = std::fs::read_to_string(dir.path().join("BUILD.bazel")).unwrap();
        assert_eq!(got, "filegroup(\n    name = \"z\",\n)\n");
    }

    #[test]
    fn new_file_patch_respects_patch_strip() {
        // `+++ b/BUILD.bazel` with patch_strip = 1 → `BUILD.bazel`.
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
--- /dev/null
+++ b/BUILD.bazel
@@ -0,0 +1,1 @@
+exports_files([\"z\"])
";
        apply_patch(dir.path(), patch, 1).unwrap();
        assert!(dir.path().join("BUILD.bazel").is_file());
        let got = std::fs::read_to_string(dir.path().join("BUILD.bazel")).unwrap();
        assert_eq!(got, "exports_files([\"z\"])\n");
    }

    #[test]
    fn modifies_existing_file_with_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        // Insert `B` after `b`, keeping context lines `a` and `c`.
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,4 @@
 a
 b
+B
 c
";
        apply_patch(dir.path(), patch, 1).unwrap();
        let got = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(got, "a\nb\nB\nc\n");
    }

    #[test]
    fn removes_line_with_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,2 @@
 a
-b
 c
";
        apply_patch(dir.path(), patch, 1).unwrap();
        let got = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(got, "a\nc\n");
    }

    #[test]
    fn context_mismatch_is_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x\ny\nz\n").unwrap();
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,4 @@
 a
 b
+B
 c
";
        let r = apply_patch(dir.path(), patch, 1);
        assert!(matches!(r, Err(PatchError::ContextMismatch { .. })));
    }

    #[test]
    fn multi_file_patch_applies_each_section() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
--- /dev/null
+++ BUILD.bazel
@@ -0,0 +1,1 @@
+filegroup(name = \"z\")
--- /dev/null
+++ MODULE.bazel
@@ -0,0 +1,1 @@
+module(name = \"zlib\")
";
        apply_patch(dir.path(), patch, 0).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("BUILD.bazel")).unwrap(),
            "filegroup(name = \"z\")\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("MODULE.bazel")).unwrap(),
            "module(name = \"zlib\")\n"
        );
    }

    #[test]
    fn skips_git_diff_preamble() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
diff --git a/BUILD.bazel b/BUILD.bazel
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/BUILD.bazel
@@ -0,0 +1,1 @@
+filegroup(name = \"z\")
";
        apply_patch(dir.path(), patch, 1).unwrap();
        assert!(dir.path().join("BUILD.bazel").is_file());
    }

    #[test]
    fn path_escape_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
--- /dev/null
+++ ../escape.txt
@@ -0,0 +1,1 @@
+nope
";
        let r = apply_patch(dir.path(), patch, 0);
        assert!(matches!(r, Err(PatchError::PathEscape(_))));
    }

    #[test]
    fn file_deletion_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\n").unwrap();
        let patch = "\
--- a/f.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-a
";
        let r = apply_patch(dir.path(), patch, 1);
        assert!(matches!(r, Err(PatchError::Unsupported(_))));
    }
}
