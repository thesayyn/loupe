//! Environment-variable policy for repo-rule execution.
//!
//! Baseline is the host environment. Per-variable overrides are applied
//! on top: `Set("value")` overwrites, `Unset` removes, `Inherit` is a
//! no-op but documents intent (and pins which host vars the rule is
//! allowed to observe, if a stricter policy is wired later).
//!
//! Mirrors Bazel's `--repo_env` (`NAME=VAL` sets, `NAME` inherits,
//! `NAME=` unsets), but stays as plain data so the engine builder can
//! supply overrides programmatically too.

use std::collections::BTreeMap;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvOverride {
    Set(String),
    Inherit,
    Unset,
}

/// Parse Bazel's `--repo_env`-flag syntax.
///
/// - `NAME=VAL` → `Set("VAL")`
/// - `NAME=`    → `Unset`
/// - `NAME`     → `Inherit`
pub fn parse_repo_env_flag(s: &str) -> (String, EnvOverride) {
    match s.split_once('=') {
        Some((name, "")) => (name.to_string(), EnvOverride::Unset),
        Some((name, val)) => (name.to_string(), EnvOverride::Set(val.to_string())),
        None => (s.to_string(), EnvOverride::Inherit),
    }
}

/// Resolve the effective environment a repo rule sees: host env with
/// `overrides` layered on. Returned as a sorted `BTreeMap` so callers
/// (and the marker file) see a deterministic ordering.
pub fn resolve_env(
    host: &HashMap<String, String>,
    overrides: &BTreeMap<String, EnvOverride>,
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> =
        host.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (name, override_) in overrides {
        match override_ {
            EnvOverride::Set(v) => {
                out.insert(name.clone(), v.clone());
            }
            EnvOverride::Unset => {
                out.remove(name);
            }
            EnvOverride::Inherit => { /* no-op; host value already present */ }
        }
    }
    out
}

/// Snapshot of the current process environment. Caller supplies — kept
/// out of the lib core so tests can inject deterministic input.
pub fn host_env_snapshot() -> HashMap<String, String> {
    std::env::vars().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_set() {
        assert_eq!(
            parse_repo_env_flag("FOO=bar"),
            ("FOO".to_string(), EnvOverride::Set("bar".to_string()))
        );
    }

    #[test]
    fn parse_unset() {
        assert_eq!(
            parse_repo_env_flag("FOO="),
            ("FOO".to_string(), EnvOverride::Unset)
        );
    }

    #[test]
    fn parse_inherit() {
        assert_eq!(
            parse_repo_env_flag("FOO"),
            ("FOO".to_string(), EnvOverride::Inherit)
        );
    }

    #[test]
    fn parse_value_with_equals() {
        assert_eq!(
            parse_repo_env_flag("FOO=a=b"),
            ("FOO".to_string(), EnvOverride::Set("a=b".to_string()))
        );
    }

    #[test]
    fn resolve_overrides_layer_on_host() {
        let mut host = HashMap::new();
        host.insert("HOME".to_string(), "/home/user".to_string());
        host.insert("PATH".to_string(), "/usr/bin".to_string());
        let mut over = BTreeMap::new();
        over.insert("FOO".to_string(), EnvOverride::Set("bar".to_string()));
        over.insert("PATH".to_string(), EnvOverride::Unset);
        over.insert("HOME".to_string(), EnvOverride::Inherit);
        let out = resolve_env(&host, &over);
        assert_eq!(out.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(out.get("HOME"), Some(&"/home/user".to_string()));
        assert!(!out.contains_key("PATH"));
    }
}
