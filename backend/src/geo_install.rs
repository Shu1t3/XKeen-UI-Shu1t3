//! Install the geodata required by bundled Xray templates from fixed publishers.
use crate::{geo, logger::log, release_integrity, types::*};
use axum::{extract::State, response::Json};
use futures_util::StreamExt;
use memmap2::MmapOptions;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, io::AsyncWriteExt};

static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Source {
    file: &'static str,
    repo: &'static str,
    asset: &'static str,
    site: bool,
}
const SOURCES: &[Source] = &[
    Source {
        file: "zkeen.dat",
        repo: "jameszeroX/zkeen-domains",
        asset: "zkeen.dat",
        site: true,
    },
    Source {
        file: "zkeenip.dat",
        repo: "jameszeroX/zkeen-ip",
        asset: "zkeenip.dat",
        site: false,
    },
    Source {
        file: "geosite_v2fly.dat",
        repo: "v2fly/domain-list-community",
        asset: "dlc.dat",
        site: true,
    },
];

#[derive(Deserialize)]
pub struct InstallRequest {
    files: Vec<String>,
}

fn sources(files: &[String]) -> Result<Vec<Source>, String> {
    if files.is_empty() || files.len() > SOURCES.len() {
        return Err("Выберите от одного до трёх геофайлов шаблона".into());
    }
    let mut selected = Vec::new();
    for file in files {
        let source = SOURCES
            .iter()
            .find(|source| source.file == file)
            .ok_or_else(|| format!("Автоустановка {file} не поддерживается; установите файл вручную"))?;
        if !selected.iter().any(|entry: &Source| entry.file == source.file) {
            selected.push(*source);
        }
    }
    Ok(selected)
}

fn validate_file(path: &Path, site: bool) -> Result<(), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    if size == 0 || size > MAX_FILE_SIZE {
        return Err("Недопустимый размер геофайла (лимит 16 МиБ)".into());
    }
    let data = unsafe { MmapOptions::new().map(&file) }.map_err(|e| e.to_string())?;
    let actual = geo::detect_geo_file_type(&data);
    if actual != (site, !site) {
        return Err("Содержимое не соответствует требуемому типу GeoSite/GeoIP".into());
    }
    Ok(())
}

fn target(base: &Path, source: Source) -> Result<PathBuf, String> {
    let path = base.join(source.file);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let resolved = std::fs::canonicalize(&path).map_err(|e| e.to_string())?;
            let root = std::fs::canonicalize(base).map_err(|e| e.to_string())?;
            if !resolved.starts_with(root) || !resolved.is_file() {
                return Err(format!("Небезопасный путь геофайла {}", source.file));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    Ok(path)
}

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn download_once(client: &reqwest::Client, url: &str, path: &Path) -> Result<(), String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{e:?}"))?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    if response.content_length().is_some_and(|size| size > MAX_FILE_SIZE) {
        return Err("Геофайл превышает лимит 16 МиБ".into());
    }
    let mut file = fs::File::create(path).await.map_err(|e| e.to_string())?;
    let mut stream = response.bytes_stream();
    let mut size = 0u64;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .map_err(|_| "Нет данных геофайла в течение 30 с")?
    {
        let chunk = chunk.map_err(|e| e.to_string())?;
        size += chunk.len() as u64;
        if size > MAX_FILE_SIZE {
            return Err("Геофайл превышает лимит 16 МиБ".into());
        }
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    if size == 0 {
        return Err("Получен пустой геофайл".into());
    }
    file.sync_all().await.map_err(|e| e.to_string())
}

async fn download(source: Source, proxies: &[String], path: &Path) -> Result<(), String> {
    // Metadata stays on direct authenticated HTTPS; gateways transport bytes only.
    let trusted = std::sync::Arc::new(release_integrity::asset(source.repo, "latest", source.asset).await?);
    let client = reqwest::Client::builder()
        .user_agent("XKeen-UI")
        .https_only(true)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| e.to_string())?;
    let urls: Vec<_> = std::iter::once(trusted.url.clone())
        .chain(
            proxies
                .iter()
                .map(|proxy| format!("{}/{}", proxy.trim_end_matches('/'), trusted.url)),
        )
        .collect();
    let operation = async {
        let mut last_error = String::new();
        for url in urls {
            for attempt in 0..2 {
                let result = match download_once(&client, &url, path).await {
                    Ok(()) => {
                        let file = path.to_path_buf();
                        let trusted = trusted.clone();
                        // Hashing and protobuf inspection run off async workers.
                        tokio::task::spawn_blocking(move || {
                            trusted.verify_file(&file)?;
                            validate_file(&file, source.site)
                        })
                        .await
                        .map_err(|e| e.to_string())?
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        log("WARN", format!("Геофайл {}: {error}", source.file));
                        last_error = error;
                    }
                }
                if attempt == 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        Err(last_error)
    };
    tokio::time::timeout(Duration::from_secs(300), operation)
        .await
        .map_err(|_| "Истекло время загрузки геофайла (300 с)".to_string())?
}

async fn install(
    base: PathBuf, selected: Vec<Source>, proxies: Vec<String>, cache: std::sync::Arc<std::sync::RwLock<GeoCache>>,
) -> Result<Value, String> {
    install_with(base, selected, cache, move |source, path| {
        let proxies = proxies.clone();
        async move { download(source, &proxies, &path).await }
    })
    .await
}

async fn install_with<F, Fut>(
    base: PathBuf, selected: Vec<Source>, cache: std::sync::Arc<std::sync::RwLock<GeoCache>>, mut fetch: F,
) -> Result<Value, String>
where
    F: FnMut(Source, PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    fs::create_dir_all(&base).await.map_err(|e| e.to_string())?;
    // Validate every destination before making any network request.
    for source in &selected {
        target(&base, *source)?;
    }
    let staging_path = base.join(format!(".xkeen-geo-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging_path)
        .await
        .map_err(|e| e.to_string())?;
    let staging = Staging(staging_path);
    let mut skipped = Vec::new();
    let mut pending = Vec::new();
    for source in selected {
        let path = target(&base, source)?;
        if validate_file(&path, source.site).is_ok() {
            skipped.push(source.file);
            continue;
        }
        log("INFO", format!("Загрузка геофайла {}", source.file));
        let staged = staging.0.join(source.file);
        fetch(source, staged.clone())
            .await
            .map_err(|e| format!("{}: {e}", source.file))?;
        validate_file(&staged, source.site).map_err(|e| format!("{}: {e}", source.file))?;
        pending.push((source, staged));
    }
    // All downloads passed verification before touching live files. A cancelled
    // browser request cannot cancel the spawned installation transaction.
    crate::config_transaction::run(async move {
        tokio::task::spawn_blocking(move || {
            let _staging = staging;
            let mut installed = Vec::new();
            for (source, staged) in pending {
                let path = target(&base, source)?;
                if validate_file(&path, source.site).is_ok() {
                    skipped.push(source.file);
                    continue;
                }
                let file = File::open(&staged).map_err(|e| e.to_string())?;
                let bytes = unsafe { MmapOptions::new().map(&file) }.map_err(|e| e.to_string())?;
                crate::config_transaction::write_atomic(&path, &bytes, !path.exists())
                    .map_err(|e| format!("{}: {e}; уже установлены: {}", source.file, installed.join(", ")))?;
                cache.write().unwrap().clear();
                installed.push(source.file);
                log("INFO", format!("Установлен геофайл {}", source.file));
            }
            Ok(json!({ "installed": installed, "skipped": skipped }))
        })
        .await
        .map_err(|e| e.to_string())?
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn post_install(State(state): State<AppState>, Json(request): Json<InstallRequest>) -> Json<Value> {
    let selected = match sources(&request.files) {
        Ok(selected) => selected,
        Err(error) => return Json(json!({ "success": false, "error": error })),
    };
    let proxies = state.settings.read().unwrap().updater.github_proxy.clone();
    let cache = state.geo_cache.clone();
    let result = tokio::spawn(async move {
        let _guard = INSTALL_LOCK
            .try_lock()
            .map_err(|_| "Установка геофайлов уже выполняется".to_string())?;
        install(PathBuf::from(XRAY_ASSET_DIR), selected, proxies, cache).await
    })
    .await;
    match result {
        Ok(Ok(data)) => Json(json!({ "success": true, "installed": data["installed"], "skipped": data["skipped"] })),
        Ok(Err(error)) => Json(json!({ "success": false, "error": error })),
        Err(error) => Json(json!({ "success": false, "error": error.to_string() })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    };

    // GeoIPList entry: code RU, CIDR 1.2.3.0/24.
    const IP: &[u8] = &[10, 14, 10, 2, b'R', b'U', 18, 8, 10, 4, 1, 2, 3, 0, 16, 24];
    // GeoSiteList entry: code RU, domain type 2, value example.org.
    const SITE: &[u8] = &[
        10, 21, 10, 2, b'R', b'U', 18, 15, 8, 2, 18, 11, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'o', b'r',
        b'g',
    ];

    fn dir() -> Staging {
        let path = std::env::temp_dir().join(format!("xkeen-geo-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Staging(path)
    }
    fn cache() -> Arc<RwLock<GeoCache>> {
        Arc::new(RwLock::new(GeoCache::new()))
    }

    #[test]
    fn only_known_filenames_are_accepted_and_duplicates_are_deduplicated() {
        for files in [
            vec![],
            vec!["../zkeenip.dat".into()],
            vec!["/tmp/zkeenip.dat".into()],
            vec!["unknown.dat".into()],
        ] {
            assert!(sources(&files).is_err());
        }
        let files = sources(&["zkeenip.dat".into(), "zkeenip.dat".into()]).unwrap();
        assert_eq!(files.len(), 1);
        assert!(!files[0].site);
        assert_eq!(
            sources(&SOURCES.iter().map(|s| s.file.into()).collect::<Vec<_>>())
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn validates_type_size_and_rejects_external_symlinks() {
        let base = dir();
        let outside = dir();
        let path = base.0.join("zkeenip.dat");
        std::fs::write(&path, IP).unwrap();
        assert!(validate_file(&path, false).is_ok());
        assert!(validate_file(&path, true).is_err());
        for bytes in [b"<html>error</html>".as_slice(), b"", b"corrupt"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(validate_file(&path, false).is_err());
        }
        File::create(&path).unwrap().set_len(MAX_FILE_SIZE + 1).unwrap();
        assert!(validate_file(&path, false).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(outside.0.join("ip.dat"), IP).unwrap();
        std::os::unix::fs::symlink(outside.0.join("ip.dat"), &path).unwrap();
        assert!(target(&base.0, SOURCES[1]).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(base.0.join("geoip_zkeenip.dat"), IP).unwrap();
        std::os::unix::fs::symlink("geoip_zkeenip.dat", &path).unwrap();
        assert!(target(&base.0, SOURCES[1]).is_ok());
    }

    #[tokio::test]
    async fn installs_all_missing_files_skips_existing_and_clears_cache() {
        let base = dir();
        std::fs::write(base.0.join("zkeen.dat"), SITE).unwrap();
        let cache = cache();
        cache
            .write()
            .unwrap()
            .insert("old.dat".into(), (std::time::SystemTime::UNIX_EPOCH, true, false));
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result = install_with(base.0.clone(), SOURCES.to_vec(), cache.clone(), move |source, path| {
            c.fetch_add(1, Ordering::SeqCst);
            async move {
                fs::write(path, if source.site { SITE } else { IP })
                    .await
                    .map_err(|e| e.to_string())
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(result["skipped"], json!(["zkeen.dat"]));
        assert_eq!(result["installed"], json!(["zkeenip.dat", "geosite_v2fly.dat"]));
        assert!(cache.read().unwrap().is_empty());
        for source in SOURCES {
            validate_file(&base.0.join(source.file), source.site).unwrap();
        }
        assert_eq!(std::fs::read_dir(&base.0).unwrap().count(), 3);
        // A second request makes no downloads and leaves existing bytes alone.
        install_with(base.0.clone(), SOURCES.to_vec(), cache, |_, _| async {
            panic!("unexpected download")
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_or_wrong_type_downloads_leave_live_files_unchanged_and_remove_staging() {
        for wrong_type in [false, true] {
            let base = dir();
            let old = base.0.join("zkeen.dat");
            std::fs::write(&old, b"previous invalid data").unwrap();
            let result = install_with(
                base.0.clone(),
                SOURCES.to_vec(),
                cache(),
                move |source, path| async move {
                    if source.file == "zkeenip.dat" {
                        if wrong_type {
                            fs::write(path, SITE).await.map_err(|e| e.to_string())
                        } else {
                            Err("download failed".into())
                        }
                    } else {
                        fs::write(path, SITE).await.map_err(|e| e.to_string())
                    }
                },
            )
            .await;
            assert!(result.is_err());
            assert_eq!(std::fs::read(&old).unwrap(), b"previous invalid data");
            assert!(!base.0.join("zkeenip.dat").exists());
            assert_eq!(std::fs::read_dir(&base.0).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn streaming_download_rejects_empty_html_and_oversized_response() {
        use axum::{Router, http::header, routing::get};
        let app = Router::new()
            .route("/ok", get(|| async { IP }))
            .route("/empty", get(|| async { "" }))
            .route("/html", get(|| async { "<html>not geodata</html>" }))
            .route(
                "/large",
                get(|| async { ([(header::CONTENT_LENGTH, (MAX_FILE_SIZE + 1).to_string())], "") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let base = dir();
        let path = base.0.join("download");
        download_once(&client, &format!("{url}/ok"), &path).await.unwrap();
        validate_file(&path, false).unwrap();
        assert!(download_once(&client, &format!("{url}/empty"), &path).await.is_err());
        download_once(&client, &format!("{url}/html"), &path).await.unwrap();
        assert!(validate_file(&path, false).is_err());
        assert!(download_once(&client, &format!("{url}/large"), &path).await.is_err());
        server.abort();
    }
}
