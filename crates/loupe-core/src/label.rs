//! Bazel labels with canonical-form identity.
//!
//! Per `DESIGN.md`'s non-negotiable invariants:
//! > `Label` is a real type, not a string. Hash and equality by canonical
//! > form. Methods for relative resolution. Display in canonical form.
//!
//! v1 only models a slice of Bazel's label space: bzlmod-style canonical
//! repos, no relative-package operators (`..`, repo-relative `//`),
//! and no apparent-name conversion (Phase 2 doesn't have a real repo
//! mapping yet — Phase 3+ adds it).

use std::fmt;

/// A canonical repo name. The empty string is the main module.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, allocative::Allocative, pagable::Pagable,
)]
pub struct CanonicalRepo(String);

impl CanonicalRepo {
    pub const MAIN: CanonicalRepo = CanonicalRepo(String::new());

    pub fn new(name: impl Into<String>) -> Self {
        CanonicalRepo(name.into())
    }

    pub fn is_main(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CanonicalRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@@{}", self.0)
    }
}

/// A package path within a repo, e.g. `""` for the top-level package
/// and `"foo/bar"` for `//foo/bar`.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, allocative::Allocative, pagable::Pagable,
)]
pub struct PackagePath(String);

impl PackagePath {
    pub fn root() -> Self {
        PackagePath(String::new())
    }

    pub fn new(path: impl Into<String>) -> Result<Self, ParseLabelError> {
        let s: String = path.into();
        if s.starts_with('/')
            || s.ends_with('/')
            || s.contains("//")
            || s.contains(':')
            || s.contains('\\')
        {
            return Err(ParseLabelError::BadPackagePath(s));
        }
        Ok(PackagePath(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for PackagePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `(repo, package_path)` — identifies a package without a target name.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, allocative::Allocative, pagable::Pagable,
)]
pub struct PackageId {
    pub repo: CanonicalRepo,
    pub path: PackagePath,
}

impl PackageId {
    pub fn new(repo: CanonicalRepo, path: PackagePath) -> Self {
        PackageId { repo, path }
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}//{}", self.repo, self.path)
    }
}

/// A fully-qualified, canonical Bazel label.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, allocative::Allocative, pagable::Pagable,
)]
pub struct Label {
    pub package: PackageId,
    pub name: String,
}

impl Label {
    pub fn new(repo: CanonicalRepo, path: PackagePath, name: impl Into<String>) -> Self {
        Label {
            package: PackageId::new(repo, path),
            name: name.into(),
        }
    }

    pub fn in_package(package: PackageId, name: impl Into<String>) -> Self {
        Label {
            package,
            name: name.into(),
        }
    }

    pub fn repo(&self) -> &CanonicalRepo {
        &self.package.repo
    }

    pub fn package_path(&self) -> &PackagePath {
        &self.package.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Resolve a label written in a BUILD/`.bzl` file (apparent or
    /// repo-relative) against the file's current package and the
    /// active repo mapping. Phase 3 supports the slice fixtures need:
    ///
    /// - `:foo` and `:foo.bzl`        → `@@<current_repo>//<current_pkg>:foo`
    /// - `//pkg:name` / `//pkg/sub`   → `@@<current_repo>//pkg:name`
    /// - `@apparent//pkg:name`        → resolved via `repo_mapping`
    /// - `@@canonical//pkg:name`      → already canonical, parsed
    ///
    /// Apparent names that miss the mapping fail with
    /// `ParseLabelError::UnknownApparentRepo`.
    pub fn resolve(
        s: &str,
        current_package: &PackageId,
        repo_mapping: &std::collections::BTreeMap<String, CanonicalRepo>,
    ) -> Result<Self, ParseLabelError> {
        if let Some(rest) = s.strip_prefix("@@") {
            return Label::parse_canonical(&format!("@@{rest}"));
        }
        if let Some(rest) = s.strip_prefix('@') {
            // `@apparent//pkg:name`
            let sep = rest
                .find("//")
                .ok_or_else(|| ParseLabelError::Bad(s.to_string()))?;
            let apparent = &rest[..sep];
            let canonical = repo_mapping
                .get(apparent)
                .cloned()
                .ok_or_else(|| ParseLabelError::UnknownApparentRepo(apparent.to_string()))?;
            let canonical_str = format!("@@{}//{}", canonical.as_str(), &rest[sep + 2..]);
            return Label::parse_canonical(&canonical_str);
        }
        if let Some(rest) = s.strip_prefix("//") {
            let canonical_str = format!("@@{}//{}", current_package.repo.as_str(), rest);
            return Label::parse_canonical(&canonical_str);
        }
        if let Some(name) = s.strip_prefix(':') {
            return Ok(Label::new(
                current_package.repo.clone(),
                current_package.path.clone(),
                name,
            ));
        }
        Err(ParseLabelError::Bad(s.to_string()))
    }

    /// Parse a canonical label like `@@//pkg:name` or `@@canonical//:name`.
    pub fn parse_canonical(s: &str) -> Result<Self, ParseLabelError> {
        let s = s
            .strip_prefix("@@")
            .ok_or_else(|| ParseLabelError::Bad(s.to_string()))?;
        let sep = s
            .find("//")
            .ok_or_else(|| ParseLabelError::Bad(s.to_string()))?;
        let repo = &s[..sep];
        let rest = &s[sep + 2..];
        let (pkg, name) = match rest.find(':') {
            Some(i) => (&rest[..i], rest[i + 1..].to_string()),
            None => {
                let derived = rest
                    .rsplit('/')
                    .next()
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| ParseLabelError::Bad(rest.to_string()))?;
                (rest, derived.to_string())
            }
        };
        Ok(Label::new(
            CanonicalRepo::new(repo),
            PackagePath::new(pkg)?,
            name,
        ))
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.package, self.name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseLabelError {
    #[error("malformed label: {0}")]
    Bad(String),
    #[error("malformed package path: {0}")]
    BadPackagePath(String),
    #[error("unknown apparent repo `{0}` (not in module-graph repo mapping)")]
    UnknownApparentRepo(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_repo_label_displays_canonically() {
        let l = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "world");
        assert_eq!(l.to_string(), "@@//:world");
    }

    #[test]
    fn nested_package_displays_with_path() {
        let l = Label::new(
            CanonicalRepo::MAIN,
            PackagePath::new("foo/bar").unwrap(),
            "lib",
        );
        assert_eq!(l.to_string(), "@@//foo/bar:lib");
    }

    #[test]
    fn parse_canonical_main_repo() {
        let l = Label::parse_canonical("@@//:world").unwrap();
        assert_eq!(l.repo(), &CanonicalRepo::MAIN);
        assert_eq!(l.package_path().as_str(), "");
        assert_eq!(l.name(), "world");
    }

    #[test]
    fn parse_canonical_with_repo_and_package() {
        let l = Label::parse_canonical("@@rules_python//pkg/sub:lib").unwrap();
        assert_eq!(l.repo().as_str(), "rules_python");
        assert_eq!(l.package_path().as_str(), "pkg/sub");
        assert_eq!(l.name(), "lib");
    }

    #[test]
    fn parse_canonical_implicit_name_from_package() {
        // `@@//foo/bar` is shorthand for `@@//foo/bar:bar`.
        let l = Label::parse_canonical("@@//foo/bar").unwrap();
        assert_eq!(l.name(), "bar");
    }

    #[test]
    fn round_trip_parse_display() {
        for s in ["@@//:world", "@@//foo:lib", "@@dep+//pkg/sub:tgt"] {
            let l = Label::parse_canonical(s).unwrap();
            assert_eq!(l.to_string(), s);
        }
    }

    #[test]
    fn rejects_bad_package_paths() {
        assert!(PackagePath::new("/leading").is_err());
        assert!(PackagePath::new("trailing/").is_err());
        assert!(PackagePath::new("double//slash").is_err());
        assert!(PackagePath::new("with:colon").is_err());
    }

    #[test]
    fn rejects_bad_labels() {
        assert!(Label::parse_canonical("//:world").is_err()); // missing @@
        assert!(Label::parse_canonical("@@//").is_err()); // empty package, no name
    }

    #[test]
    fn equality_uses_canonical_form() {
        let a = Label::parse_canonical("@@//:world").unwrap();
        let b = Label::new(CanonicalRepo::MAIN, PackagePath::root(), "world");
        assert_eq!(a, b);
    }
}
