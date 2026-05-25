//! `MODULE.bazel.lock` parsing for module-extension repos and
//! per-repo `apparent → canonical` mappings.
//!
//! Bazel's bzlmod gives every repo its own apparent-name view. The
//! main module's mapping comes from its `bazel_dep` calls
//! ([`crate::module_graph::parse`]); transitive deps' mappings live
//! in the lockfile under `moduleExtensions[*].general` in one of two
//! shapes depending on lockfile version:
//!
//! - **`recordedInputs`** — string-encoded `REPO_MAPPING:<from>,<apparent> <canonical>`
//!   records. Used by Bazel 7's `lockFileVersion: 26` rich lockfile.
//! - **`recordedRepoMappingEntries`** — array of `[<from>, <apparent>, <canonical>]`
//!   tuples. Used by Bazel 7.4 / 8 lockfiles. Captures the same data
//!   in a structured form.
//!
//! Both are parsed; consuming-module `use_repo(...)` entries surface
//! through these records (Bazel writes one entry per module that
//! sees the imported name).
//!
//! Module extensions (`use_extension(...)` in a `MODULE.bazel`) also
//! generate fresh repos at fetch time. The canonical name follows
//! `<module_canonical><sep><extension_name><sep><repo_name>` — `<sep>`
//! is whichever bzlmod separator the owner's canonical already uses
//! (`+` in Bazel 7+, `~` in older / lockFileVersion 11). Examples:
//! `bazel_features++version_extension+bazel_features_globals` (Bazel 7+)
//! or `aspect_bazel_lib~~toolchains~copy_directory_toolchains` (older).
//! These land in the defining module's per-repo mapping.
//!
//! The lockfile is JSON; see Bazel's `BzlmodFlagsAndEnvVars` /
//! `lockFileVersion` 26 for the full schema. Loupe only consumes the
//! `moduleExtensions` block — everything else (registry hashes, MVS
//! selection, yanked versions) is out of scope for v1.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use loupe_labels::{CanonicalRepo, RepoMapping};

/// Parsed `MODULE.bazel.lock` data. Only the bits Loupe needs.
#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct Lockfile {
    /// `from_repo → (apparent → canonical)`. Records the repo
    /// mapping each external repo sees when it does
    /// `load("@apparent//...")`. Each `RepoMapping`'s `owner` is
    /// pre-stamped to the key so the `@//pkg:x` self-ref convention
    /// resolves correctly when a consumer reads
    /// `per_repo_mapping[X]` directly.
    pub per_repo_mapping: BTreeMap<CanonicalRepo, RepoMapping>,
    /// The bzlmod canonical separator the lockfile uses (`+` for
    /// Bazel 7+ / lockFileVersion 26, `~` for the older convention
    /// found in lockFileVersion 11). Detected at parse time by
    /// inspecting extension owner canonicals; defaults to `+` for
    /// empty lockfiles.
    /// Stored as the byte form of the separator char so the struct
    /// satisfies `Allocative` (which `char` doesn't implement). Use
    /// [`Self::canonical_separator()`] to read it.
    canonical_separator_byte: u8,
}

impl Default for Lockfile {
    fn default() -> Self {
        Lockfile {
            per_repo_mapping: BTreeMap::new(),
            canonical_separator_byte: b'+',
        }
    }
}

impl Lockfile {
    /// The bzlmod canonical separator the lockfile uses (`+` or `~`).
    pub fn canonical_separator(&self) -> char {
        self.canonical_separator_byte as char
    }
}

/// Read `<workspace>/MODULE.bazel.lock` if it exists; otherwise
/// return an empty [`Lockfile`]. Tolerates a missing file because
/// fixtures without external deps don't need one — Bazel only
/// generates the lockfile after a build / cquery.
pub fn load(workspace_root: &Path) -> Result<Lockfile> {
    load_from(workspace_root.join("MODULE.bazel.lock"))
}

/// Like [`load`] but reads the lockfile from an explicit path. Used
/// when the workspace's lockfile lacks `recordedInputs` (Bazel 7
/// writes `lockFileVersion: 11` to the workspace and `26` to
/// `<output_base>/MODULE.bazel.lock`; only 26 carries the per-repo
/// mapping records Loupe needs).
pub fn load_from(path: std::path::PathBuf) -> Result<Lockfile> {
    if !path.exists() {
        return Ok(Lockfile::default());
    }
    let src =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    parse_lockfile(&src).with_context(|| format!("parsing {}", path.display()))
}

/// Parse a `MODULE.bazel.lock` JSON string. The `"missing file"`
/// convenience (return `Lockfile::default()` on absence) lives in
/// [`load`] / [`load_from`]; this variant assumes the caller has
/// already done the existence check and read the bytes (e.g. via
/// `FileContentKey`).
pub fn parse_from_str(src: &str) -> Result<Lockfile> {
    parse_lockfile(src)
}

fn parse_lockfile(src: &str) -> Result<Lockfile> {
    let v: Value = serde_json::from_str(src).context("MODULE.bazel.lock is not valid JSON")?;
    let mut per_repo: BTreeMap<CanonicalRepo, RepoMapping> = BTreeMap::new();

    let extensions = v
        .get("moduleExtensions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // Detect the separator FIRST. Any extension owner ending in `~`
    // signals a Bazel 7-era lockfile; otherwise default to `+`.
    // Generated-repo synthesis below needs this to canonicalise
    // well-known owners (`platforms`, `bazel_tools`, etc.) — those
    // canonicals don't carry a trailing separator, so the per-owner
    // heuristic in `canonical_separator_for` always returns `+` and
    // produces the wrong path on Bazel 7.
    let detected_sep: char = {
        let mut s = '+';
        'detect_pre: for ext_id in extensions.keys() {
            if let Some(rest) = ext_id.strip_prefix("@@")
                && let Some(owner) = rest.split("//").next()
                && owner.ends_with('~')
            {
                s = '~';
                break 'detect_pre;
            }
        }
        s
    };

    for (ext_id, ext_node) in extensions.iter() {
        let general = ext_node.get("general").unwrap_or(ext_node);

        // Recorded REPO_MAPPING inputs — shape A (string-encoded).
        // Bazel 7's `lockFileVersion: 26` writes per-repo mappings as
        // `"REPO_MAPPING:<from>,<apparent> <canonical>"` strings inside
        // `recordedInputs`.
        if let Some(inputs) = general.get("recordedInputs").and_then(Value::as_array) {
            for input in inputs {
                let s = input.as_str().unwrap_or_default();
                if let Some((from, apparent, canonical)) = parse_repo_mapping_record(s) {
                    per_repo
                        .entry(from.clone())
                        .or_insert_with(|| RepoMapping::owned_by(from))
                        .entries
                        .insert(apparent, canonical);
                }
            }
        }

        // Recorded repo-mapping entries — shape B (array-of-tuples).
        // Bazel 7.4 / 8 writes `recordedRepoMappingEntries` as
        // `[[<from>, <apparent>, <canonical>], …]`, replacing the
        // string-encoded form for newer lockfile versions. Both can
        // co-exist across versions; we accept either.
        if let Some(entries) = general
            .get("recordedRepoMappingEntries")
            .and_then(Value::as_array)
        {
            for entry in entries {
                let arr = match entry.as_array() {
                    Some(a) if a.len() == 3 => a,
                    _ => continue,
                };
                let from = match arr[0].as_str() {
                    Some(s) => CanonicalRepo::new(s),
                    None => continue,
                };
                let apparent = match arr[1].as_str() {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let canonical = match arr[2].as_str() {
                    Some(s) => CanonicalRepo::new(s),
                    None => continue,
                };
                per_repo
                    .entry(from.clone())
                    .or_insert_with(|| RepoMapping::owned_by(from))
                    .entries
                    .insert(apparent, canonical);
            }
        }

        // Generated repos: register them in the *defining* module's
        // per-repo mapping under their short name. Other modules that
        // `use_repo(...)` them get their entry from
        // `recordedRepoMappingEntries` above (Bazel writes one entry
        // per consuming module). Canonical naming convention:
        // `<owner_canonical><sep><extension><sep><repo>` where `<sep>`
        // is whichever character the owner_canonical already uses
        // (`+` for Bazel 7+ bzlmod, `~` for the older convention).
        if let (Some(owner), Some(extension_name)) = parse_extension_id(ext_id) {
            let owner_canonical = CanonicalRepo::new(owner);
            let sep = detected_sep;
            if let Some(specs) = general.get("generatedRepoSpecs").and_then(Value::as_object) {
                for repo_name in specs.keys() {
                    let canonical = CanonicalRepo::new(format!(
                        "{}{sep}{}{sep}{}",
                        owner_canonical.as_str(),
                        extension_name,
                        repo_name
                    ));
                    per_repo
                        .entry(owner_canonical.clone())
                        .or_insert_with(|| RepoMapping::owned_by(owner_canonical.clone()))
                        .entries
                        .insert(repo_name.clone(), canonical);
                }
            }
        }
    }

    // Detect the bzlmod canonical separator the lockfile uses. Any
    // canonical ending in `~` (an extension owner, a recorded
    // canonical, or anything in the per-repo map) signals the older
    // convention. `+` is the default when no signal is present.
    let mut canonical_separator_byte = b'+';
    'detect: for ext_id in extensions.keys() {
        // Extension ID shape: `@@<owner_canonical>//<path>:<file>%<name>`.
        // Pull `<owner_canonical>` and check its trailing char.
        if let Some(rest) = ext_id.strip_prefix("@@")
            && let Some(owner) = rest.split("//").next()
            && owner.ends_with('~')
        {
            canonical_separator_byte = b'~';
            break;
        }
        for (from, m) in &per_repo {
            if from.as_str().ends_with('~') {
                canonical_separator_byte = b'~';
                break 'detect;
            }
            for c in m.entries.values() {
                if c.as_str().ends_with('~') {
                    canonical_separator_byte = b'~';
                    break 'detect;
                }
            }
        }
    }

    Ok(Lockfile {
        per_repo_mapping: per_repo,
        canonical_separator_byte,
    })
}

/// Parse a `REPO_MAPPING:<from_repo>,<apparent> <canonical>` record.
/// Returns `None` if the line doesn't match the shape.
fn parse_repo_mapping_record(line: &str) -> Option<(CanonicalRepo, String, CanonicalRepo)> {
    let body = line.strip_prefix("REPO_MAPPING:")?;
    let (from, rest) = body.split_once(',')?;
    let (apparent, canonical) = rest.split_once(' ')?;
    Some((
        CanonicalRepo::new(from),
        apparent.to_string(),
        CanonicalRepo::new(canonical),
    ))
}

/// Parse `@@<owner_canonical>//<path>:<file>%<extension_name>`.
/// Returns `(owner_canonical, extension_name)` or `None`.
fn parse_extension_id(id: &str) -> (Option<String>, Option<&str>) {
    let body = match id.strip_prefix("@@") {
        Some(b) => b,
        None => return (None, None),
    };
    let (owner_with_path, ext_name) = match body.rsplit_once('%') {
        Some(pair) => pair,
        None => return (None, None),
    };
    let owner = match owner_with_path.split_once("//") {
        Some((repo, _path)) => repo,
        None => owner_with_path,
    };
    (Some(owner.to_string()), Some(ext_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_lockfile() {
        let lf = parse_lockfile(r#"{"lockFileVersion":26}"#).unwrap();
        assert!(lf.per_repo_mapping.is_empty());
    }

    #[test]
    fn parses_repo_mapping_records_from_module_extension() {
        let lf = parse_lockfile(
            r#"{
              "moduleExtensions": {
                "@@bazel_features+//private:extensions.bzl%version_extension": {
                  "general": {
                    "recordedInputs": [
                      "REPO_MAPPING:bazel_features+,bazel_skylib bazel_skylib+",
                      "REPO_MAPPING:bazel_features+,bazel_tools bazel_tools"
                    ],
                    "generatedRepoSpecs": {
                      "bazel_features_globals": {}
                    }
                  }
                }
              }
            }"#,
        )
        .unwrap();
        let bazel_features = CanonicalRepo::new("bazel_features+");
        let map = lf.per_repo_mapping.get(&bazel_features).unwrap();
        assert_eq!(
            map.lookup("bazel_skylib"),
            Some(&CanonicalRepo::new("bazel_skylib+"))
        );
        assert_eq!(
            map.lookup("bazel_tools"),
            Some(&CanonicalRepo::new("bazel_tools"))
        );
        // Generated repo registers under the defining module's
        // mapping with the bzlmod-spawned canonical name.
        assert_eq!(
            map.lookup("bazel_features_globals"),
            Some(&CanonicalRepo::new(
                "bazel_features++version_extension+bazel_features_globals"
            ))
        );
    }

    #[test]
    fn parse_repo_mapping_record_handles_well_formed_lines() {
        assert_eq!(
            parse_repo_mapping_record("REPO_MAPPING:bazel_features+,bazel_skylib bazel_skylib+"),
            Some((
                CanonicalRepo::new("bazel_features+"),
                "bazel_skylib".to_string(),
                CanonicalRepo::new("bazel_skylib+")
            ))
        );
        assert_eq!(parse_repo_mapping_record("not a record"), None);
        assert_eq!(parse_repo_mapping_record("REPO_MAPPING:no_comma"), None);
    }

    #[test]
    fn parses_recorded_repo_mapping_entries_array_shape() {
        // Bazel 7.4 / 8 lockfiles use the array-of-tuples shape under
        // `recordedRepoMappingEntries` instead of the string-encoded
        // `recordedInputs` form. Each `[from, apparent, canonical]`
        // tuple lands as one per-repo mapping entry.
        let lf = parse_lockfile(
            r#"{
              "moduleExtensions": {
                "@@aspect_bazel_lib~//lib:extensions.bzl%toolchains": {
                  "general": {
                    "recordedRepoMappingEntries": [
                      ["aspect_bazel_lib~", "bazel_skylib", "bazel_skylib~"],
                      ["rules_oci~", "aspect_bazel_lib", "aspect_bazel_lib~"]
                    ]
                  }
                }
              }
            }"#,
        )
        .unwrap();
        assert_eq!(
            lf.per_repo_mapping
                .get(&CanonicalRepo::new("aspect_bazel_lib~"))
                .and_then(|m| m.lookup("bazel_skylib")),
            Some(&CanonicalRepo::new("bazel_skylib~"))
        );
        assert_eq!(
            lf.per_repo_mapping
                .get(&CanonicalRepo::new("rules_oci~"))
                .and_then(|m| m.lookup("aspect_bazel_lib")),
            Some(&CanonicalRepo::new("aspect_bazel_lib~"))
        );
    }

    #[test]
    fn generated_repo_canonical_uses_tilde_separator_when_owner_does() {
        // Pre-Bazel-7 / lockFileVersion 11 lockfiles use `~` as the
        // bzlmod canonical separator; the parser has to match. The
        // expected on-disk repo dir is
        // `<output_base>/external/aspect_bazel_lib~~toolchains~copy_directory_toolchains`.
        let lf = parse_lockfile(
            r#"{
              "moduleExtensions": {
                "@@aspect_bazel_lib~//lib:extensions.bzl%toolchains": {
                  "general": {
                    "generatedRepoSpecs": {
                      "copy_directory_toolchains": {}
                    }
                  }
                }
              }
            }"#,
        )
        .unwrap();
        assert_eq!(
            lf.per_repo_mapping
                .get(&CanonicalRepo::new("aspect_bazel_lib~"))
                .and_then(|m| m.lookup("copy_directory_toolchains")),
            Some(&CanonicalRepo::new(
                "aspect_bazel_lib~~toolchains~copy_directory_toolchains"
            ))
        );
    }

    #[test]
    fn parse_extension_id_recovers_owner_and_name() {
        let (owner, name) = parse_extension_id("@@bazel_features+//private:extensions.bzl%ext1");
        assert_eq!(owner.as_deref(), Some("bazel_features+"));
        assert_eq!(name, Some("ext1"));
    }
}
