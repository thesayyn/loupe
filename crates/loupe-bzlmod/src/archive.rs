//! Archive extraction (`.tar.{gz,xz,zst}`, `.tar`, `.zip`) with
//! Bazel-compatible `strip_prefix` semantics.
//!
//! Bazel's `strip_prefix` is an *exact-match* prefix removed from every
//! entry, not a "skip N components" — if any entry doesn't start with
//! the prefix, extraction fails. We mirror that.
//!
//! CPU-bound; call via [`tokio::task::spawn_blocking`] from async paths.
//!
//! Symlinks on Windows raise [`ArchiveError::WindowsSymlink`]
//! ("real or fail, never silent stubs" — DESIGN.md). Unix extracts them
//! as ordinary symlinks.

use std::io::Cursor;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    TarXz,
    TarZst,
    Tar,
    Zip,
}

impl ArchiveKind {
    /// Best-effort detection from URL or filename. Returns `None` for
    /// unrecognised extensions; callers may also provide an explicit
    /// kind from `http_archive`'s `type` attribute.
    pub fn from_url(url: &str) -> Option<Self> {
        let lower = url.to_ascii_lowercase();
        let path = lower.split(['?', '#']).next().unwrap_or(&lower);
        if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
            Some(ArchiveKind::TarGz)
        } else if path.ends_with(".tar.xz") || path.ends_with(".txz") {
            Some(ArchiveKind::TarXz)
        } else if path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") {
            Some(ArchiveKind::TarZst)
        } else if path.ends_with(".tar") {
            Some(ArchiveKind::Tar)
        } else if path.ends_with(".zip") || path.ends_with(".jar") {
            Some(ArchiveKind::Zip)
        } else {
            None
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("entry path escapes destination: {0}")]
    PathEscape(String),

    #[error("strip_prefix `{0}` did not match any archive entry")]
    StripPrefixUnused(String),

    #[error("symlinks are not supported on Windows; archive entry: {0}")]
    WindowsSymlink(String),

    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
}

/// Extract `bytes` interpreted as `kind` into `dest`. Creates `dest`
/// if it doesn't exist. Applies `strip_prefix` exactly as Bazel does.
pub fn extract(
    kind: ArchiveKind,
    bytes: &[u8],
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<(), ArchiveError> {
    std::fs::create_dir_all(dest)?;
    match kind {
        ArchiveKind::TarGz => extract_tar(
            &mut flate2::read::GzDecoder::new(Cursor::new(bytes)),
            dest,
            strip_prefix,
        ),
        ArchiveKind::TarXz => extract_tar(
            &mut xz2::read::XzDecoder::new(Cursor::new(bytes)),
            dest,
            strip_prefix,
        ),
        ArchiveKind::TarZst => extract_tar(
            &mut zstd::stream::read::Decoder::new(Cursor::new(bytes))?,
            dest,
            strip_prefix,
        ),
        ArchiveKind::Tar => extract_tar(&mut Cursor::new(bytes), dest, strip_prefix),
        ArchiveKind::Zip => extract_zip(bytes, dest, strip_prefix),
    }
}

fn extract_tar<R: Read>(
    reader: &mut R,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<(), ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);

    let normalized_prefix = strip_prefix.map(normalize_prefix);
    let mut prefix_matched = false;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        let entry_type = entry.header().entry_type();
        // PAX and GNU extension records carry metadata for the *next*
        // regular entry — they are not files. Their paths
        // (`pax_global_header`, `././@LongLink`, etc.) won't match any
        // real strip_prefix and must be ignored. The `tar` crate
        // already applies their effects to subsequent entries
        // transparently; we just need to not unpack them.
        if matches!(
            entry_type,
            tar::EntryType::XHeader
                | tar::EntryType::XGlobalHeader
                | tar::EntryType::GNULongName
                | tar::EntryType::GNULongLink
        ) {
            continue;
        }
        let is_symlink = entry_type == tar::EntryType::Symlink;
        let is_hardlink = entry_type == tar::EntryType::Link;

        let Some(stripped) = apply_strip(&path, normalized_prefix.as_deref())? else {
            continue;
        };
        if stripped.as_os_str().is_empty() {
            continue;
        }
        prefix_matched = true;

        if (is_symlink || is_hardlink) && cfg!(windows) {
            return Err(ArchiveError::WindowsSymlink(path.display().to_string()));
        }

        let target = safe_join(dest, &stripped)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
    }

    if let Some(prefix) = normalized_prefix
        && !prefix.is_empty()
        && !prefix_matched
    {
        return Err(ArchiveError::StripPrefixUnused(prefix));
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path, strip_prefix: Option<&str>) -> Result<(), ArchiveError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let normalized_prefix = strip_prefix.map(normalize_prefix);
    let mut prefix_matched = false;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let Some(enclosed) = file.enclosed_name() else {
            return Err(ArchiveError::PathEscape(file.name().to_string()));
        };

        let Some(stripped) = apply_strip(&enclosed, normalized_prefix.as_deref())? else {
            continue;
        };
        if stripped.as_os_str().is_empty() {
            continue;
        }
        prefix_matched = true;

        let is_symlink = file
            .unix_mode()
            .map(|m| m & 0xF000 == 0xA000)
            .unwrap_or(false);
        if is_symlink && cfg!(windows) {
            return Err(ArchiveError::WindowsSymlink(enclosed.display().to_string()));
        }

        let target = safe_join(dest, &stripped)?;
        if file.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if is_symlink {
            #[cfg(unix)]
            {
                let mut link = String::new();
                file.read_to_string(&mut link)?;
                if target.exists() || target.is_symlink() {
                    std::fs::remove_file(&target)?;
                }
                std::os::unix::fs::symlink(link, &target)?;
            }
            continue;
        }

        let mut out = std::fs::File::create(&target)?;
        std::io::copy(&mut file, &mut out)?;

        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
        }
    }

    if let Some(prefix) = normalized_prefix
        && !prefix.is_empty()
        && !prefix_matched
    {
        return Err(ArchiveError::StripPrefixUnused(prefix));
    }
    Ok(())
}

fn normalize_prefix(p: &str) -> String {
    p.trim_end_matches('/').to_string()
}

/// Strip the prefix from `path`. Returns `Ok(None)` for any entry
/// that isn't a descendant of the prefix (the prefix dir itself, a
/// bare-directory ancestor, sibling-of-prefix, or unrelated entries
/// all collapse to a single skip case). Returns `Ok(Some(remainder))`
/// for an entry under the prefix.
///
/// Bazel's `StripPrefixedPath` skips non-matching entries silently
/// and raises `StripPrefixUnused` only if NO entry matched. The
/// caller tracks `prefix_matched` and raises after the loop if
/// nothing matched.
fn apply_strip(path: &Path, prefix: Option<&str>) -> Result<Option<PathBuf>, ArchiveError> {
    let Some(prefix) = prefix else {
        return Ok(Some(path.to_path_buf()));
    };
    if prefix.is_empty() {
        return Ok(Some(path.to_path_buf()));
    }
    let prefix_path = Path::new(prefix);
    match path.strip_prefix(prefix_path) {
        Ok(rest) => Ok(Some(rest.to_path_buf())),
        Err(_) => {
            if path == prefix_path {
                return Ok(Some(PathBuf::new()));
            }
            // Skip silently. Covers:
            //   - Bare-directory ancestor of the prefix (archive
            //     contains an explicit dir entry for an ancestor of
            //     strip_prefix; the dir gets implicitly created by
            //     deeper entries).
            //   - Sibling-of-prefix / unrelated entries (e.g.
            //     archive with `pkg/.bcr/` and `pkg/metadata/` where
            //     strip_prefix=`pkg/metadata`; Bazel skips `.bcr/`).
            // `StripPrefixUnused` after the loop covers the
            // genuinely-empty-match case.
            Ok(None)
        }
    }
}

/// Reject entries that would escape `dest` via `..` or absolute paths.
fn safe_join(dest: &Path, rel: &Path) -> Result<PathBuf, ArchiveError> {
    let mut joined = dest.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(s) => joined.push(s),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ArchiveError::PathEscape(rel.display().to_string()));
            }
        }
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn build_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut t = tar::Builder::new(gz);
            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                t.append_data(&mut header, name, *data).unwrap();
            }
            t.into_inner().unwrap().finish().unwrap();
        }
        buf
    }

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
            for (name, data) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn detect_kind() {
        assert_eq!(ArchiveKind::from_url("a.tar.gz"), Some(ArchiveKind::TarGz));
        assert_eq!(ArchiveKind::from_url("a.tgz"), Some(ArchiveKind::TarGz));
        assert_eq!(ArchiveKind::from_url("a.tar.xz"), Some(ArchiveKind::TarXz));
        assert_eq!(
            ArchiveKind::from_url("a.tar.zst"),
            Some(ArchiveKind::TarZst)
        );
        assert_eq!(ArchiveKind::from_url("a.tar"), Some(ArchiveKind::Tar));
        assert_eq!(ArchiveKind::from_url("a.zip"), Some(ArchiveKind::Zip));
        assert_eq!(
            ArchiveKind::from_url("https://x/y/foo.tar.gz?token=1"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(ArchiveKind::from_url("a.rar"), None);
    }

    #[test]
    fn tar_gz_round_trip() {
        let buf = build_tar_gz(&[
            ("pkg-1.2.3/README", b"hello"),
            ("pkg-1.2.3/src/main.rs", b"fn main() {}"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        extract(ArchiveKind::TarGz, &buf, tmp.path(), Some("pkg-1.2.3")).unwrap();
        let readme = std::fs::read(tmp.path().join("README")).unwrap();
        assert_eq!(readme, b"hello");
        let main = std::fs::read(tmp.path().join("src/main.rs")).unwrap();
        assert_eq!(main, b"fn main() {}");
    }

    fn build_tar_gz_with_pax_global_header(entries: &[(&str, &[u8])]) -> Vec<u8> {
        // Mirror what GitHub's release tarballs (and many other
        // producers) emit: a leading `pax_global_header` entry of type
        // 'g' carrying repo metadata, followed by the real files.
        let mut buf: Vec<u8> = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut t = tar::Builder::new(gz);

            let pax_payload: &[u8] = b"52 comment=hash\n";
            let mut pax = tar::Header::new_ustar();
            pax.set_size(pax_payload.len() as u64);
            pax.set_mode(0o644);
            pax.set_entry_type(tar::EntryType::XGlobalHeader);
            pax.set_cksum();
            t.append_data(&mut pax, "pax_global_header", pax_payload)
                .unwrap();

            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                t.append_data(&mut header, name, *data).unwrap();
            }
            t.into_inner().unwrap().finish().unwrap();
        }
        buf
    }

    #[test]
    fn tar_skips_pax_global_header() {
        // Regression: `rules_nodejs-6.7.3.tar.gz` (and many GitHub
        // release tarballs) ships a leading `pax_global_header` entry
        // of tar type 'g'. The extractor was applying `strip_prefix`
        // to that entry's path and erroring because `pax_global_header`
        // doesn't begin with the expected `rules_nodejs-6.7.3` prefix.
        let buf = build_tar_gz_with_pax_global_header(&[("rules_nodejs-6.7.3/README", b"hi")]);
        let tmp = tempfile::tempdir().unwrap();
        extract(
            ArchiveKind::TarGz,
            &buf,
            tmp.path(),
            Some("rules_nodejs-6.7.3"),
        )
        .unwrap();
        assert_eq!(std::fs::read(tmp.path().join("README")).unwrap(), b"hi");
        // The metadata entry must not have been unpacked anywhere
        // under dest — its path would have collided with the strip
        // check, but skipping it should also not leave a stray file.
        assert!(!tmp.path().join("pax_global_header").exists());
    }

    #[test]
    fn tar_no_strip_prefix() {
        let buf = build_tar_gz(&[("a.txt", b"x")]);
        let tmp = tempfile::tempdir().unwrap();
        extract(ArchiveKind::TarGz, &buf, tmp.path(), None).unwrap();
        assert_eq!(std::fs::read(tmp.path().join("a.txt")).unwrap(), b"x");
    }

    #[test]
    fn tar_strip_prefix_no_match_raises_unused() {
        // No entry under `expected/` → StripPrefixUnused. Bazel skips
        // non-matching entries silently and only raises when nothing
        // matched.
        let buf = build_tar_gz(&[("other/file", b"x")]);
        let tmp = tempfile::tempdir().unwrap();
        let r = extract(ArchiveKind::TarGz, &buf, tmp.path(), Some("expected"));
        assert!(matches!(r, Err(ArchiveError::StripPrefixUnused(_))));
    }

    #[test]
    fn tar_strip_prefix_skips_ancestor_dir_entries() {
        // Mirrors the rust toolchain archive shape: a bare-directory
        // entry for the top-level dir (`pkg`) followed by file
        // entries under a deeper strip_prefix (`pkg/inner`). Bazel
        // silently skips the bare-directory ancestor.
        let buf = build_tar_gz(&[("pkg", b""), ("pkg/inner/file.txt", b"hi")]);
        let tmp = tempfile::tempdir().unwrap();
        extract(ArchiveKind::TarGz, &buf, tmp.path(), Some("pkg/inner")).unwrap();
        assert_eq!(std::fs::read(tmp.path().join("file.txt")).unwrap(), b"hi");
    }

    #[test]
    fn tar_strip_prefix_skips_sibling_entries() {
        // Archive contains both files under the prefix
        // (`pkg/metadata/...`) AND siblings outside it
        // (`pkg/.bcr/...`). Bazel skips the siblings silently and
        // extracts only the prefix-matching entries.
        let buf = build_tar_gz(&[
            ("pkg/.bcr/source.json", b"sibling"),
            ("pkg/metadata/info.yaml", b"under-prefix"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        extract(ArchiveKind::TarGz, &buf, tmp.path(), Some("pkg/metadata")).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("info.yaml")).unwrap(),
            b"under-prefix"
        );
        assert!(!tmp.path().join(".bcr/source.json").exists());
    }

    #[test]
    fn tar_strip_prefix_unused_errors() {
        let buf = build_tar_gz(&[]);
        let tmp = tempfile::tempdir().unwrap();
        let r = extract(ArchiveKind::TarGz, &buf, tmp.path(), Some("never-matches"));
        assert!(matches!(r, Err(ArchiveError::StripPrefixUnused(_))));
    }

    #[test]
    fn zip_round_trip() {
        let buf = build_zip(&[("pkg/file.txt", b"hi"), ("pkg/sub/x", b"y")]);
        let tmp = tempfile::tempdir().unwrap();
        extract(ArchiveKind::Zip, &buf, tmp.path(), Some("pkg")).unwrap();
        assert_eq!(std::fs::read(tmp.path().join("file.txt")).unwrap(), b"hi");
        assert_eq!(std::fs::read(tmp.path().join("sub/x")).unwrap(), b"y");
    }

    #[test]
    fn safe_join_rejects_parent_dir() {
        let r = safe_join(Path::new("/tmp/x"), Path::new("../escape"));
        assert!(matches!(r, Err(ArchiveError::PathEscape(_))));
    }

    #[test]
    fn safe_join_rejects_absolute() {
        let r = safe_join(Path::new("/tmp/x"), Path::new("/etc/passwd"));
        assert!(matches!(r, Err(ArchiveError::PathEscape(_))));
    }

    #[test]
    fn safe_join_accepts_normal() {
        let r = safe_join(Path::new("/tmp/x"), Path::new("a/b/c")).unwrap();
        assert_eq!(r, Path::new("/tmp/x/a/b/c"));
    }
}
