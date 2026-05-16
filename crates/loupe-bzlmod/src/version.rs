//! Bazel module versions — semver-shaped but with bazel-specific
//! tie-breaking and `0`-version handling.
//!
//! Bazel's spec: `MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`.
//! Comparison follows semver rules; `+BUILD` is ignored for ordering.
//! Version `""` (empty) is "no version" — only legal for non-registry
//! overrides (local_path, archive, git) and sorts below everything.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct Version {
    raw: String,
    parts: Vec<u64>,
    prerelease: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Self {
        if s.is_empty() {
            return Version {
                raw: String::new(),
                parts: Vec::new(),
                prerelease: None,
            };
        }
        // Strip `+build` metadata — not used for ordering.
        let core_and_pre = s.split('+').next().unwrap_or(s);
        // Split on `-` for prerelease.
        let (core, prerelease) = match core_and_pre.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (core_and_pre, None),
        };
        let parts: Vec<u64> = core
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect();
        Version {
            raw: s.to_string(),
            parts,
            prerelease,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // Empty version sorts below everything else.
        if self.raw.is_empty() && other.raw.is_empty() {
            return Ordering::Equal;
        }
        if self.raw.is_empty() {
            return Ordering::Less;
        }
        if other.raw.is_empty() {
            return Ordering::Greater;
        }
        let len = self.parts.len().max(other.parts.len());
        for i in 0..len {
            let a = self.parts.get(i).copied().unwrap_or(0);
            let b = other.parts.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }
        // Semver: a version without prerelease is greater than one with.
        match (&self.prerelease, &other.prerelease) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple() {
        assert_eq!(Version::parse("1.2.3").as_str(), "1.2.3");
        assert!(Version::parse("").is_empty());
    }

    #[test]
    fn orders_by_major_minor_patch() {
        assert!(Version::parse("1.2.3") < Version::parse("1.2.4"));
        assert!(Version::parse("1.2.3") < Version::parse("1.3.0"));
        assert!(Version::parse("1.2.3") < Version::parse("2.0.0"));
        assert!(Version::parse("1.10.0") > Version::parse("1.9.0"));
    }

    #[test]
    fn prerelease_sorts_below_release() {
        assert!(Version::parse("1.0.0-rc1") < Version::parse("1.0.0"));
        assert!(Version::parse("1.0.0-rc1") < Version::parse("1.0.0-rc2"));
    }

    #[test]
    fn empty_sorts_lowest() {
        assert!(Version::parse("") < Version::parse("0.0.1"));
        let a = Version::parse("");
        let b = Version::parse("");
        assert_eq!(a.cmp(&b), Ordering::Equal);
    }

    #[test]
    fn build_metadata_ignored() {
        let a = Version::parse("1.0.0+abc");
        let b = Version::parse("1.0.0+xyz");
        assert_eq!(a.cmp(&b), Ordering::Equal);
    }
}
