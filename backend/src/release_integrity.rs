//! Artifact hashes are trusted only when fetched directly from GitHub's HTTPS API.
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, time::Duration};

#[derive(Deserialize)]
struct Asset {
    name: String,
    digest: Option<String>,
    state: String,
}
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    assets: Vec<Asset>,
}

pub struct TrustedAsset {
    pub url: String,
    hash: String,
}

fn select(release: Release, repo: &str, tag: &str, name: &str) -> Result<TrustedAsset, String> {
    if release.draft || (tag != "latest" && release.tag_name != tag) {
        return Err("Метаданные не соответствуют опубликованному релизу".into());
    }
    let mut matches = release.assets.into_iter().filter(|a| a.name == name);
    let asset = matches.next().ok_or("Нет ассета в доверенном релизе")?;
    if matches.next().is_some() || asset.state != "uploaded" {
        return Err("Неоднозначный или незавершённый ассет".into());
    }
    let hash = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("GitHub не предоставил доверенный SHA-256; установка запрещена")?
        .to_ascii_lowercase();
    Ok(TrustedAsset {
        url: format!(
            "https://github.com/{repo}/releases/download/{}/{}",
            urlencoding::encode(&release.tag_name),
            urlencoding::encode(name)
        ),
        hash,
    })
}

pub async fn asset(repo: &str, tag: &str, name: &str) -> Result<TrustedAsset, String> {
    let parts: Vec<_> = repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|s| {
            s.is_empty()
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return Err("Некорректный репозиторий для проверки подлинности".into());
    }
    let endpoint = if tag == "latest" {
        "latest".into()
    } else {
        format!("tags/{}", urlencoding::encode(tag))
    };
    let client = reqwest::Client::builder()
        .user_agent("XKeen-UI")
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let release = fetch(
        &client,
        &format!("https://api.github.com/repos/{repo}/releases/{endpoint}"),
    )
    .await?;
    select(release, repo, tag, name)
}

// Bound both the number of attempts and the entire metadata operation.
const METADATA_ATTEMPTS: usize = 5;
const METADATA_BUDGET: Duration = Duration::from_secs(180);

struct FetchFailure {
    message: String,
    retryable: bool,
    retry_after: Option<Duration>,
}

fn network_failure(error: reqwest::Error) -> FetchFailure {
    use std::error::Error;
    // bytes() reports truncated HTTP bodies as decode errors, before JSON parsing.
    let retryable = error.is_connect() || error.is_timeout() || error.is_body() || error.is_decode();
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    FetchFailure {
        message: format!("Прямой GitHub API недоступен: {message}"),
        retryable,
        retry_after: None,
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    // GitHub normally sends seconds; HTTP also permits an absolute GMT date.
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
            .to_std()
            .unwrap_or_default(),
    )
}

async fn fetch_once(client: &reqwest::Client, url: &str) -> Result<Release, FetchFailure> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(network_failure)?;
    let status = response.status();
    if !status.is_success() {
        let retry_after = retry_after(response.headers());
        return Err(FetchFailure {
            message: format!("Прямой GitHub API: {status}"),
            retryable: matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
                || (status.as_u16() == 403 && retry_after.is_some()),
            retry_after,
        });
    }
    // Read the body separately so interrupted transfers can be retried, while
    // invalid JSON and invalid integrity metadata remain terminal failures.
    let bytes = response.bytes().await.map_err(network_failure)?;
    serde_json::from_slice(&bytes).map_err(|error| FetchFailure {
        message: format!("Некорректные доверенные метаданные: {error}"),
        retryable: false,
        retry_after: None,
    })
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Release, String> {
    fetch_with_policy(client, url, Duration::from_secs(1), METADATA_BUDGET).await
}

async fn fetch_with_policy(
    client: &reqwest::Client, url: &str, base_delay: Duration, budget: Duration,
) -> Result<Release, String> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut last_error = String::new();
    for attempt in 1..=METADATA_ATTEMPTS {
        let failure = match tokio::time::timeout_at(deadline, fetch_once(client, url)).await {
            Ok(Ok(release)) => return Ok(release),
            Ok(Err(failure)) => failure,
            Err(_) => {
                return Err(format!(
                    "Истекло время проверки GitHub API ({} с), попытка {attempt}/{METADATA_ATTEMPTS}. {last_error}",
                    budget.as_secs()
                ));
            }
        };
        last_error = failure.message;
        if !failure.retryable || attempt == METADATA_ATTEMPTS {
            return Err(format!("{last_error} (попытка {attempt}/{METADATA_ATTEMPTS})"));
        }
        let delay = base_delay
            .saturating_mul(1 << (attempt - 1))
            .max(failure.retry_after.unwrap_or_default());
        // Never retry earlier than Retry-After, or extend the total budget.
        if delay >= deadline.saturating_duration_since(tokio::time::Instant::now()) {
            return Err(format!(
                "{last_error}. Следующая попытка не укладывается в лимит {} с",
                budget.as_secs()
            ));
        }
        crate::logger::log(
            "WARN",
            format!(
                "{last_error}. Повтор {}/{METADATA_ATTEMPTS} через {:.1} с",
                attempt + 1,
                delay.as_secs_f64()
            ),
        );
        tokio::time::sleep(delay).await;
    }
    unreachable!("metadata attempts are nonzero")
}

impl TrustedAsset {
    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        self.verify_hash(hex(&Sha256::digest(bytes)))
    }
    pub fn verify_file(&self, path: &Path) -> Result<(), String> {
        #[cfg(unix)]
        unsafe {
            // Lower CPU priority (nice +15) during large file SHA-256 hashing to preserve responsiveness on weak MIPS devices.
            _ = nix::libc::setpriority(nix::libc::PRIO_PROCESS, 0, 15);
        }
        let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let size = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if size == 0 {
                break;
            }
            hasher.update(&buffer[..size]);
        }
        self.verify_hash(hex(&hasher.finalize()))
    }
    fn verify_hash(&self, hash: String) -> Result<(), String> {
        if self.hash == hash {
            Ok(())
        } else {
            Err("SHA-256 не совпадает с прямым GitHub API; установка запрещена".into())
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
pub(crate) fn fixture_asset(url: String, expected: &[u8]) -> TrustedAsset {
    TrustedAsset { url, hash: hex(&Sha256::digest(expected)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    fn release(digest: serde_json::Value) -> Release {
        serde_json::from_value(serde_json::json!({"tag_name":"v1", "draft":false,
            "assets":[{"name":"panel", "state":"uploaded", "digest":digest}]}))
        .unwrap()
    }
    #[test]
    fn only_exact_published_asset_with_valid_sha256_is_trusted() {
        for digest in [
            serde_json::Value::Null,
            "".into(),
            "sha1:abcd".into(),
            "sha256:abcd".into(),
            format!("sha256:{}", "z".repeat(64)).into(),
        ] {
            assert!(select(release(digest), "owner/repo", "v1", "panel").is_err());
        }
        let trusted = select(
            release(format!("sha256:{ABC}").into()),
            "owner/repo",
            "v1",
            "panel",
        )
        .unwrap();
        assert_eq!(
            trusted.url,
            "https://github.com/owner/repo/releases/download/v1/panel"
        );
        assert!(trusted.verify_bytes(b"abc").is_ok());
        for bytes in [b"ab".as_slice(), b"abcd", b"malicious executable", b""] {
            assert!(trusted.verify_bytes(bytes).is_err());
        }
        assert!(
            select(
                release(format!("sha256:{ABC}").into()),
                "owner/repo",
                "v2",
                "panel"
            )
            .is_err()
        );
        assert!(
            select(
                release(format!("sha256:{ABC}").into()),
                "owner/repo",
                "v1",
                "other"
            )
            .is_err()
        );
        let mut draft = release(format!("sha256:{ABC}").into());
        draft.draft = true;
        assert!(select(draft, "owner/repo", "latest", "panel").is_err());
        let mut duplicate = release(format!("sha256:{ABC}").into());
        duplicate.assets.push(Asset {
            name: "panel".into(),
            digest: Some(format!("sha256:{ABC}")),
            state: "uploaded".into(),
        });
        assert!(select(duplicate, "owner/repo", "v1", "panel").is_err());
        let mut pending = release(format!("sha256:{ABC}").into());
        pending.assets[0].state = "starter".into();
        assert!(select(pending, "owner/repo", "v1", "panel").is_err());
    }
    #[test]
    fn disk_hash_covers_all_chunks_and_rejects_truncation_or_tampering() {
        let path = std::env::temp_dir().join(format!("xkeen-digest-{}", uuid::Uuid::new_v4()));
        let bytes = vec![b'x'; 128 * 1024 + 7];
        let trusted = TrustedAsset {
            url: String::new(),
            hash: hex(&Sha256::digest(&bytes)),
        };
        std::fs::write(&path, &bytes).unwrap();
        assert!(trusted.verify_file(&path).is_ok());
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert!(trusted.verify_file(&path).is_err());
        let mut tampered = bytes;
        tampered[65537] = b'y';
        std::fs::write(&path, tampered).unwrap();
        assert!(trusted.verify_file(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(trusted.verify_file(&path).is_err());
    }
    #[tokio::test]
    async fn metadata_redirects_and_failures_are_not_followed_or_replaced_by_gateway_data() {
        use axum::{
            Router,
            http::{StatusCode, header},
            routing::get,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new()
            .route(
                "/redirect",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/forged")]) }),
            )
            .route(
                "/forged",
                get(move || {
                    let h = h.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        "{}"
                    }
                }),
            )
            .route(
                "/unavailable",
                get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        assert!(fetch(&client, &format!("{base}/redirect")).await.is_err());
        assert!(
            fetch_with_policy(&client, &format!("{base}/unavailable"), Duration::ZERO, Duration::from_secs(5))
                .await
                .is_err()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        server.abort();
    }
    #[tokio::test]
    async fn metadata_retries_transient_statuses_but_not_invalid_or_missing_metadata() {
        use axum::{Router, http::StatusCode, routing::get};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for (status, recover, expected_hits) in [
            (StatusCode::SERVICE_UNAVAILABLE, true, 3),
            (StatusCode::TOO_MANY_REQUESTS, true, 3),
            (StatusCode::GATEWAY_TIMEOUT, false, METADATA_ATTEMPTS),
            (StatusCode::NOT_FOUND, false, 1),
            (StatusCode::FORBIDDEN, false, 1),
            (StatusCode::FOUND, false, 1),
            (StatusCode::OK, false, 1), // Invalid JSON is not a network failure.
        ] {
            let hits = Arc::new(AtomicUsize::new(0));
            let h = hits.clone();
            let app = Router::new().route(
                "/",
                get(move || {
                    let h = h.clone();
                    async move {
                        let attempt = h.fetch_add(1, Ordering::SeqCst) + 1;
                        if recover && attempt >= 3 {
                            (StatusCode::OK, r#"{"tag_name":"v1","draft":false,"assets":[]}"#)
                        } else {
                            (status, "invalid JSON")
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let result = fetch_with_policy(&client, &url, Duration::ZERO, Duration::from_secs(5)).await;
            assert_eq!(result.is_ok(), recover, "status {status}");
            assert_eq!(hits.load(Ordering::SeqCst), expected_hits, "status {status}");
            if let Err(message) = result {
                assert!(message.contains(&format!("попытка {expected_hits}/{METADATA_ATTEMPTS}")));
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn metadata_retries_request_timeouts_and_reports_the_underlying_cause() {
        use axum::{Router, routing::get};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/",
            get(move || {
                let h = h.clone();
                async move {
                    if h.fetch_add(1, Ordering::SeqCst) == 0 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    r#"{"tag_name":"v1","draft":false,"assets":[]}"#
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        assert!(
            fetch_with_policy(&client, &url, Duration::ZERO, Duration::from_secs(5))
                .await
                .is_ok()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // A closed local port gives a deterministic transport error and source chain.
        server.abort();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_url = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let message = fetch_with_policy(&client, &closed_url, Duration::ZERO, Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert!(message.contains("попытка 5/5"), "{message}");
        assert!(
            message.contains("Connection refused") || message.contains("connection refused"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn metadata_respects_retry_after_and_does_not_extend_the_total_budget() {
        use axum::{
            Router,
            http::{StatusCode, header},
            routing::get,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new()
            .route(
                "/limited",
                get(move || {
                    let h = h.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "60")])
                    }
                }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    "{}"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let message = fetch_with_policy(
            &client,
            &format!("{base}/limited"),
            Duration::ZERO,
            Duration::from_secs(5),
        )
        .await
        .err()
        .unwrap();
        assert!(
            message.contains("429") && message.contains("не укладывается"),
            "{message}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let start = tokio::time::Instant::now();
        let message = fetch_with_policy(
            &client,
            &format!("{base}/slow"),
            Duration::ZERO,
            Duration::from_millis(50),
        )
        .await
        .err()
        .unwrap();
        assert!(message.contains("Истекло время"), "{message}");
        assert!(start.elapsed() < Duration::from_secs(2));
        server.abort();
    }

    #[test]
    fn retry_after_supports_seconds_and_http_dates() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "60".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(60)));
        let future = (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc2822();
        headers.insert("retry-after", future.parse().unwrap());
        let delay = retry_after(&headers).unwrap();
        assert!(delay > Duration::from_secs(58) && delay <= Duration::from_secs(60));
        headers.insert("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::ZERO));
        headers.insert("retry-after", "invalid".parse().unwrap());
        assert_eq!(retry_after(&headers), None);
    }

    #[tokio::test]
    async fn metadata_retries_interrupted_response_bodies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 4096);
                }
                let body = r#"{"tag_name":"v1","draft":false,"assets":[]}"#;
                let response = if attempt == 0 {
                    "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{".to_string()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let result = fetch_with_policy(&client, &url, Duration::ZERO, Duration::from_secs(5)).await;
        assert!(result.is_ok(), "{:?}", result.err());
        server.await.unwrap();
    }
}
