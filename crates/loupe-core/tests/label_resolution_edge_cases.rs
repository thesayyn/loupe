//! `Label::resolve` edge cases. Patterned after
//! `src/test/java/com/google/devtools/build/lib/cmdline/LabelTest.java`:
//! apparent vs canonical, package-relative forms, bad syntax, repo
//! mapping application.
//!
//! Loupe's `Label::resolve(s, current_pkg, mapping)` is the
//! canonical entry point. These tests pin the contract.

use loupe_labels::{CanonicalRepo, Label, PackageId, PackagePath, RepoMapping};

fn main_pkg() -> PackageId {
    PackageId::new(CanonicalRepo::main(), PackagePath::root())
}

fn pkg(path: &str) -> PackageId {
    PackageId::new(
        CanonicalRepo::main(),
        PackagePath::new(path.to_string()).unwrap(),
    )
}

fn empty_mapping() -> RepoMapping {
    RepoMapping::empty()
}

#[test]
fn fully_qualified_label_with_repo_resolves_through_mapping() {
    let mut mapping = empty_mapping();
    mapping
        .entries
        .insert("foo".to_string(), CanonicalRepo::new("foo+"));
    let l = Label::resolve("@foo//pkg:target", &main_pkg(), &mapping).unwrap();
    assert_eq!(l.repo().as_str(), "foo+");
    assert_eq!(l.package_path().as_str(), "pkg");
    assert_eq!(l.name(), "target");
}

#[test]
fn double_at_label_bypasses_repo_mapping() {
    // `@@canonical//...` is the already-canonical form per Bazel 7+.
    // The mapping should not be consulted for these.
    let mut mapping = empty_mapping();
    // Even with a different mapping for `foo`, the `@@foo+//` form is
    // taken verbatim.
    mapping
        .entries
        .insert("foo".to_string(), CanonicalRepo::new("rewritten"));
    let l = Label::resolve("@@foo+//pkg:target", &main_pkg(), &mapping).unwrap();
    assert_eq!(l.repo().as_str(), "foo+", "double-@ must bypass mapping");
}

#[test]
fn relative_target_colon_form_resolves_against_current_package() {
    let l = Label::resolve(":target", &pkg("foo/bar"), &empty_mapping()).unwrap();
    assert!(l.repo().is_main());
    assert_eq!(l.package_path().as_str(), "foo/bar");
    assert_eq!(l.name(), "target");
}

#[test]
fn double_slash_label_uses_main_repo_and_explicit_package() {
    let l = Label::resolve("//pkg:target", &pkg("other"), &empty_mapping()).unwrap();
    assert!(l.repo().is_main());
    assert_eq!(l.package_path().as_str(), "pkg");
    assert_eq!(l.name(), "target");
}

#[test]
fn package_only_shorthand_uses_last_segment_as_target() {
    // `//pkg/sub` == `//pkg/sub:sub`. Single-segment also works.
    let l = Label::resolve("//pkg/sub", &main_pkg(), &empty_mapping()).unwrap();
    assert_eq!(l.package_path().as_str(), "pkg/sub");
    assert_eq!(l.name(), "sub", "shorthand target = last segment");

    let l2 = Label::resolve("//x", &main_pkg(), &empty_mapping()).unwrap();
    assert_eq!(l2.package_path().as_str(), "x");
    assert_eq!(l2.name(), "x");
}

#[test]
fn root_package_with_target_only() {
    let l = Label::resolve("//:target", &pkg("other"), &empty_mapping()).unwrap();
    assert_eq!(l.package_path().as_str(), "");
    assert_eq!(l.name(), "target");
}

#[test]
fn apparent_repo_not_in_mapping_raises() {
    // 1:1 with Bazel: an apparent name with no mapping entry is an
    // error. There's no `<apparent>+` fallback — every cross-repo
    // reference must be declared via `bazel_dep` (or be one of the
    // hardcoded native repos like `bazel_tools`).
    let err = Label::resolve("@orphan//pkg:t", &main_pkg(), &empty_mapping()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("orphan"),
        "expected error to mention the missing apparent, got: {msg}"
    );
}

#[test]
#[ignore = "TODO(loupe-labels): Loupe's parser is more permissive than \
    Bazel's — accepts empty labels, just-`:`, double-colons in target \
    names, etc. Bazel rejects all of these in `LabelTest.testBadCharacters` \
    etc. Pin the eventual stricter behavior when the parser is tightened."]
fn malformed_labels_should_be_rejected_per_bazel_semantics() {
    for bad in &["", ":", "//", "@", "//pkg:", "//pkg:bad:name"] {
        let r = Label::resolve(bad, &main_pkg(), &empty_mapping());
        assert!(r.is_err(), "expected `{bad}` to be rejected, got {r:?}");
    }
}

#[test]
fn label_display_round_trips_for_canonical_form() {
    let mut mapping = empty_mapping();
    mapping
        .entries
        .insert("foo".to_string(), CanonicalRepo::new("foo+"));
    let l = Label::resolve("@foo//pkg/sub:target", &main_pkg(), &mapping).unwrap();
    let s = l.to_string();
    // Re-parsing the displayed form must produce the same label.
    let l2 = Label::resolve(&s, &main_pkg(), &empty_mapping()).unwrap();
    assert_eq!(l, l2, "round trip failed: {s} -> {l2}");
}

#[test]
fn current_package_apparent_repo_resolves_when_mapping_present() {
    // `@foo//pkg:t` from a package in main repo uses main's mapping.
    let mut mapping = empty_mapping();
    mapping
        .entries
        .insert("foo".to_string(), CanonicalRepo::new("foo+"));
    let l = Label::resolve("@foo//pkg:t", &main_pkg(), &mapping).unwrap();
    assert_eq!(l.repo().as_str(), "foo+");
}

#[test]
fn at_self_label_with_empty_mapping_raises() {
    // `@//pkg:target` uses the empty apparent — meaning "the
    // mapping's owner module". Without an `owner` on the mapping
    // (and no entry for the empty key), this is unresolved and must
    // raise rather than silently invent a canonical.
    let err = Label::resolve("@//pkg:target", &main_pkg(), &empty_mapping()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("apparent") || msg.contains("unknown"),
        "expected unresolved-apparent error, got: {msg}"
    );
}
