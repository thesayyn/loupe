//! Async HTTP download with mirror fallback and integrity verification.
//!
//! Called from inside DICE compute futures, so the API is async end-to-end.
//! Connection reuse is the caller's responsibility — pass a shared
//! [`reqwest::Client`].

use bytes::Bytes;

use crate::integrity::Integrity;
use crate::integrity::IntegrityError;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("no urls provided")]
    NoUrls,

    #[error("integrity: {0}")]
    Integrity(#[from] IntegrityError),

    #[error("all {tried} mirror(s) failed; last error: {last}")]
    AllFailed { tried: usize, last: String },
}

/// Download `urls` in order until one succeeds, then verify against
/// `integrity` if provided.
///
/// A non-2xx response counts as a mirror failure and falls through to
/// the next URL. Integrity mismatch is fatal — we do not retry with
/// the next mirror, because a hash mismatch usually means tampering
/// or a misconfigured pin, not a flaky CDN.
pub async fn download(
    client: &reqwest::Client,
    urls: &[String],
    integrity: Option<&Integrity>,
) -> Result<Bytes, FetchError> {
    if urls.is_empty() {
        return Err(FetchError::NoUrls);
    }

    let mut last_err: Option<String> = None;
    for url in urls {
        match try_fetch(client, url).await {
            Ok(bytes) => {
                if let Some(expected) = integrity {
                    expected.verify(&bytes)?;
                }
                return Ok(bytes);
            }
            Err(e) => {
                tracing::debug!(url = %url, error = %e, "mirror failed; trying next");
                last_err = Some(format!("{url}: {e}"));
            }
        }
    }

    Err(FetchError::AllFailed {
        tried: urls.len(),
        last: last_err.unwrap_or_else(|| "(none)".to_string()),
    })
}

async fn try_fetch(client: &reqwest::Client, url: &str) -> Result<Bytes, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("status {status}"));
    }
    resp.bytes().await.map_err(|e| format!("body: {e}"))
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

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn empty_urls_errors() {
        let r = download(&client(), &[], None).await;
        assert!(matches!(r, Err(FetchError::NoUrls)));
    }

    #[tokio::test]
    async fn single_url_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes("hello".as_bytes()))
            .mount(&server)
            .await;

        let url = format!("{}/file", server.uri());
        let bytes = download(&client(), &[url], None).await.unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[tokio::test]
    async fn integrity_verified() {
        let server = MockServer::start().await;
        let body = b"hello";
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;

        let sri = Integrity::compute(HashAlgorithm::Sha256, body).to_sri_string();
        let parsed = Integrity::parse(&sri).unwrap();
        let url = format!("{}/file", server.uri());
        download(&client(), &[url], Some(&parsed)).await.unwrap();
    }

    #[tokio::test]
    async fn integrity_mismatch_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello"))
            .mount(&server)
            .await;

        let wrong = Integrity::compute(HashAlgorithm::Sha256, b"goodbye");
        let url = format!("{}/file", server.uri());
        let r = download(&client(), &[url], Some(&wrong)).await;
        assert!(matches!(r, Err(FetchError::Integrity(_))));
    }

    #[tokio::test]
    async fn mirror_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/present"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok"))
            .mount(&server)
            .await;

        let urls = vec![
            format!("{}/missing", server.uri()),
            format!("{}/present", server.uri()),
        ];
        let bytes = download(&client(), &urls, None).await.unwrap();
        assert_eq!(&bytes[..], b"ok");
    }

    #[tokio::test]
    async fn all_mirrors_fail() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let urls = vec![format!("{}/a", server.uri()), format!("{}/b", server.uri())];
        let r = download(&client(), &urls, None).await;
        match r {
            Err(FetchError::AllFailed { tried, .. }) => assert_eq!(tried, 2),
            _ => panic!("expected AllFailed"),
        }
    }
}
