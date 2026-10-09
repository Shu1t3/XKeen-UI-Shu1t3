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

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Release, String> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("Прямой GitHub API недоступен: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Прямой GitHub API: {}", response.status()));
    }
    response
        .json()
        .await
        .map_err(|e| format!("Некорректные доверенные метаданные: {e}"))
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
            fetch(&client, &format!("{base}/unavailable"))
                .await
                .is_err()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
