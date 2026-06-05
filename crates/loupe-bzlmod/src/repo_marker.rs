//! Marker file: the source-of-truth cache key for a materialised repo.
//!
//! Layout (line-oriented, Bazel-compatible):
//!
//! ```text
//! <predeclared_input_hash_hex>
//! FILE:<encoded_path> <sha256-hex|DIR|ENOENT>
//! DIRENTS:<encoded_path> <fingerprint-hex|ENOENT>
//! DIRTREE:<encoded_path> <digest-hex|ENOENT>
//! ENV:<encoded_name> <encoded_value|ENOENT>
//! REPO_MAPPING:<encoded_src>,<encoded_apparent> <encoded_canonical>
//! ```
//!
//! The first line is a SHA-256 over predeclared inputs (loupe version,
//! rule definition source, attrs, OS/arch, declared environ). If it
//! mismatches the recomputed value, the marker is stale unconditionally.
//! Otherwise each subsequent line is re-observed; the first drift
//! triggers a rerun.
//!
//! Source-of-truth: marker-only. There are no DICE keys per recorded
//! input — the only DICE key is `MaterializedRepoKey`, which inside its
//! compute reparses this file and revalidates. Documented choice; see
//! the `lets-implement-it` discussion at
//! `~/.claude/plans/write-up-a-plan-lexical-papert.md`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::recorded_input::RecordParseError;
use crate::recorded_input::RepoRecordedInput;
use crate::starlark_eval::RepoAttrValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub predeclared_hash: String,
    pub records: Vec<RepoRecordedInput>,
}

#[derive(Debug, thiserror::Error)]
pub enum MarkerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("empty marker file")]
    Empty,

    #[error("record parse: {0}")]
    Record(#[from] RecordParseError),
}

impl Marker {
    pub fn read(path: &Path) -> Result<Marker, MarkerError> {
        let text = fs::read_to_string(path)?;
        Marker::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Marker, MarkerError> {
        let mut lines = text.lines();
        let predeclared_hash = lines.next().ok_or(MarkerError::Empty)?.trim().to_string();
        let mut records = Vec::new();
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            records.push(RepoRecordedInput::from_marker_line(line)?);
        }
        Ok(Marker {
            predeclared_hash,
            records,
        })
    }

    pub fn write(&self, path: &Path) -> Result<(), MarkerError> {
        let mut text = String::new();
        text.push_str(&self.predeclared_hash);
        text.push('\n');
        for r in &self.records {
            text.push_str(&r.to_marker_line());
            text.push('\n');
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)?;
        Ok(())
    }

    /// True when `current_predeclared` matches and every recorded
    /// input re-observes to its stored value.
    pub fn is_fresh(
        &self,
        current_predeclared: &str,
        declared_env: &BTreeMap<String, String>,
    ) -> FreshnessCheck {
        if self.predeclared_hash != current_predeclared {
            return FreshnessCheck::PredeclaredMismatch {
                stored: self.predeclared_hash.clone(),
                current: current_predeclared.to_string(),
            };
        }
        for r in &self.records {
            let now = r.reobserve(declared_env);
            if &now != r {
                return FreshnessCheck::Drifted {
                    stored: r.clone(),
                    current: now,
                };
            }
        }
        FreshnessCheck::Fresh
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshnessCheck {
    Fresh,
    PredeclaredMismatch {
        stored: String,
        current: String,
    },
    Drifted {
        stored: RepoRecordedInput,
        current: RepoRecordedInput,
    },
}

/// Inputs to the predeclared-input hash. Anything that should bust the
/// whole marker unconditionally (Bazel version bump, rule source edit,
/// attr change, OS swap) belongs here.
#[derive(Debug, Clone)]
pub struct PredeclaredInputs<'a> {
    pub loupe_version: &'a str,
    /// SHA-256 (hex) of the `.bzl` file that defined the rule. Coarse —
    /// any edit to the file invalidates all rules defined in it — but
    /// reliable and simple. See `feedback_*` discussion at
    /// `~/.claude/plans/write-up-a-plan-lexical-papert.md` for the
    /// alternative considered.
    pub rule_source_digest: &'a str,
    pub attrs: &'a BTreeMap<String, RepoAttrValue>,
    pub os: &'a str,
    pub arch: &'a str,
    pub declared_environ: &'a [String],
}

impl<'a> PredeclaredInputs<'a> {
    pub fn compute(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"loupe_version=");
        h.update(self.loupe_version.as_bytes());
        h.update(b"\n");
        h.update(b"rule_source_digest=");
        h.update(self.rule_source_digest.as_bytes());
        h.update(b"\n");
        h.update(b"os=");
        h.update(self.os.as_bytes());
        h.update(b"\n");
        h.update(b"arch=");
        h.update(self.arch.as_bytes());
        h.update(b"\n");
        h.update(b"attrs=\n");
        for (k, v) in self.attrs.iter() {
            h.update(k.as_bytes());
            h.update(b"=");
            hash_attr(&mut h, v);
            h.update(b"\n");
        }
        h.update(b"environ=\n");
        let mut env_sorted: Vec<&str> = self.declared_environ.iter().map(String::as_str).collect();
        env_sorted.sort();
        for e in env_sorted {
            h.update(e.as_bytes());
            h.update(b"\n");
        }
        hex_encode(h.finalize())
    }
}

fn hash_attr(h: &mut Sha256, v: &RepoAttrValue) {
    match v {
        RepoAttrValue::None => {
            h.update(b"N");
        }
        RepoAttrValue::String(s) => {
            h.update(b"S:");
            h.update(s.as_bytes());
        }
        RepoAttrValue::Int(i) => {
            h.update(b"I:");
            h.update(i.to_string().as_bytes());
        }
        RepoAttrValue::Bool(b) => {
            h.update(b"B:");
            h.update(if *b { b"1" } else { b"0" });
        }
        RepoAttrValue::StringList(items) => {
            h.update(b"L:[");
            for item in items {
                h.update(item.as_bytes());
                h.update(b",");
            }
            h.update(b"]");
        }
        RepoAttrValue::StringDict(map) => {
            h.update(b"D:{");
            for (k, v) in map.iter() {
                h.update(k.as_bytes());
                h.update(b"=");
                h.update(v.as_bytes());
                h.update(b";");
            }
            h.update(b"}");
        }
        RepoAttrValue::StringListDict(map) => {
            h.update(b"LD:{");
            for (k, vs) in map.iter() {
                h.update(k.as_bytes());
                h.update(b"=[");
                for v in vs {
                    h.update(v.as_bytes());
                    h.update(b",");
                }
                h.update(b"];");
            }
            h.update(b"}");
        }
        RepoAttrValue::Label(s) => {
            h.update(b"LB:");
            h.update(s.as_bytes());
        }
        RepoAttrValue::LabelList(items) => {
            h.update(b"LBL:[");
            for item in items {
                h.update(item.as_bytes());
                h.update(b",");
            }
            h.update(b"]");
        }
        RepoAttrValue::LabelKeyedStringDict(map) => {
            h.update(b"LKD:{");
            for (k, v) in map.iter() {
                h.update(k.as_bytes());
                h.update(b"=");
                h.update(v.as_bytes());
                h.update(b";");
            }
            h.update(b"}");
        }
    }
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorded_input::FileValue;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn predeclared_hash_is_deterministic() {
        let mut attrs = BTreeMap::new();
        attrs.insert("name".to_string(), RepoAttrValue::String("x".to_string()));
        attrs.insert(
            "urls".to_string(),
            RepoAttrValue::StringList(vec!["a".to_string(), "b".to_string()]),
        );
        let environ = vec!["PATH".to_string(), "JAVA_HOME".to_string()];
        let inputs = PredeclaredInputs {
            loupe_version: "0.0.0",
            rule_source_digest: "abc",
            attrs: &attrs,
            os: "macos",
            arch: "aarch64",
            declared_environ: &environ,
        };
        let h1 = inputs.compute();
        let h2 = inputs.compute();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn predeclared_hash_changes_on_attr_change() {
        let env: Vec<String> = vec![];
        let mut a1 = BTreeMap::new();
        a1.insert("k".to_string(), RepoAttrValue::String("v1".to_string()));
        let mut a2 = BTreeMap::new();
        a2.insert("k".to_string(), RepoAttrValue::String("v2".to_string()));
        let mk = |attrs: &BTreeMap<String, RepoAttrValue>| {
            PredeclaredInputs {
                loupe_version: "0",
                rule_source_digest: "x",
                attrs,
                os: "linux",
                arch: "x86_64",
                declared_environ: &env,
            }
            .compute()
        };
        assert_ne!(mk(&a1), mk(&a2));
    }

    #[test]
    fn predeclared_hash_changes_on_environ_set() {
        let attrs = BTreeMap::new();
        let env1: Vec<String> = vec!["PATH".to_string()];
        let env2: Vec<String> = vec!["PATH".to_string(), "HOME".to_string()];
        let mk = |environ: &Vec<String>| {
            PredeclaredInputs {
                loupe_version: "0",
                rule_source_digest: "x",
                attrs: &attrs,
                os: "linux",
                arch: "x86_64",
                declared_environ: environ,
            }
            .compute()
        };
        assert_ne!(mk(&env1), mk(&env2));
    }

    #[test]
    fn predeclared_hash_independent_of_environ_order() {
        let attrs = BTreeMap::new();
        let env1: Vec<String> = vec!["A".to_string(), "B".to_string()];
        let env2: Vec<String> = vec!["B".to_string(), "A".to_string()];
        let mk = |environ: &Vec<String>| {
            PredeclaredInputs {
                loupe_version: "0",
                rule_source_digest: "x",
                attrs: &attrs,
                os: "linux",
                arch: "x86_64",
                declared_environ: environ,
            }
            .compute()
        };
        assert_eq!(mk(&env1), mk(&env2));
    }

    #[test]
    fn marker_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".marker");
        let m = Marker {
            predeclared_hash: "deadbeef".to_string(),
            records: vec![
                RepoRecordedInput::EnvVar {
                    name: "PATH".to_string(),
                    value: Some("/usr/bin:/bin".to_string()),
                },
                RepoRecordedInput::File {
                    path: PathBuf::from("/x/y"),
                    value: FileValue::Digest("aabb".to_string()),
                },
            ],
        };
        m.write(&path).unwrap();
        let parsed = Marker::read(&path).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn freshness_predeclared_mismatch() {
        let m = Marker {
            predeclared_hash: "old".to_string(),
            records: vec![],
        };
        let env = BTreeMap::new();
        assert_eq!(
            m.is_fresh("new", &env),
            FreshnessCheck::PredeclaredMismatch {
                stored: "old".to_string(),
                current: "new".to_string(),
            }
        );
    }

    #[test]
    fn freshness_env_drift() {
        let stored = RepoRecordedInput::EnvVar {
            name: "FOO".to_string(),
            value: Some("v1".to_string()),
        };
        let m = Marker {
            predeclared_hash: "h".to_string(),
            records: vec![stored.clone()],
        };
        let mut env = BTreeMap::new();
        env.insert("FOO".to_string(), "v2".to_string());
        match m.is_fresh("h", &env) {
            FreshnessCheck::Drifted { .. } => {}
            other => panic!("expected drift, got {other:?}"),
        }
    }

    #[test]
    fn freshness_all_match() {
        let stored = RepoRecordedInput::EnvVar {
            name: "FOO".to_string(),
            value: Some("v1".to_string()),
        };
        let m = Marker {
            predeclared_hash: "h".to_string(),
            records: vec![stored.clone()],
        };
        let mut env = BTreeMap::new();
        env.insert("FOO".to_string(), "v1".to_string());
        assert_eq!(m.is_fresh("h", &env), FreshnessCheck::Fresh);
    }
}
