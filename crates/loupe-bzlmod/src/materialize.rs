//! Download + verify + extract a `RepoSpec::Http` into the cache.
//!
//! Pure function over a shared `reqwest::Client` and a cache root. The
//! DICE wrapper in `dice.rs` calls this; tests call it directly with
//! `wiremock`. Keeps the I/O surface testable without spinning DICE.

use std::path::Path;
use std::path::PathBuf;

use crate::archive::ArchiveError;
use crate::archive::ArchiveKind;
use crate::archive::extract;
use crate::fetch::FetchError;
use crate::fetch::download;
use crate::integrity::Integrity;
use crate::integrity::IntegrityError;
use crate::repo_spec::RepoSpec;
use crate::repo_spec::RepoSpecKind;
use crate::starlark_eval::eval_repository_rule;

#[derive(Debug, thiserror::Error)]
pub enum MaterializeError {
    #[error("fetch: {0}")]
    Fetch(#[from] FetchError),

    #[error("integrity: {0}")]
    Integrity(#[from] IntegrityError),

    #[error("archive: {0}")]
    Archive(#[from] ArchiveError),

    #[error("cannot determine archive kind from url(s): {0}")]
    UnknownArchiveKind(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("repo kind not yet supported: {0}")]
    Unsupported(&'static str),

    #[error("task join: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("repository_rule eval: {0}")]
    RuleEval(String),
}

/// Materialise one repo into `<cache_root>/<canonical>/`. Returns the
/// resulting directory path.
///
/// Skips fetching when a sibling `.integrity` marker matches the
/// spec's integrity string — keeps repeat runs cheap without
/// re-walking the tree.
pub async fn materialize(
    client: &reqwest::Client,
    cache_root: &Path,
    spec: &RepoSpec,
    declared_env: std::sync::Arc<std::collections::BTreeMap<String, String>>,
    label_resolver: std::sync::Arc<dyn crate::label_resolver::LabelResolver>,
) -> Result<PathBuf, MaterializeError> {
    let dest = cache_root.join(&spec.canonical);
    match &spec.kind {
        RepoSpecKind::Http {
            urls,
            integrity,
            strip_prefix,
            archive_kind,
        } => {
            materialize_http(
                client,
                cache_root,
                &spec.canonical,
                &dest,
                urls,
                integrity.as_deref(),
                strip_prefix.as_deref(),
                *archive_kind,
            )
            .await
        }
        RepoSpecKind::Git { .. } => Err(MaterializeError::Unsupported("git_repository")),
        RepoSpecKind::LocalPath { .. } => Err(MaterializeError::Unsupported("local_repository")),
        RepoSpecKind::StarlarkRule {
            module,
            rule_fv,
            attrs,
            rule_source_digest,
            repo_mapping,
        } => {
            materialize_starlark_rule(
                client,
                cache_root,
                &dest,
                &spec.canonical,
                module,
                *rule_fv,
                attrs,
                rule_source_digest,
                std::sync::Arc::clone(repo_mapping),
                declared_env,
                label_resolver,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn materialize_starlark_rule(
    client: &reqwest::Client,
    cache_root: &Path,
    dest: &Path,
    canonical: &str,
    module: &std::sync::Arc<starlark::environment::FrozenModule>,
    rule_fv: starlark::values::FrozenValue,
    attrs: &std::collections::BTreeMap<String, crate::starlark_eval::RepoAttrValue>,
    rule_source_digest: &std::sync::Arc<String>,
    repo_mapping: std::sync::Arc<std::collections::BTreeMap<String, String>>,
    declared_env: std::sync::Arc<std::collections::BTreeMap<String, String>>,
    label_resolver: std::sync::Arc<dyn crate::label_resolver::LabelResolver>,
) -> Result<PathBuf, MaterializeError> {
    use crate::repo_marker::FreshnessCheck;
    use crate::repo_marker::Marker;
    use crate::repo_marker::PredeclaredInputs;
    use starlark::values::ValueLike;

    let marker_path = cache_root.join(format!("{canonical}.marker"));

    // Pull the rule's declared environ list off the FrozenRepositoryRule.
    // We don't run any Starlark here — just downcast and clone the Vec.
    let declared_environ: Vec<String> = {
        let rule = rule_fv
            .downcast_ref::<crate::starlark_eval::FrozenRepositoryRule>()
            .ok_or_else(|| {
                MaterializeError::RuleEval(format!(
                    "FrozenValue for `{canonical}` is not a repository_rule (got `{}`)",
                    rule_fv.to_value().get_type()
                ))
            })?;
        rule.environ.clone()
    };

    let predeclared = PredeclaredInputs {
        loupe_version: env!("CARGO_PKG_VERSION"),
        rule_source_digest: rule_source_digest.as_str(),
        attrs,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        declared_environ: &declared_environ,
    }
    .compute();

    // Fast path: existing marker + everything still observed identical.
    if dest.is_dir()
        && let Ok(m) = Marker::read(&marker_path)
        && let FreshnessCheck::Fresh = m.is_fresh(&predeclared, &declared_env)
    {
        return Ok(dest.to_path_buf());
    }

    if dest.exists() {
        tokio::fs::remove_dir_all(dest).await?;
    }
    tokio::fs::create_dir_all(dest).await?;

    let handle = tokio::runtime::Handle::current();
    let module = module.clone();
    let attrs_owned = attrs.clone();
    let client = client.clone();
    let dest_owned = dest.to_path_buf();
    let canonical_owned = canonical.to_string();
    let env_for_eval = std::sync::Arc::clone(&declared_env);
    let mapping_for_eval = std::sync::Arc::clone(&repo_mapping);
    let resolver_for_eval = std::sync::Arc::clone(&label_resolver);
    let records = tokio::task::spawn_blocking(move || {
        eval_repository_rule(
            rule_fv,
            &module,
            &canonical_owned,
            &attrs_owned,
            &dest_owned,
            &client,
            handle,
            env_for_eval,
            mapping_for_eval,
            resolver_for_eval,
        )
    })
    .await??;

    // Always pre-record the rule's declared `environ=[...]` so the
    // marker tracks them even if the rule never called `ctx.getenv`.
    let mut final_records = records;
    let seen_env: std::collections::BTreeSet<String> = final_records
        .iter()
        .filter_map(|r| match r {
            crate::recorded_input::RepoRecordedInput::EnvVar { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    for name in &declared_environ {
        if !seen_env.contains(name) {
            final_records.push(crate::recorded_input::RepoRecordedInput::EnvVar {
                name: name.clone(),
                value: declared_env.get(name).cloned(),
            });
        }
    }

    let marker = Marker {
        predeclared_hash: predeclared,
        records: final_records,
    };
    if let Some(parent) = marker_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    marker
        .write(&marker_path)
        .map_err(|e| MaterializeError::RuleEval(format!("write marker: {e}")))?;

    Ok(dest.to_path_buf())
}

#[allow(clippy::too_many_arguments)]
async fn materialize_http(
    client: &reqwest::Client,
    cache_root: &Path,
    canonical: &str,
    dest: &Path,
    urls: &[String],
    integrity: Option<&str>,
    strip_prefix: Option<&str>,
    explicit_kind: Option<ArchiveKind>,
) -> Result<PathBuf, MaterializeError> {
    let marker = cache_root.join(format!("{canonical}.integrity"));
    if let Some(expected) = integrity
        && dest.is_dir()
        && tokio::fs::read_to_string(&marker)
            .await
            .map(|s| s.trim() == expected)
            .unwrap_or(false)
    {
        return Ok(dest.to_path_buf());
    }

    let parsed = integrity.map(Integrity::parse).transpose()?;
    let bytes = download(client, urls, parsed.as_ref()).await?;

    let kind = explicit_kind
        .or_else(|| urls.iter().find_map(|u| ArchiveKind::from_url(u)))
        .ok_or_else(|| MaterializeError::UnknownArchiveKind(urls.join(", ")))?;

    if dest.exists() {
        tokio::fs::remove_dir_all(dest).await?;
    }
    tokio::fs::create_dir_all(dest).await?;

    let bytes_for_blocking = bytes.to_vec();
    let dest_for_blocking = dest.to_path_buf();
    let strip = strip_prefix.map(String::from);
    tokio::task::spawn_blocking(move || {
        extract(
            kind,
            &bytes_for_blocking,
            &dest_for_blocking,
            strip.as_deref(),
        )
    })
    .await??;

    if let Some(expected) = integrity {
        tokio::fs::create_dir_all(cache_root).await?;
        tokio::fs::write(&marker, expected).await?;
    }

    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::HashAlgorithm;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    fn build_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut t = tar::Builder::new(gz);
            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                t.append_data(&mut header, name, *data).unwrap();
            }
            t.into_inner().unwrap().finish().unwrap();
        }
        buf
    }

    #[tokio::test]
    async fn materialises_http_archive_end_to_end() {
        let tarball = build_tar_gz(&[("pkg-1.0/MODULE.bazel", b"module(name='x')")]);
        let sri = Integrity::compute(HashAlgorithm::Sha256, &tarball).to_sri_string();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball.clone()))
            .mount(&server)
            .await;

        let cache = tempfile::tempdir().unwrap();
        let spec = RepoSpec {
            canonical: "platforms".to_string(),
            kind: RepoSpecKind::Http {
                urls: vec![format!("{}/x.tar.gz", server.uri())],
                integrity: Some(sri.clone()),
                strip_prefix: Some("pkg-1.0".to_string()),
                archive_kind: None,
            },
        };

        let dir = materialize(
            &reqwest::Client::new(),
            cache.path(),
            &spec,
            std::sync::Arc::new(std::collections::BTreeMap::new()),
            std::sync::Arc::new(crate::label_resolver::UnresolvedLabelResolver),
        )
        .await
        .unwrap();
        assert_eq!(dir, cache.path().join("platforms"));
        assert_eq!(
            std::fs::read(dir.join("MODULE.bazel")).unwrap(),
            b"module(name='x')"
        );
        assert!(cache.path().join("platforms.integrity").is_file());
    }

    #[tokio::test]
    async fn second_call_hits_integrity_cache() {
        let tarball = build_tar_gz(&[("pkg/file", b"hello")]);
        let sri = Integrity::compute(HashAlgorithm::Sha256, &tarball).to_sri_string();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball.clone()))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let cache = tempfile::tempdir().unwrap();
        let spec = RepoSpec {
            canonical: "thing".to_string(),
            kind: RepoSpecKind::Http {
                urls: vec![format!("{}/x.tar.gz", server.uri())],
                integrity: Some(sri),
                strip_prefix: Some("pkg".to_string()),
                archive_kind: None,
            },
        };
        let client = reqwest::Client::new();
        materialize(
            &client,
            cache.path(),
            &spec,
            std::sync::Arc::new(std::collections::BTreeMap::new()),
            std::sync::Arc::new(crate::label_resolver::UnresolvedLabelResolver),
        )
        .await
        .unwrap();
        // Server is set to allow only one fetch; second call would 404
        // (or wiremock 'unmatched' error) if we redownloaded.
        let dir = materialize(
            &client,
            cache.path(),
            &spec,
            std::sync::Arc::new(std::collections::BTreeMap::new()),
            std::sync::Arc::new(crate::label_resolver::UnresolvedLabelResolver),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(dir.join("file")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn git_raises_unsupported() {
        let cache = tempfile::tempdir().unwrap();
        let spec = RepoSpec {
            canonical: "x".to_string(),
            kind: RepoSpecKind::Git {
                remote: "https://example/x.git".to_string(),
                commit: "deadbeef".to_string(),
                strip_prefix: None,
            },
        };
        let r = materialize(
            &reqwest::Client::new(),
            cache.path(),
            &spec,
            std::sync::Arc::new(std::collections::BTreeMap::new()),
            std::sync::Arc::new(crate::label_resolver::UnresolvedLabelResolver),
        )
        .await;
        assert!(matches!(r, Err(MaterializeError::Unsupported(_))));
    }
}
