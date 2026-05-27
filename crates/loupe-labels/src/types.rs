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

use crate::repo_mapping::RepoMapping;

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
    /// active repo mapping:
    ///
    /// - `:foo` and `:foo.bzl`        → `@@<current_repo>//<current_pkg>:foo`
    /// - `//pkg:name` / `//pkg/sub`   → `@@<current_repo>//pkg:name`
    /// - `@apparent//pkg:name`        → resolved via `mapping`
    /// - `@//pkg:name`                → resolved to `mapping.owner` (self-ref)
    /// - `@@canonical//pkg:name`      → already canonical, parsed
    ///
    /// Apparent names that aren't in `mapping` raise
    /// [`ParseLabelError::UnknownApparentRepo`]. The small set of
    /// repos Bazel hardcodes (`bazel_tools`, `platforms`,
    /// `local_config_platform`) plus Loupe's `_builtins` virtual repo
    /// resolve to their bare names without a mapping entry.
    pub fn resolve(
        s: &str,
        current_package: &PackageId,
        mapping: &RepoMapping,
    ) -> Result<Self, ParseLabelError> {
        if let Some(rest) = s.strip_prefix("@@") {
            return Label::parse_canonical(&format!("@@{rest}"));
        }
        if let Some(rest) = s.strip_prefix('@') {
            // `@apparent//pkg:name` (or `@//pkg:name` for self-ref).
            // Bazel shorthand: a bare `@apparent` with no `//` means
            // `@apparent//:apparent` — the target named after the repo
            // in the repo's root package. rules_js's root `tools/BUILD`
            // does `load("@bazelrc-preset.bzl", "bazelrc_preset")`,
            // loading the `bazelrc-preset.bzl` file from that repo's
            // root.
            let (apparent, after_sep) = match rest.find("//") {
                Some(sep) => (&rest[..sep], rest[sep + 2..].to_string()),
                None => (rest, format!(":{rest}")),
            };
            let canonical = match mapping.resolve_apparent(apparent) {
                Some(c) => c,
                None => match native_well_known(apparent) {
                    Some(c) => c,
                    None => return Err(ParseLabelError::UnknownApparentRepo(apparent.to_string())),
                },
            };
            let canonical_str = format!("@@{}//{}", canonical.as_str(), after_sep);
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
        // Same-package shorthand. Bazel treats any string without
        // `@` / `//` prefix as a target-name in the current package —
        // including source-file labels with `/` separators or
        // embedded `@` (`bin/nodejs/lib/node_modules/@scoped/pkg/file`
        // is the same as `:bin/nodejs/lib/node_modules/@scoped/...`).
        // Only reject the unambiguous junk: leading `/`, embedded `:`,
        // and `@` at the very start (already handled above).
        if !s.contains(':') && !s.starts_with('/') {
            return Ok(Label::new(
                current_package.repo.clone(),
                current_package.path.clone(),
                s,
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

/// Native repos Bazel hardcodes (`bazel_tools`, `platforms`,
/// `local_config_platform`) plus Loupe's `_builtins` virtual repo
/// keep their apparent name as canonical with no separator suffix.
/// They never need a mapping entry. Every other apparent name MUST
/// resolve through the repo mapping — there is no fallback.
fn native_well_known(apparent: &str) -> Option<CanonicalRepo> {
    match apparent {
        "bazel_tools" | "platforms" | "local_config_platform" | "_builtins" => {
            Some(CanonicalRepo::new(apparent))
        }
        _ => None,
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
    fn bare_apparent_repo_resolves_to_root_target_named_after_repo() {
        // Bazel shorthand: `@repo` with no `//` means `@repo//:repo` —
        // the target named after the repo in its root package. rules_js's
        // root `tools/BUILD` does `load("@bazelrc-preset.bzl", ...)`.
        let mapping = RepoMapping::from_iter([(
            "bazelrc-preset.bzl",
            CanonicalRepo::new("rules_x++ext+preset"),
        )]);
        let pkg = PackageId::new(CanonicalRepo::MAIN, PackagePath::root());
        let l = Label::resolve("@bazelrc-preset.bzl", &pkg, &mapping).unwrap();
        assert_eq!(l.to_string(), "@@rules_x++ext+preset//:bazelrc-preset.bzl");
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
