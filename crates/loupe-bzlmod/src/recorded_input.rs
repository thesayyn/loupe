//! Reproducibility records for a materialised repo.
//!
//! Mirrors Bazel's `RepoRecordedInput` family: every observation a
//! repo-rule impl makes through `ctx.read` / `ctx.watch` / `ctx.getenv`
//! / label resolution is captured as one record and written into the
//! repo's marker file. On a subsequent run, each record is re-observed;
//! the first mismatch triggers a re-materialise.
//!
//! `ctx.execute` records nothing implicit — the contract is that the
//! rule explicitly observes anything that should affect the cache.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// One observation made by a repo-rule impl. The variants line up with
/// Bazel's marker-file record prefixes: FILE / DIRENTS / DIRTREE / ENV /
/// REPO_MAPPING.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoRecordedInput {
    File {
        path: PathBuf,
        value: FileValue,
    },
    Dirents {
        path: PathBuf,
        value: DirentsValue,
    },
    DirTree {
        path: PathBuf,
        value: DirTreeValue,
    },
    EnvVar {
        name: String,
        value: Option<String>,
    },
    RepoMapping {
        source_repo: String,
        apparent: String,
        canonical: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileValue {
    Digest(String),
    Dir,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirentsValue {
    Fingerprint(String),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirTreeValue {
    Digest(String),
    Missing,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordParseError {
    #[error("unknown record prefix: {0}")]
    UnknownPrefix(String),

    #[error("malformed record line: {0}")]
    Malformed(String),
}

impl RepoRecordedInput {
    /// Bazel-format marker line for this record. One per line; no
    /// trailing newline. Whitespace inside paths and env values is
    /// percent-escaped so the format stays line-oriented.
    pub fn to_marker_line(&self) -> String {
        match self {
            RepoRecordedInput::File { path, value } => {
                let tail = match value {
                    FileValue::Digest(d) => d.clone(),
                    FileValue::Dir => "DIR".to_string(),
                    FileValue::Missing => "ENOENT".to_string(),
                };
                format!("FILE:{} {}", encode(path.to_string_lossy().as_ref()), tail)
            }
            RepoRecordedInput::Dirents { path, value } => {
                let tail = match value {
                    DirentsValue::Fingerprint(f) => f.clone(),
                    DirentsValue::Missing => "ENOENT".to_string(),
                };
                format!("DIRENTS:{} {}", encode(path.to_string_lossy().as_ref()), tail)
            }
            RepoRecordedInput::DirTree { path, value } => {
                let tail = match value {
                    DirTreeValue::Digest(d) => d.clone(),
                    DirTreeValue::Missing => "ENOENT".to_string(),
                };
                format!("DIRTREE:{} {}", encode(path.to_string_lossy().as_ref()), tail)
            }
            RepoRecordedInput::EnvVar { name, value } => {
                let tail = match value {
                    Some(v) => encode(v),
                    None => "ENOENT".to_string(),
                };
                format!("ENV:{} {}", encode(name), tail)
            }
            RepoRecordedInput::RepoMapping {
                source_repo,
                apparent,
                canonical,
            } => {
                format!(
                    "REPO_MAPPING:{},{} {}",
                    encode(source_repo),
                    encode(apparent),
                    encode(canonical)
                )
            }
        }
    }

    pub fn from_marker_line(line: &str) -> Result<Self, RecordParseError> {
        let (prefix, rest) = line
            .split_once(':')
            .ok_or_else(|| RecordParseError::Malformed(line.to_string()))?;
        match prefix {
            "FILE" => parse_file(rest),
            "DIRENTS" => parse_dirents(rest),
            "DIRTREE" => parse_dirtree(rest),
            "ENV" => parse_env(rest),
            "REPO_MAPPING" => parse_repo_mapping(rest),
            other => Err(RecordParseError::UnknownPrefix(other.to_string())),
        }
    }

    /// Observe the current state of whatever this record refers to,
    /// using the provided env map for `EnvVar` lookups. For
    /// `RepoMapping`, this returns the record unchanged — repo-mapping
    /// invalidation is handled at engine-build time when the mapping
    /// table changes, not at re-observe time.
    pub fn reobserve(&self, declared_env: &BTreeMap<String, String>) -> RepoRecordedInput {
        match self {
            RepoRecordedInput::File { path, .. } => RepoRecordedInput::File {
                path: path.clone(),
                value: observe_file(path),
            },
            RepoRecordedInput::Dirents { path, .. } => RepoRecordedInput::Dirents {
                path: path.clone(),
                value: observe_dirents(path),
            },
            RepoRecordedInput::DirTree { path, .. } => RepoRecordedInput::DirTree {
                path: path.clone(),
                value: observe_dirtree(path),
            },
            RepoRecordedInput::EnvVar { name, .. } => RepoRecordedInput::EnvVar {
                name: name.clone(),
                value: declared_env.get(name).cloned(),
            },
            RepoRecordedInput::RepoMapping { .. } => self.clone(),
        }
    }
}

pub fn observe_file(path: &Path) -> FileValue {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return FileValue::Missing,
    };
    if meta.is_dir() {
        return FileValue::Dir;
    }
    match fs::read(path) {
        Ok(bytes) => {
            let mut h = Sha256::new();
            h.update(&bytes);
            FileValue::Digest(hex_encode(h.finalize()))
        }
        Err(_) => FileValue::Missing,
    }
}

pub fn observe_dirents(path: &Path) -> DirentsValue {
    let read = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => return DirentsValue::Missing,
    };
    let mut names: Vec<String> = read
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut h = Sha256::new();
    for n in &names {
        h.update(n.as_bytes());
        h.update(b"\0");
    }
    DirentsValue::Fingerprint(hex_encode(h.finalize()))
}

pub fn observe_dirtree(path: &Path) -> DirTreeValue {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return DirTreeValue::Missing,
    };
    if !meta.is_dir() {
        return DirTreeValue::Missing;
    }
    let mut entries: Vec<(String, FileValue)> = Vec::new();
    if let Err(_e) = walk_tree(path, path, &mut entries) {
        return DirTreeValue::Missing;
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    for (rel, val) in &entries {
        h.update(rel.as_bytes());
        h.update(b"\0");
        match val {
            FileValue::Digest(d) => h.update(d.as_bytes()),
            FileValue::Dir => h.update(b"DIR"),
            FileValue::Missing => h.update(b"ENOENT"),
        }
        h.update(b"\n");
    }
    DirTreeValue::Digest(hex_encode(h.finalize()))
}

fn walk_tree(root: &Path, dir: &Path, out: &mut Vec<(String, FileValue)>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned();
        let meta = fs::symlink_metadata(&p)?;
        if meta.is_dir() {
            out.push((rel.clone(), FileValue::Dir));
            walk_tree(root, &p, out)?;
        } else {
            out.push((rel, observe_file(&p)));
        }
    }
    Ok(())
}

fn parse_file(rest: &str) -> Result<RepoRecordedInput, RecordParseError> {
    let (encoded_path, tail) = rest
        .rsplit_once(' ')
        .ok_or_else(|| RecordParseError::Malformed(rest.to_string()))?;
    let path = PathBuf::from(decode(encoded_path));
    let value = match tail {
        "DIR" => FileValue::Dir,
        "ENOENT" => FileValue::Missing,
        digest => FileValue::Digest(digest.to_string()),
    };
    Ok(RepoRecordedInput::File { path, value })
}

fn parse_dirents(rest: &str) -> Result<RepoRecordedInput, RecordParseError> {
    let (encoded_path, tail) = rest
        .rsplit_once(' ')
        .ok_or_else(|| RecordParseError::Malformed(rest.to_string()))?;
    let path = PathBuf::from(decode(encoded_path));
    let value = match tail {
        "ENOENT" => DirentsValue::Missing,
        fp => DirentsValue::Fingerprint(fp.to_string()),
    };
    Ok(RepoRecordedInput::Dirents { path, value })
}

fn parse_dirtree(rest: &str) -> Result<RepoRecordedInput, RecordParseError> {
    let (encoded_path, tail) = rest
        .rsplit_once(' ')
        .ok_or_else(|| RecordParseError::Malformed(rest.to_string()))?;
    let path = PathBuf::from(decode(encoded_path));
    let value = match tail {
        "ENOENT" => DirTreeValue::Missing,
        d => DirTreeValue::Digest(d.to_string()),
    };
    Ok(RepoRecordedInput::DirTree { path, value })
}

fn parse_env(rest: &str) -> Result<RepoRecordedInput, RecordParseError> {
    let (encoded_name, tail) = rest
        .split_once(' ')
        .ok_or_else(|| RecordParseError::Malformed(rest.to_string()))?;
    let name = decode(encoded_name);
    let value = if tail == "ENOENT" {
        None
    } else {
        Some(decode(tail))
    };
    Ok(RepoRecordedInput::EnvVar { name, value })
}

fn parse_repo_mapping(rest: &str) -> Result<RepoRecordedInput, RecordParseError> {
    let (head, encoded_canonical) = rest
        .split_once(' ')
        .ok_or_else(|| RecordParseError::Malformed(rest.to_string()))?;
    let (encoded_src, encoded_apparent) = head
        .split_once(',')
        .ok_or_else(|| RecordParseError::Malformed(rest.to_string()))?;
    Ok(RepoRecordedInput::RepoMapping {
        source_repo: decode(encoded_src),
        apparent: decode(encoded_apparent),
        canonical: decode(encoded_canonical),
    })
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b' ' => out.push_str("%20"),
            b'\n' => out.push_str("%0A"),
            b'\r' => out.push_str("%0D"),
            b',' => out.push_str("%2C"),
            b'%' => out.push_str("%25"),
            _ => out.push(b as char),
        }
    }
    out
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi << 4) | lo) as u8 as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn file_round_trip() {
        let r = RepoRecordedInput::File {
            path: PathBuf::from("/a path/with space"),
            value: FileValue::Digest("abc123".to_string()),
        };
        let line = r.to_marker_line();
        assert_eq!(line, "FILE:/a%20path/with%20space abc123");
        let parsed = RepoRecordedInput::from_marker_line(&line).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn file_dir_and_missing() {
        for v in [FileValue::Dir, FileValue::Missing] {
            let r = RepoRecordedInput::File {
                path: PathBuf::from("/x"),
                value: v.clone(),
            };
            let parsed = RepoRecordedInput::from_marker_line(&r.to_marker_line()).unwrap();
            assert_eq!(parsed, r);
        }
    }

    #[test]
    fn env_round_trip_set_and_unset() {
        let set = RepoRecordedInput::EnvVar {
            name: "FOO BAR".to_string(),
            value: Some("hello world".to_string()),
        };
        let parsed = RepoRecordedInput::from_marker_line(&set.to_marker_line()).unwrap();
        assert_eq!(parsed, set);

        let unset = RepoRecordedInput::EnvVar {
            name: "FOO".to_string(),
            value: None,
        };
        let parsed = RepoRecordedInput::from_marker_line(&unset.to_marker_line()).unwrap();
        assert_eq!(parsed, unset);
    }

    #[test]
    fn repo_mapping_round_trip() {
        let r = RepoRecordedInput::RepoMapping {
            source_repo: "@@main".to_string(),
            apparent: "io_bazel_rules_go".to_string(),
            canonical: "rules_go~0.41.0".to_string(),
        };
        let parsed = RepoRecordedInput::from_marker_line(&r.to_marker_line()).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn observe_file_round_trip_on_real_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        fs::write(&p, b"hello").unwrap();
        let v = observe_file(&p);
        let expected_digest = {
            let mut h = Sha256::new();
            h.update(b"hello");
            hex_encode(h.finalize())
        };
        assert_eq!(v, FileValue::Digest(expected_digest));
    }

    #[test]
    fn observe_file_missing_is_missing() {
        assert_eq!(observe_file(Path::new("/no/such/path/loupe-test")), FileValue::Missing);
    }

    #[test]
    fn observe_dirents_is_stable_over_entry_order() {
        let dir = tempdir().unwrap();
        for name in ["a", "c", "b"] {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        let v1 = observe_dirents(dir.path());
        let v2 = observe_dirents(dir.path());
        assert_eq!(v1, v2);
        assert!(matches!(v1, DirentsValue::Fingerprint(_)));
    }

    #[test]
    fn observe_dirtree_changes_on_content_edit() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a"), b"1").unwrap();
        let v1 = observe_dirtree(dir.path());
        fs::write(dir.path().join("a"), b"2").unwrap();
        let v2 = observe_dirtree(dir.path());
        assert_ne!(v1, v2);
    }

    #[test]
    fn reobserve_env_uses_declared_env() {
        let mut env = BTreeMap::new();
        env.insert("FOO".to_string(), "fresh".to_string());
        let stale = RepoRecordedInput::EnvVar {
            name: "FOO".to_string(),
            value: Some("stale".to_string()),
        };
        let fresh = stale.reobserve(&env);
        assert_eq!(
            fresh,
            RepoRecordedInput::EnvVar {
                name: "FOO".to_string(),
                value: Some("fresh".to_string()),
            }
        );
    }
}
