//! Per-repo apparent → canonical mapping with self-ref support.
//!
//! Bazel's `RepositoryMappingValue` is per-repo: each repo R has its
//! own `apparent → canonical` map plus an "owner" hint that lets
//! `@//pkg:label` (apparent name empty) resolve back to R. Loupe used
//! to thread the same data as a bare `BTreeMap<String, CanonicalRepo>`
//! everywhere, which produced three different concrete shapes across
//! `loupe-core` / `loupe-bzlmod` and made every call site re-derive
//! the @-self-ref entry. `RepoMapping` is the single typed value those
//! sites now share.

use std::collections::BTreeMap;

use crate::types::CanonicalRepo;

/// Apparent → canonical lookup for a single owning repo.
///
/// Bazel's `RepositoryMapping` shape: a name table plus the owner so
/// the `@<empty>//pkg:label` convention ("a label in the owning
/// module") resolves without each caller seeding the entry. Most
/// loupe code stores this behind an `Arc<RepoMapping>` — clones are
/// then pointer-cheap and the value participates in DICE equality.
#[derive(Debug, Clone, Default, PartialEq, Eq, allocative::Allocative)]
pub struct RepoMapping {
    /// Apparent → canonical entries. Direct field access so synthesis
    /// code (`module_graph::synthesise_module_graph`, the lockfile
    /// parser, etc.) can use `entries.entry(k).or_insert(...)` patterns
    /// without an encapsulation wrapper.
    pub entries: BTreeMap<String, CanonicalRepo>,
    /// Repo that owns this mapping — drives the `@//pkg:label`
    /// self-ref convention. `None` means self-ref resolution returns
    /// nothing (callers may then fall through to
    /// [`crate::types::Label::resolve`]'s fallback).
    pub owner: Option<CanonicalRepo>,
}

impl RepoMapping {
    /// Empty mapping, no owner. Equivalent to `Default::default()`.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Empty mapping with the given owner.
    pub fn owned_by(owner: CanonicalRepo) -> Self {
        Self {
            entries: BTreeMap::new(),
            owner: Some(owner),
        }
    }

    /// Builder-style owner setter for chaining off other constructors
    /// (`RepoMapping::from(map).with_owner(repo)`).
    pub fn with_owner(mut self, owner: CanonicalRepo) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Resolve `apparent` to its canonical, honoring the @-self-ref
    /// convention: empty `apparent` resolves to [`Self::owner`] when
    /// set. Returns `None` when the name isn't in the table and no
    /// owner is set — the caller is expected to raise; there is no
    /// `<apparent>+` fallback.
    pub fn resolve_apparent(&self, apparent: &str) -> Option<CanonicalRepo> {
        if apparent.is_empty() {
            return self.owner.clone();
        }
        self.entries.get(apparent).cloned()
    }

    /// Borrow form of [`Self::resolve_apparent`]. Useful when the
    /// caller just needs the `&str` and clones are wasteful.
    pub fn lookup(&self, apparent: &str) -> Option<&CanonicalRepo> {
        if apparent.is_empty() {
            return self.owner.as_ref();
        }
        self.entries.get(apparent)
    }

    /// `true` when both [`Self::entries`] and [`Self::owner`] are
    /// empty/unset.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.owner.is_none()
    }

    /// Iterate over the `(apparent, canonical)` entries. The
    /// self-ref entry is not included — `apparent` strings here are
    /// the literal `bazel_dep` names + `use_repo` aliases that were
    /// declared.
    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, String, CanonicalRepo> {
        self.entries.iter()
    }
}

impl From<BTreeMap<String, CanonicalRepo>> for RepoMapping {
    fn from(entries: BTreeMap<String, CanonicalRepo>) -> Self {
        Self {
            entries,
            owner: None,
        }
    }
}

impl<S: Into<String>> FromIterator<(S, CanonicalRepo)> for RepoMapping {
    fn from_iter<I: IntoIterator<Item = (S, CanonicalRepo)>>(iter: I) -> Self {
        Self {
            entries: iter.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            owner: None,
        }
    }
}

impl<S: Into<String>> Extend<(S, CanonicalRepo)> for RepoMapping {
    fn extend<I: IntoIterator<Item = (S, CanonicalRepo)>>(&mut self, iter: I) {
        self.entries
            .extend(iter.into_iter().map(|(k, v)| (k.into(), v)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_resolves_nothing() {
        let m = RepoMapping::empty();
        assert!(m.is_empty());
        assert_eq!(m.resolve_apparent("foo"), None);
        assert_eq!(m.resolve_apparent(""), None);
    }

    #[test]
    fn lookup_hits_entries() {
        let m = RepoMapping::from_iter([("foo", CanonicalRepo::new("foo+"))]);
        assert_eq!(m.resolve_apparent("foo"), Some(CanonicalRepo::new("foo+")));
        assert_eq!(m.resolve_apparent("bar"), None);
    }

    #[test]
    fn empty_apparent_resolves_to_owner() {
        let m = RepoMapping::owned_by(CanonicalRepo::new("rules_multitool+"));
        assert_eq!(
            m.resolve_apparent(""),
            Some(CanonicalRepo::new("rules_multitool+"))
        );
    }

    #[test]
    fn empty_apparent_with_no_owner_returns_none() {
        let m = RepoMapping::from_iter([("foo", CanonicalRepo::new("foo+"))]);
        assert_eq!(m.resolve_apparent(""), None);
    }

    #[test]
    fn explicit_empty_entry_loses_to_owner() {
        // Owner takes precedence over an "" entry; that's the
        // documented convention. Construction code that wants the
        // entry to win should clear the owner.
        let mut m = RepoMapping::owned_by(CanonicalRepo::new("owner+"));
        m.entries
            .insert(String::new(), CanonicalRepo::new("other+"));
        assert_eq!(m.resolve_apparent(""), Some(CanonicalRepo::new("owner+")));
    }

    #[test]
    fn from_btreemap_no_owner() {
        let mut bt = BTreeMap::new();
        bt.insert("a".to_string(), CanonicalRepo::new("a+"));
        let m = RepoMapping::from(bt);
        assert_eq!(m.entries.len(), 1);
        assert!(m.owner.is_none());
    }

    #[test]
    fn with_owner_sets_owner() {
        let m: RepoMapping = RepoMapping::from_iter([("a", CanonicalRepo::new("a+"))])
            .with_owner(CanonicalRepo::MAIN);
        assert_eq!(m.owner.as_ref(), Some(&CanonicalRepo::MAIN));
    }
}
