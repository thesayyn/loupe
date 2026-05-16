//! Scan `MODULE.bazel.lock` for `generatedRepoSpecs` payloads.
//!
//! Distinct from `loupe-core::lockfile` which extracts only repo
//! mappings (`recordedInputs`, `recordedRepoMappingEntries`). Two
//! readers, same file, different concerns — keeps loupe-core free of
//! the archive/HTTP-shaped types this crate owns.
//!
//! Canonical naming for extension-generated repos mirrors the
//! convention loupe-core's `lockfile.rs` already detects:
//! `<owner_canonical><sep><extension><sep><repo>` where `<sep>` is
//! whichever character (`+` Bazel 7+, `~` older) appears in extension
//! owner canonicals.

use std::collections::BTreeMap;
use std::path::Path;

use crate::repo_spec::{RepoSpec, RepoSpecParseError, parse_generated_repo_spec};

#[derive(Debug, thiserror::Error)]
pub enum LoadSpecsError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },

    #[error(transparent)]
    Spec(#[from] RepoSpecParseError),
}

/// Load every `generatedRepoSpecs` entry from `<workspace>/MODULE.bazel.lock`,
/// keyed by canonical repo name. Missing lockfile returns an empty map.
pub fn load_from_workspace(workspace_root: &Path) -> Result<BTreeMap<String, RepoSpec>, LoadSpecsError> {
    load_from_path(&workspace_root.join("MODULE.bazel.lock"))
}

pub fn load_from_path(path: &Path) -> Result<BTreeMap<String, RepoSpec>, LoadSpecsError> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let src = std::fs::read_to_string(path).map_err(|e| LoadSpecsError::Read {
        path: path.display().to_string(),
        source: e,
    })?;
    parse_str(&src).map_err(|e| match e {
        ParseSpecsError::Json(source) => LoadSpecsError::Parse {
            path: path.display().to_string(),
            source,
        },
        ParseSpecsError::Spec(s) => LoadSpecsError::Spec(s),
    })
}

#[derive(Debug)]
enum ParseSpecsError {
    Json(serde_json::Error),
    Spec(RepoSpecParseError),
}

impl From<serde_json::Error> for ParseSpecsError {
    fn from(e: serde_json::Error) -> Self {
        ParseSpecsError::Json(e)
    }
}
impl From<RepoSpecParseError> for ParseSpecsError {
    fn from(e: RepoSpecParseError) -> Self {
        ParseSpecsError::Spec(e)
    }
}

fn parse_str(src: &str) -> Result<BTreeMap<String, RepoSpec>, ParseSpecsError> {
    let v: serde_json::Value = serde_json::from_str(src)?;
    let mut out: BTreeMap<String, RepoSpec> = BTreeMap::new();

    let extensions = v
        .get("moduleExtensions")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    // Detect canonical separator: any owner canonical ending in `~`
    // signals the older Bazel 7-era convention; otherwise `+`.
    let sep = detect_separator(&extensions);

    for (ext_id, ext_node) in extensions.iter() {
        let Some((owner_canonical, extension_name)) = parse_extension_id(ext_id) else {
            continue;
        };
        let general = ext_node.get("general").unwrap_or(ext_node);
        let Some(specs) = general.get("generatedRepoSpecs").and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        for (repo_name, spec_node) in specs {
            let canonical = format!("{owner_canonical}{sep}{extension_name}{sep}{repo_name}");
            let spec = parse_generated_repo_spec(&canonical, spec_node)?;
            out.insert(canonical, spec);
        }
    }
    Ok(out)
}

fn detect_separator(extensions: &serde_json::Map<String, serde_json::Value>) -> char {
    for ext_id in extensions.keys() {
        if let Some(rest) = ext_id.strip_prefix("@@")
            && let Some(owner) = rest.split("//").next()
            && owner.ends_with('~')
        {
            return '~';
        }
    }
    '+'
}

/// Parse `@@<owner_canonical>//<path>:<file>%<extension_name>` →
/// `(owner_canonical, extension_name)`.
fn parse_extension_id(id: &str) -> Option<(&str, &str)> {
    let body = id.strip_prefix("@@")?;
    let (owner_with_path, ext_name) = body.rsplit_once('%')?;
    let owner = owner_with_path.split_once("//").map(|(r, _)| r).unwrap_or(owner_with_path);
    Some((owner, ext_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_spec::RepoSpecKind;

    #[test]
    fn empty_lockfile() {
        let m = parse_str(r#"{"lockFileVersion": 26}"#).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn extracts_extension_generated_http_archive() {
        let src = r#"{
          "moduleExtensions": {
            "@@bazel_features+//private:extensions.bzl%version_extension": {
              "general": {
                "generatedRepoSpecs": {
                  "bazel_features_globals": {
                    "ruleClassName": "http_archive",
                    "attributes": {
                      "urls": ["https://example.com/x.tar.gz"],
                      "integrity": "sha256-abc",
                      "strip_prefix": "x-1.0"
                    }
                  }
                }
              }
            }
          }
        }"#;
        let m = parse_str(src).unwrap();
        let canonical = "bazel_features++version_extension+bazel_features_globals";
        let spec = m.get(canonical).expect("spec exists under canonical name");
        match &spec.kind {
            RepoSpecKind::Http { urls, integrity, strip_prefix, .. } => {
                assert_eq!(urls, &vec!["https://example.com/x.tar.gz".to_string()]);
                assert_eq!(integrity.as_deref(), Some("sha256-abc"));
                assert_eq!(strip_prefix.as_deref(), Some("x-1.0"));
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn detects_tilde_separator() {
        let src = r#"{
          "moduleExtensions": {
            "@@bazel_tools~//tools:foo%bar": {
              "general": {
                "generatedRepoSpecs": {
                  "baz": {
                    "ruleClassName": "http_archive",
                    "attributes": { "url": "https://x/y.zip" }
                  }
                }
              }
            }
          }
        }"#;
        let m = parse_str(src).unwrap();
        assert!(m.contains_key("bazel_tools~~bar~baz"));
    }

    #[test]
    fn missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let m = load_from_workspace(tmp.path()).unwrap();
        assert!(m.is_empty());
    }
}
