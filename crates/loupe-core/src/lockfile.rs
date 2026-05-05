//! `MODULE.bazel.lock` parsing for module-extension repos and
//! per-repo `apparent → canonical` mappings.
//!
//! Bazel's bzlmod gives every repo its own apparent-name view. The
//! main module's mapping comes from its `bazel_dep` calls
//! ([`crate::module_graph::parse`]); transitive deps' mappings live
//! in the lockfile under `moduleExtensions[*].general.recordedInputs`
//! as `REPO_MAPPING:<from_repo>,<apparent> <canonical>` records.
//!
//! Module extensions (`use_extension(...)` in a `MODULE.bazel`) also
//! generate fresh repos at fetch time. The generated repos' canonical
//! names follow the convention `<module_canonical>+<extension_name>+<repo_name>`
//! — e.g. `bazel_features++version_extension+bazel_features_globals`
//! for an extension named `version_extension` defined in
//! `@@bazel_features+`. Inside `bazel_features+`'s code, those repos
//! are visible by their short name (`@bazel_features_globals//...`),
//! so we register them in `bazel_features+`'s per-repo mapping.
//!
//! The lockfile is JSON; see Bazel's `BzlmodFlagsAndEnvVars` /
//! `lockFileVersion` 26 for the full schema. Loupe only consumes the
//! `moduleExtensions` block — everything else (registry hashes, MVS
//! selection, yanked versions) is out of scope for v1.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::label::CanonicalRepo;

/// Parsed `MODULE.bazel.lock` data. Only the bits Loupe needs.
#[derive(Debug, Default, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct Lockfile {
    /// `from_repo → (apparent → canonical)`. Records the repo
    /// mapping each external repo sees when it does
    /// `load("@apparent//...")`.
    pub per_repo_mapping: BTreeMap<CanonicalRepo, BTreeMap<String, CanonicalRepo>>,
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

fn parse_lockfile(src: &str) -> Result<Lockfile> {
    let v: Value = serde_json::from_str(src).context("MODULE.bazel.lock is not valid JSON")?;
    let mut per_repo: BTreeMap<CanonicalRepo, BTreeMap<String, CanonicalRepo>> = BTreeMap::new();

    let extensions = v
        .get("moduleExtensions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (ext_id, ext_node) in extensions.iter() {
        let general = ext_node.get("general").unwrap_or(ext_node);

        // Recorded REPO_MAPPING inputs — these are the per-repo
        // mappings other repos see for this extension's deps.
        if let Some(inputs) = general.get("recordedInputs").and_then(Value::as_array) {
            for input in inputs {
                let s = input.as_str().unwrap_or_default();
                if let Some((from, apparent, canonical)) = parse_repo_mapping_record(s) {
                    per_repo
                        .entry(from)
                        .or_default()
                        .insert(apparent, canonical);
                }
            }
        }

        // Generated repos: register them in the *defining* module's
        // per-repo mapping under their short name. Other modules that
        // `use_repo(...)` them would also want the entry — v1 doesn't
        // parse `use_repo`, so consumers reach generated repos only
        // from inside the defining module (which is enough for the
        // load chains rules_oci pulls in).
        if let (Some(owner), Some(extension_name)) = parse_extension_id(ext_id) {
            let owner_canonical = CanonicalRepo::new(owner);
            if let Some(specs) = general.get("generatedRepoSpecs").and_then(Value::as_object) {
                for repo_name in specs.keys() {
                    let canonical = CanonicalRepo::new(format!(
                        "{}+{}+{}",
                        owner_canonical.as_str(),
                        extension_name,
                        repo_name
                    ));
                    per_repo
                        .entry(owner_canonical.clone())
                        .or_default()
                        .insert(repo_name.clone(), canonical);
                }
            }
        }
    }

    Ok(Lockfile {
        per_repo_mapping: per_repo,
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

/// Combined view that resolves apparent names against (1) the
/// per-repo mapping for the current load site, then (2) the main
/// module's mapping, then (3) Bazel 7's `<apparent>+` fallback (with
/// native-repo special cases). See [`crate::label::Label::resolve`].
pub fn resolve_apparent(
    main_mapping: &BTreeMap<String, CanonicalRepo>,
    per_repo: &BTreeMap<CanonicalRepo, BTreeMap<String, CanonicalRepo>>,
    current_repo: &CanonicalRepo,
    apparent: &str,
) -> Option<CanonicalRepo> {
    if let Some(m) = per_repo.get(current_repo)
        && let Some(c) = m.get(apparent)
    {
        return Some(c.clone());
    }
    main_mapping.get(apparent).cloned()
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
            map.get("bazel_skylib"),
            Some(&CanonicalRepo::new("bazel_skylib+"))
        );
        assert_eq!(
            map.get("bazel_tools"),
            Some(&CanonicalRepo::new("bazel_tools"))
        );
        // Generated repo registers under the defining module's
        // mapping with the bzlmod-spawned canonical name.
        assert_eq!(
            map.get("bazel_features_globals"),
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
    fn parse_extension_id_recovers_owner_and_name() {
        let (owner, name) = parse_extension_id("@@bazel_features+//private:extensions.bzl%ext1");
        assert_eq!(owner.as_deref(), Some("bazel_features+"));
        assert_eq!(name, Some("ext1"));
    }
}
