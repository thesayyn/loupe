//! Typed `RepoSpec` — how a single external repo gets materialised.
//!
//! `Http` is fully implemented today. `Git`, `LocalPath`, and
//! `StarlarkRule` parse but their `materialize()` paths raise — they
//! land in later commits per `~/.claude/plans/write-up-a-plan-lexical-papert.md`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use loupe_labels::RepoMapping;
use starlark::environment::FrozenModule;
use starlark::values::FrozenValue;

use crate::archive::ArchiveKind;
use crate::starlark_eval::RepoAttrValue;

#[derive(Debug, Clone)]
pub struct RepoSpec {
    pub canonical: String,
    pub kind: RepoSpecKind,
}

impl PartialEq for RepoSpec {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical && self.kind == other.kind
    }
}
impl Eq for RepoSpec {}

// `RepoSpec` lives behind `Arc` in DICE values. Real memory accounting
// would require descending into every variant's `Arc<FrozenModule>` and
// the cached `Url`/`Path` strings — not worth the maintenance burden
// while DICE's allocator visibility is informational only.
impl allocative::Allocative for RepoSpec {
    fn visit<'a, 'b: 'a>(&self, _visitor: &'a mut allocative::Visitor<'b>) {}
}

#[derive(Debug, Clone)]
pub enum RepoSpecKind {
    /// `http_archive`-shaped: download from one of `urls`, verify, extract.
    Http {
        urls: Vec<String>,
        /// SRI integrity string (e.g. `sha256-…`). Optional but
        /// strongly recommended — bzlmod-generated specs always carry it.
        integrity: Option<String>,
        /// Bazel-exact prefix to strip from every entry.
        strip_prefix: Option<String>,
        /// Explicit archive kind. When `None`, detect from the URL suffix.
        archive_kind: Option<ArchiveKind>,
    },
    /// Parsed for future support; current `materialize()` raises.
    Git {
        remote: String,
        commit: String,
        strip_prefix: Option<String>,
    },
    /// Parsed for future support; current `materialize()` raises.
    LocalPath { path: PathBuf },
    /// Materialise by running a Starlark `repository_rule` impl. The
    /// `rule_fv` is a `FrozenValue` pointing at a `FrozenRepositoryRule`
    /// in `module`'s frozen heap; the `Arc<FrozenModule>` keeps that
    /// heap pinned for the lifetime of this spec. `attrs` is the
    /// kwargs the rule was invoked with — captured during
    /// module-extension eval (or by a test helper).
    ///
    /// `rule_source_digest` is the hex SHA-256 of the `.bzl` file
    /// that defined the rule (`my_rule = repository_rule(...)`). It
    /// enters the marker file's `predeclaredInputHash` so an edit to
    /// the defining `.bzl` busts every cached materialisation that
    /// referenced it.
    StarlarkRule {
        module: Arc<FrozenModule>,
        rule_fv: FrozenValue,
        attrs: BTreeMap<String, RepoAttrValue>,
        rule_source_digest: Arc<String>,
        /// `apparent → canonical` for the rule's defining `.bzl`. Used
        /// by [`ctx.path(Label(...))`] to resolve apparent labels into
        /// canonical repo names before dispatching the DICE
        /// materialisation. Empty for repo rules that don't use labels.
        /// The mapping's `owner` carries the defining `.bzl`'s repo so
        /// `Label("@//pkg:x")` self-refs to the right canonical.
        repo_mapping: Arc<RepoMapping>,
    },
}

impl PartialEq for RepoSpecKind {
    fn eq(&self, other: &Self) -> bool {
        use RepoSpecKind::*;
        match (self, other) {
            (
                Http {
                    urls: a,
                    integrity: b,
                    strip_prefix: c,
                    archive_kind: d,
                },
                Http {
                    urls: a2,
                    integrity: b2,
                    strip_prefix: c2,
                    archive_kind: d2,
                },
            ) => a == a2 && b == b2 && c == c2 && d == d2,
            (
                Git {
                    remote: a,
                    commit: b,
                    strip_prefix: c,
                },
                Git {
                    remote: a2,
                    commit: b2,
                    strip_prefix: c2,
                },
            ) => a == a2 && b == b2 && c == c2,
            (LocalPath { path: a }, LocalPath { path: b }) => a == b,
            // StarlarkRule equality is pointer-identity on module +
            // FrozenValue ptr bits plus attr equality. Good enough for
            // DICE invalidation; a recompute produces a fresh module so
            // the pointer comparison invalidates downstream correctly.
            (
                StarlarkRule {
                    module: m1,
                    rule_fv: r1,
                    attrs: a1,
                    rule_source_digest: d1,
                    repo_mapping: rm1,
                },
                StarlarkRule {
                    module: m2,
                    rule_fv: r2,
                    attrs: a2,
                    rule_source_digest: d2,
                    repo_mapping: rm2,
                },
            ) => {
                Arc::ptr_eq(m1, m2)
                    && r1.to_value().ptr_eq(r2.to_value())
                    && a1 == a2
                    && d1 == d2
                    && rm1 == rm2
            }
            _ => false,
        }
    }
}
impl Eq for RepoSpecKind {}

#[derive(Debug, thiserror::Error)]
pub enum RepoSpecParseError {
    #[error("missing required attribute `{0}` in repo spec for `{repo}`", repo = .1)]
    MissingAttr(&'static str, String),

    #[error("attribute `{0}` in repo spec for `{repo}` has unexpected type", repo = .1)]
    BadAttrType(&'static str, String),

    #[error("unknown rule class `{class}` for repo `{repo}`")]
    UnknownRuleClass { class: String, repo: String },
}

/// Decode a single `generatedRepoSpecs.<repo>` JSON node into a `RepoSpec`.
///
/// Bazel writes this shape for module-extension-generated `http_archive`s
/// and friends:
///
/// ```json
/// {
///   "bzlFile": "@@bazel_tools//tools/build_defs/repo:http.bzl",
///   "ruleClassName": "http_archive",
///   "attributes": {
///     "name": "platforms",
///     "urls": ["https://github.com/.../platforms.tar.gz"],
///     "integrity": "sha256-…",
///     "strip_prefix": "platforms-0.0.10"
///   }
/// }
/// ```
pub fn parse_generated_repo_spec(
    canonical: &str,
    node: &serde_json::Value,
) -> Result<RepoSpec, RepoSpecParseError> {
    let class = node
        .get("ruleClassName")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RepoSpecParseError::MissingAttr("ruleClassName", canonical.to_string()))?;
    let attrs = node
        .get("attributes")
        .ok_or_else(|| RepoSpecParseError::MissingAttr("attributes", canonical.to_string()))?;

    let kind = match class {
        "http_archive" | "_http_archive_impl" => http_archive_from_attrs(canonical, attrs)?,
        "git_repository" | "new_git_repository" => git_from_attrs(canonical, attrs)?,
        "local_repository" | "new_local_repository" => local_from_attrs(canonical, attrs)?,
        other => {
            return Err(RepoSpecParseError::UnknownRuleClass {
                class: other.to_string(),
                repo: canonical.to_string(),
            });
        }
    };

    Ok(RepoSpec {
        canonical: canonical.to_string(),
        kind,
    })
}

fn http_archive_from_attrs(
    repo: &str,
    attrs: &serde_json::Value,
) -> Result<RepoSpecKind, RepoSpecParseError> {
    let urls = if let Some(arr) = attrs.get("urls").and_then(|v| v.as_array()) {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect::<Vec<_>>()
    } else if let Some(url) = attrs.get("url").and_then(|v| v.as_str()) {
        vec![url.to_string()]
    } else {
        return Err(RepoSpecParseError::MissingAttr("urls", repo.to_string()));
    };

    let integrity = attrs
        .get("integrity")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let strip_prefix = attrs
        .get("strip_prefix")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let archive_kind = attrs
        .get("type")
        .and_then(|v| v.as_str())
        .and_then(archive_kind_from_explicit);

    Ok(RepoSpecKind::Http {
        urls,
        integrity,
        strip_prefix,
        archive_kind,
    })
}

fn archive_kind_from_explicit(s: &str) -> Option<ArchiveKind> {
    match s.to_ascii_lowercase().as_str() {
        "tar.gz" | "tgz" => Some(ArchiveKind::TarGz),
        "tar.xz" | "txz" => Some(ArchiveKind::TarXz),
        "tar.zst" | "tar.zstd" => Some(ArchiveKind::TarZst),
        "tar" => Some(ArchiveKind::Tar),
        "zip" | "jar" => Some(ArchiveKind::Zip),
        _ => None,
    }
}

fn git_from_attrs(
    repo: &str,
    attrs: &serde_json::Value,
) -> Result<RepoSpecKind, RepoSpecParseError> {
    let remote = attrs
        .get("remote")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RepoSpecParseError::MissingAttr("remote", repo.to_string()))?
        .to_string();
    let commit = attrs
        .get("commit")
        .or_else(|| attrs.get("tag"))
        .or_else(|| attrs.get("branch"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| RepoSpecParseError::MissingAttr("commit", repo.to_string()))?
        .to_string();
    let strip_prefix = attrs
        .get("strip_prefix")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(RepoSpecKind::Git {
        remote,
        commit,
        strip_prefix,
    })
}

fn local_from_attrs(
    repo: &str,
    attrs: &serde_json::Value,
) -> Result<RepoSpecKind, RepoSpecParseError> {
    let path = attrs
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RepoSpecParseError::MissingAttr("path", repo.to_string()))?;
    Ok(RepoSpecKind::LocalPath {
        path: PathBuf::from(path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_archive() {
        let node = serde_json::json!({
            "ruleClassName": "http_archive",
            "attributes": {
                "name": "platforms",
                "urls": ["https://example.com/platforms.tar.gz"],
                "integrity": "sha256-abc",
                "strip_prefix": "platforms-0.0.10"
            }
        });
        let spec = parse_generated_repo_spec("platforms", &node).unwrap();
        assert_eq!(spec.canonical, "platforms");
        match spec.kind {
            RepoSpecKind::Http {
                urls,
                integrity,
                strip_prefix,
                archive_kind,
            } => {
                assert_eq!(urls, vec!["https://example.com/platforms.tar.gz"]);
                assert_eq!(integrity.as_deref(), Some("sha256-abc"));
                assert_eq!(strip_prefix.as_deref(), Some("platforms-0.0.10"));
                assert_eq!(archive_kind, None);
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn parses_http_archive_single_url() {
        let node = serde_json::json!({
            "ruleClassName": "http_archive",
            "attributes": { "url": "https://example.com/a.zip" }
        });
        let spec = parse_generated_repo_spec("x", &node).unwrap();
        match spec.kind {
            RepoSpecKind::Http { urls, .. } => {
                assert_eq!(urls, vec!["https://example.com/a.zip"]);
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn parses_explicit_archive_type() {
        let node = serde_json::json!({
            "ruleClassName": "http_archive",
            "attributes": { "url": "https://example.com/noext", "type": "tar.gz" }
        });
        let spec = parse_generated_repo_spec("x", &node).unwrap();
        match spec.kind {
            RepoSpecKind::Http { archive_kind, .. } => {
                assert_eq!(archive_kind, Some(ArchiveKind::TarGz));
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn unknown_rule_class_errors() {
        let node = serde_json::json!({
            "ruleClassName": "exotic_rule",
            "attributes": {}
        });
        let r = parse_generated_repo_spec("x", &node);
        assert!(matches!(
            r,
            Err(RepoSpecParseError::UnknownRuleClass { .. })
        ));
    }

    #[test]
    fn missing_urls_errors() {
        let node = serde_json::json!({
            "ruleClassName": "http_archive",
            "attributes": {}
        });
        let r = parse_generated_repo_spec("x", &node);
        assert!(matches!(r, Err(RepoSpecParseError::MissingAttr("urls", _))));
    }
}
