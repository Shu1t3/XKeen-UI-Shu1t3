use crate::logger::log;
use crate::types::*;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Json};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{Cursor, Read, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const GITHUB_API: &str = "https://api.github.com/repos";
const GITHUB_RELEASE: &str = "https://github.com";

const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
struct GhAsset {
    name: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

enum DownloadResult {
    Ram(Vec<u8>),
    Disk(PathBuf),
}

pub fn repo_slug(url: &str) -> String {
    let s = url.trim().trim_end_matches('/');
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let s = s.strip_prefix("www.").unwrap_or(s);
    let s = match s.find('/') {
        Some(i) if s[..i].contains('.') => &s[i + 1..],
        _ => s,
    };
    s.strip_suffix(".git").unwrap_or(s).to_string()
}

pub fn valid_repo_url(url: &str) -> bool {
    let slug = repo_slug(url);
    let mut parts = slug.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(a), Some(b), None) if !a.is_empty() && !b.is_empty()
    )
}

pub fn get_repo(updater: &UpdaterSettings, core: &str) -> Option<String> {
    let (url, fallback) = match core {
        "xray" => (&updater.xray_repo, "XTLS/Xray-core"),
        "mihomo" => (&updater.mihomo_repo, "MetaCubeX/mihomo"),
        "self" => return Some(crate::release_source::UI_RELEASE_SOURCE.repository.clone()),
        _ => return None,
    };
    Some(if valid_repo_url(url) {
        repo_slug(url)
    } else {
        fallback.into()
    })
}

pub fn pick_asset(assets: &[String], arch: &str, ver: &str) -> Option<String> {
    // архитектурные суффиксы: (mihomo-стиль, xray-стиль)
    let (m, x) = match arch {
        "aarch64" => ("arm64", "arm64-v8a"),
        "mips" if cfg!(target_endian = "little") => ("mipsle-softfloat", "mips32le"),
        "mips" => ("mips-softfloat", "mips32"),
        _ => return None,
    };

    // alpha-ассеты заканчиваются хешем, а в релизе ещё .deb/.rpm/.zip/.zst
    if ver == "Prerelease-Alpha" {
        let alpha = format!("linux-{}-alpha", m);
        return assets
            .iter()
            .find(|a| a.ends_with(".gz") && a.contains(&alpha))
            .cloned();
    }

    // хвосты имени ассета без названия ядра:
    //   prizrak-core-linux-arm64-v1.19.31.gz -> linux-arm64-v1.19.31.gz
    //   Xray-linux-arm64-v8a.zip             -> linux-arm64-v8a.zip
    let tails = [format!("linux-{}-{}.gz", m, ver), format!("linux-{}.zip", x)];
    assets.iter().find(|a| tails.iter().any(|t| a.ends_with(t))).cloned()
}

async fn fetch_release_assets(client: &reqwest::Client, proxies: &[String], repo: &str, tag: &str) -> Vec<String> {
    let url = format!("{}/{}/releases/tags/{}", GITHUB_API, repo, tag);
    let list = std::iter::once(url.clone()).chain(
        proxies
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| format!("{}/{}", p.trim_end_matches('/'), url)),
    );

    for u in list {
        let res = match client
            .get(&u)
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };
        if let Ok(rel) = res.json::<GhRelease>().await {
            return rel.assets.into_iter().map(|a| a.name).collect();
        }
    }
    Vec::new()
}

pub async fn fetch_latest_version(
    client: &reqwest::Client, repo: &str, core: &str, proxies: &[String], current_ver: Option<&str>,
) -> Option<(String, String)> {
    let repo = if core == "self" {
        &crate::release_source::UI_RELEASE_SOURCE.repository
    } else {
        repo
    };
    let url = format!("{}/{}/releases?per_page=10", GITHUB_API, repo);
    let list = std::iter::once(url.clone()).chain(
        proxies
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| format!("{}/{}", p.trim_end_matches('/'), url)),
    );

    for u in list {
        let res = match client
            .get(&u)
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };
        if res
            .headers()
            .get("content-type")
            .is_some_and(|v| v.to_str().unwrap_or("").contains("text/html"))
        {
            continue;
        }
        let rels = match res.json::<Vec<GhRelease>>().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(release) = select_latest_release(rels, core, current_ver) {
            return Some(release);
        }
    }
    None
}

fn select_latest_release(rels: Vec<GhRelease>, core: &str, current_ver: Option<&str>) -> Option<(String, String)> {
    if current_ver.is_some_and(|v| v.contains("alpha")) && core == "mihomo"
        && let Some(r) = rels.iter().find(|r| !r.draft && r.tag_name == "Prerelease-Alpha") {
        for asset in &r.assets {
            if let Some(hash) = asset
                .name
                .find("alpha-")
                .and_then(|index| asset.name[index..].split('.').next())
            {
                return Some((hash.to_string(), "Prerelease-Alpha".into()));
            }
        }
    }

    // Panel updates offer the newest published fork release, including prereleases.
    // Core release-channel behaviour remains unchanged.
    if let Some(r) = rels.into_iter().find(|r| !r.draft && (core == "self" || !r.prerelease)) {
        let tag = r.tag_name.clone();
        return Some((tag.trim_start_matches('v').to_string(), tag));
    }
    None
}

fn response(success: bool, error: Option<String>) -> (HeaderMap, Json<Value>) {
    let mut h = HeaderMap::new();
    h.insert(header::CONNECTION, "close".parse().unwrap());
    (h, Json(json!({ "success": success, "error": error })))
}

async fn download_verified(repo: &str, tag: &str, name: &str, proxies: &[String], path: &Path) -> Result<DownloadResult, String> {
    let trusted = crate::release_integrity::asset(repo, tag, name).await?;
    download_with_trusted_asset(trusted, proxies, path).await
}

async fn download_with_trusted_asset(trusted: crate::release_integrity::TrustedAsset, proxies: &[String], path: &Path) -> Result<DownloadResult, String> {
    let result = download(&trusted.url, proxies, path).await?;
    match &result {
        DownloadResult::Ram(bytes) => trusted.verify_bytes(bytes)?,
        DownloadResult::Disk(file) => {
            let file = file.clone();
            tokio::task::spawn_blocking(move || trusted.verify_file(&file)).await.map_err(|e| e.to_string())??;
        }
    }
    Ok(result)
}

async fn download(url: &str, proxies: &[String], tmp_path: &Path) -> Result<DownloadResult, String> {
    // Downloads need a larger budget than the shared API/relay client.
    let client = reqwest::Client::builder()
        .user_agent("XKeen-UI")
        .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TOTAL_TIMEOUT)
        .build()
        .map_err(|e| format!("Не удалось создать клиент загрузки: {e:?}"))?;
    download_with_client(&client, url, proxies, tmp_path).await
}

async fn download_with_client(
    client: &reqwest::Client, url: &str, proxies: &[String], tmp_path: &Path,
) -> Result<DownloadResult, String> {
    async fn load(r: reqwest::Response, path: &Path, source: &str) -> Option<DownloadResult> {
        let size = r.content_length().unwrap_or(0) as usize;
        let (mut stream, is_disk) = (r.bytes_stream(), size > 50 * 1024 * 1024);
        let mut file = if is_disk {
            Some(fs::File::create(path).await.ok()?)
        } else {
            None
        };
        let mut buf = if is_disk {
            Vec::new()
        } else {
            Vec::with_capacity(if size > 0 { size } else { 5 * 1024 * 1024 })
        };

        loop {
            match tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    if let Some(f) = &mut file {
                        if f.write_all(&chunk).await.is_err() {
                            log("WARN", format!("Ошибка записи на диск ({})", source));
                            _ = fs::remove_file(path);
                            return None;
                        }
                    } else {
                        buf.extend_from_slice(&chunk);
                    }
                }
                Ok(None) => {
                    if !is_disk && buf.is_empty() {
                        log("WARN", format!("Загрузка вернула 0 байт ({})", source));
                        return None;
                    }
                    log(
                        "INFO",
                        format!(
                            "Файл загружен {} ({:.1} МБ)",
                            if is_disk { "на диск" } else { "в ОЗУ" },
                            (if is_disk { size } else { buf.len() }) as f64 / 1048576.0
                        ),
                    );
                    return Some(if is_disk {
                        DownloadResult::Disk(path.to_path_buf())
                    } else {
                        DownloadResult::Ram(buf)
                    });
                }
                Ok(Some(Err(e))) => {
                    log("WARN", format!("Соединение оборвалось ({}): {:?}", source, e));
                    break;
                }
                Err(_) => {
                    log(
                        "WARN",
                        format!(
                            "Таймаут загрузки ({}): нет данных {} с",
                            source,
                            DOWNLOAD_IDLE_TIMEOUT.as_secs()
                        ),
                    );
                    break;
                }
            }
        }
        if is_disk {
            _ = fs::remove_file(path).await;
        }
        None
    }

    let list = std::iter::once(url.to_string()).chain(proxies.iter().map(|p| format!("{}/{}", p, url)));
    for (i, u) in list.enumerate() {
        let (source, is_proxy) = if i == 0 {
            ("напрямую", false)
        } else {
            ("прокси", true)
        };
        if is_proxy {
            log(
                "INFO",
                format!("Попытка загрузки через прокси #{}: {}", i, proxies[i - 1]),
            );
        }

        let attempt = if is_proxy {
            format!("прокси #{}", i)
        } else {
            "напрямую".into()
        };
        match tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, client.get(&u).send()).await {
            Ok(Ok(r)) if r.status().is_success() => {
                if r.headers()
                    .get("content-type")
                    .is_some_and(|v| v.to_str().unwrap_or("").contains("text/html"))
                {
                    log(
                        "WARN",
                        if is_proxy {
                            format!("Прокси #{} вернул HTML", i)
                        } else {
                            "Прямой URL вернул HTML".into()
                        },
                    );
                    continue;
                }
                if let Some(res) = load(
                    r,
                    tmp_path,
                    &format!("{}{}", source, if is_proxy { format!(" #{}", i) } else { "".into() }),
                )
                .await
                {
                    return Ok(res);
                }
            }
            Ok(Ok(r)) => log("WARN", format!("Ошибка загрузки ({}): {}", attempt, r.status())),
            Ok(Err(e)) => log("WARN", format!("Ошибка загрузки ({}): {:?}", attempt, e)),
            Err(_) => log(
                "WARN",
                format!(
                    "Таймаут ожидания ответа ({}): {} с",
                    attempt,
                    DOWNLOAD_IDLE_TIMEOUT.as_secs()
                ),
            ),
        }
    }
    log("ERROR", "Не удалось выполнить обновление".into());
    Err("Не удалось выполнить обновление".into())
}
async fn save(dl: DownloadResult, out_path: PathBuf) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut out = File::create(&out_path)?;
        match dl {
            DownloadResult::Ram(d) => out.write_all(&d)?,
            DownloadResult::Disk(p) => {
                std::io::copy(&mut File::open(&p)?, &mut out)?;
                _ = std::fs::remove_file(p);
            }
        }
        out.sync_data()
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
}

async fn install_jq() -> Result<(), String> {
    log("INFO", "Установка jq через opkg...".into());
    let update = Command::new("opkg")
        .arg("update")
        .status()
        .await
        .map_err(|e| format!("opkg update: {}", e))?;
    if !update.success() {
        return Err("Ошибка обновления opkg кеша".into());
    }
    let install = Command::new("opkg")
        .args(["install", "jq"])
        .status()
        .await
        .map_err(|e| format!("opkg install jq: {}", e))?;
    if !install.success() {
        return Err("Ошибка установки jq".into());
    }
    log("INFO", "Пакет jq установлен".into());
    Ok(())
}

async fn install_yq(proxies: &[String], tmp_dir: &Path) -> Result<(), String> {
    let arch = std::env::consts::ARCH;
    let (tag, name) = match arch {
        "aarch64" => ("latest", "yq_linux_arm64"),
        "mips" if cfg!(target_endian = "little") => ("v4.52.2", "yq_linux_mipsle"),
        "mips" => ("v4.52.2", "yq_linux_mips"),
        _ => return Err("Архитектура не поддерживается для yq".into()),
    };
    let dl_res = download_verified("mikefarah/yq", tag, name, proxies, &tmp_dir.join("yq.tmp")).await?;
    let target = opt_path!("/sbin/yq");
    if let Err(e) = save(dl_res, tmp_dir.join("yq.bin")).await {
        return Err(format!("Ошибка записи yq: {}", e));
    }

    log("INFO", "Установка yq...".into());
    let src = tmp_dir.join("yq.bin");
    if fs::rename(&src, target).await.is_err() {
        fs::copy(&src, target)
            .await
            .map_err(|e| format!("Ошибка установки yq: {}", e))?;
        _ = fs::remove_file(&src).await;
    }
    _ = fs::set_permissions(target, std::fs::Permissions::from_mode(0o755)).await;
    log("INFO", "Пакет yq установлен".into());
    Ok(())
}

pub async fn post_update(State(state): State<AppState>, Json(req): Json<UpdateReq>) -> impl IntoResponse {
    // A disconnected request must not cancel replacement or recovery halfway through.
    match crate::update_transaction::run_to_completion(async move { perform_update(state, req).await }).await {
        Ok(result) => result,
        Err(e) => response(false, Some(format!("Ошибка обновления: {e}"))),
    }
}

async fn perform_update(state: AppState, req: UpdateReq) -> (HeaderMap, Json<Value>) {
    let (repo, proxies) = {
        let s = state.settings.read().unwrap();
        (get_repo(&s.updater, &req.core), s.updater.github_proxy.clone())
    };
    let Some(repo) = repo else {
        return response(false, Some("Неизвестное ядро".into()));
    };
    let ver = if req.version.starts_with(|c: char| c.is_ascii_digit()) {
        format!("v{}", req.version)
    } else {
        req.version.clone()
    };
    let mut core_cap = req.core.clone();
    if let Some(r) = core_cap.get_mut(0..1) {
        r.make_ascii_uppercase();
    }

    log(
        "INFO",
        format!(
            "Запущено обновление {} до {}",
            if req.core == "self" { "XKeen UI" } else { &core_cap },
            ver
        ),
    );

    let mut work = match crate::update_transaction::Workspace::create(Path::new(opt_path!("/sbin"))) {
        Ok(work) => work,
        Err(e) => return response(false, Some(e)),
    };
    let tmp_path = work.path.clone();
    let tmp_dir = tmp_path.as_path();
    let arch = std::env::consts::ARCH;

    if req.core == "self" {
        let arch_suffix = match arch {
            "aarch64" => "arm64-v8a",
            "mips" if cfg!(target_endian = "little") => "mips32le",
            "mips" => "mips32",
            _ => return response(false, Some("Архитектура не поддерживается".into())),
        };

        log("INFO", "Загрузка исполняемого файла...".into());
        let bin_d = match download_verified(&repo, &ver, &format!("xkeen-ui-{arch_suffix}"), &proxies, &tmp_dir.join("bin.tmp")).await {
            Ok(d) => d,
            Err(e) => return response(false, Some(e)),
        };

        let source = tmp_dir.join("new");
        if let Err(e) = save(bin_d, source.clone()).await {
            return response(false, Some(format!("Ошибка сохранения: {e}")));
        }
        if let Err(e) = crate::update_transaction::preflight(&source, "self", &ver).await {
            return response(false, Some(e));
        }
        let init = Path::new(S99XKEEN_UI);
        // Respect the saved init script's panel port; never use a request-supplied address.
        let init_text = fs::read_to_string(init).await.unwrap_or_default();
        let args = init_text
            .lines()
            .find_map(|line| line.trim().strip_prefix("ARGS="))
            .unwrap_or("");
        let words: Vec<_> = args
            .split([' ', '\t', '\r', '"', '\''])
            .filter(|v| !v.is_empty())
            .collect();
        let port = words
            .windows(2)
            .find_map(|w| (w[0] == "-p").then_some(w[1]))
            .filter(|p| p.parse::<u16>().is_ok_and(|p| p != 0))
            .unwrap_or("1000");
        match crate::update_transaction::start_self_update(
            &mut work,
            Path::new(opt_path!("/sbin/xkeen-ui")),
            init,
            port,
        )
        .await
        {
            Ok(id) => {
                log("INFO", format!("Обновление панели передано supervisor, job={id}"));
                let (headers, _) = response(true, None);
                // Old browser bundles must not mistake job acceptance for completed installation.
                return (
                    headers,
                    Json(json!({"success":false, "pending":true, "job_id":id,
                    "error":"Обновление запущено; ожидается подтверждение запуска"})),
                );
            }
            Err(e) => return response(false, Some(e)),
        }
    }

    let assets = if req.assets.is_empty() {
        log("INFO", format!("Получение списка ассетов релиза {}...", ver));
        fetch_release_assets(&state.http_client, &proxies, &repo, &ver).await
    } else {
        req.assets.clone()
    };

    let asset = if !assets.is_empty() {
        match pick_asset(&assets, arch, &ver) {
            Some(a) => a,
            None => {
                let msg = if matches!(arch, "aarch64" | "mips") {
                    "Не найден ассет для этой архитектуры в релизе"
                } else {
                    "Архитектура не поддерживается"
                };
                return response(false, Some(msg.into()));
            }
        }
    } else {
        // фолбэк: хардкод имён для стоковых репозиториев, если список ассетов получить не удалось
        match req.core.as_str() {
            "xray" => match arch {
                "aarch64" => "Xray-linux-arm64-v8a.zip".to_string(),
                "mips" if cfg!(target_endian = "little") => "Xray-linux-mips32le.zip".to_string(),
                "mips" => "Xray-linux-mips32.zip".to_string(),
                _ => return response(false, Some("Архитектура не поддерживается".into())),
            },
            "mihomo" if ver == "Prerelease-Alpha" => {
                return response(false, Some("Ассет не найден — обновите страницу и повторите".into()));
            }
            "mihomo" => {
                let m = match arch {
                    "aarch64" => "arm64",
                    "mips" if cfg!(target_endian = "little") => "mipsle-softfloat",
                    "mips" => "mips-softfloat",
                    _ => return response(false, Some("Архитектура не поддерживается".into())),
                };
                format!("mihomo-linux-{}-{}.gz", m, ver)
            }
            _ => return response(false, Some("Неизвестное ядро".into())),
        }
    };
    let url = format!("{}/{}/releases/download/{}/{}", GITHUB_RELEASE, repo, ver, asset);

    match req.core.as_str() {
        "xray" if !Path::new(opt_path!("/bin/jq")).exists() => {
            log("WARN", "Пакет jq не найден".into());
            if let Err(e) = install_jq().await {
                return response(false, Some(e));
            }
        }
        "mihomo" if !Path::new(opt_path!("/sbin/yq")).exists() => {
            log("WARN", "Пакет yq не найден".into());
            if let Err(e) = install_yq(&proxies, tmp_dir).await {
                return response(false, Some(e));
            }
        }
        _ => {}
    }

    log("INFO", format!("Загрузка: {}", url));
    let dl_res = match download_verified(&repo, &ver, &asset, &proxies, &tmp_dir.join("download.tmp")).await {
        Ok(r) => r,
        Err(e) => return response(false, Some(e)),
    };

    log("INFO", "Установка обновления...".into());
    let (core_name, is_zip) = (req.core.clone(), asset.ends_with(".zip"));

    fn unpack<R: Read + Seek>(rdr: R, out_path: &Path, core: &str, is_zip: bool) -> std::io::Result<()> {
        let mut out = File::create(out_path)?;
        if is_zip {
            let mut archive = zip::ZipArchive::new(rdr)?;
            let mut entry: Option<String> = None;
            for i in 0..archive.len() {
                if let Ok(f) = archive.by_index(i) {
                    let base = f.name().rsplit('/').next().unwrap_or(f.name()).to_string();
                    if !f.is_dir() && base.eq_ignore_ascii_case(core) {
                        entry = Some(f.name().to_string());
                        break;
                    }
                }
            }
            if entry.is_none() {
                let mut files: Vec<String> = Vec::new();
                for i in 0..archive.len() {
                    if let Ok(f) = archive.by_index(i)
                        && !f.is_dir() {
                            files.push(f.name().to_string());
                        }
                }
                if files.len() == 1 {
                    entry = files.into_iter().next();
                }
            }
            let name = entry
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "бинарник ядра не найден в архиве"))?;
            std::io::copy(&mut archive.by_name(&name)?, &mut out)?;
        } else {
            std::io::copy(&mut flate2::read::GzDecoder::new(rdr), &mut out)?;
        }
        out.sync_data()?;
        Ok(())
    }

    let tmp_name = "new".to_string();
    let unpack_dir = tmp_dir.to_path_buf();
    let unpack = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        #[cfg(unix)]
        unsafe {
            // Lower process/thread scheduling priority during heavy CPU decompression (nice +15)
            // to prevent CPU starvation for router routing and system services on weak single-core MIPS devices.
            _ = nix::libc::setpriority(nix::libc::PRIO_PROCESS, 0, 15);
        }
        let bin = unpack_dir.join(&tmp_name);
        match dl_res {
            DownloadResult::Ram(d) => unpack(Cursor::new(d), &bin, &core_name, is_zip)?,
            DownloadResult::Disk(p) => {
                unpack(File::open(&p)?, &bin, &core_name, is_zip)?;
                _ = std::fs::remove_file(p);
            }
        };
        Ok(())
    })
    .await;

    if let Ok(Err(e)) | Err(e) = unpack.map_err(|e| std::io::Error::other(e.to_string())) {
        return response(false, Some(format!("Ошибка распаковки: {}", e)));
    }

    let target = PathBuf::from(format!(opt_path!("/sbin/{}"), req.core));
    let source = tmp_dir.join("new");
    if let Err(e) = crate::update_transaction::preflight(&source, &req.core, &ver).await {
        return response(false, Some(e));
    }
    if req.backup_core && target.exists() {
        let backup_dir = Path::new(opt_path!("/sbin/core-backup"));
        let backup_file = backup_dir.join(format!(
            "{}-{}-{}",
            req.core,
            chrono::Utc::now().format("%Y%m%d-%H%M%S"),
            uuid::Uuid::new_v4()
        ));
        if let Err(e) = async {
            fs::create_dir_all(backup_dir).await?;
            fs::copy(&target, &backup_file).await?;
            fs::File::open(&backup_file).await?.sync_all().await
        }
        .await
        {
            return response(false, Some(format!("Ошибка архивной резервной копии: {e}")));
        }
    }
    let running = !crate::controller::get_pid(&req.core).is_empty();
    if let Err(e) = crate::update_transaction::replace_core(&mut work, &target, running, || {
        crate::controller::soft_restart(&req.core)
    })
    .await
    {
        log("ERROR", e.clone());
        return response(false, Some(e));
    }

    log("INFO", format!("Обновление {} до {} завершено", core_cap, ver));
    {
        let mut c = state.update_checker.core_outdated.write().unwrap();
        *c = false;
    }
    {
        let mut c = state.update_checker.last_core_check.write().unwrap();
        *c = None;
    }
    *state.update_checker.last_core_toast.write().unwrap() = None;

    response(true, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn download_server(header_delay: Duration, body_delay: Duration) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/asset", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            tokio::time::sleep(header_delay).await;
            if socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabcd")
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(body_delay).await;
            let _ = socket.write_all(b"efgh").await;
        });
        (url, server)
    }

    fn download_test_path() -> PathBuf {
        std::env::temp_dir().join(format!("xkeen-download-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn download_accepts_response_headers_after_five_seconds() {
        let (url, server) = download_server(Duration::from_secs(6), Duration::ZERO).await;
        let result = download(&url, &[], &download_test_path()).await.unwrap();
        assert!(matches!(result, DownloadResult::Ram(ref bytes) if bytes == b"abcdefgh"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn download_accepts_body_pauses_longer_than_five_seconds() {
        let (url, server) = download_server(Duration::ZERO, Duration::from_secs(6)).await;
        let result = download(&url, &[], &download_test_path()).await.unwrap();
        assert!(matches!(result, DownloadResult::Ram(ref bytes) if bytes == b"abcdefgh"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn download_total_timeout_discards_partial_body_and_tries_proxy() {
        let (url, stalled) = download_server(Duration::ZERO, Duration::from_secs(60)).await;
        let (proxy_url, proxy) = download_server(Duration::ZERO, Duration::ZERO).await;
        let proxy_base = proxy_url.strip_suffix("/asset").unwrap().to_string();
        // A short total budget makes the timeout regression test fast.
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let result = download_with_client(&client, &url, &[proxy_base], &download_test_path())
            .await
            .unwrap();
        assert!(matches!(result, DownloadResult::Ram(ref bytes) if bytes == b"abcdefgh"));
        proxy.await.unwrap();
        stalled.abort();
        let _ = stalled.await;
    }

    #[test]
    fn panel_offers_newest_published_release_including_prereleases() {
        let releases = r#"[
            {"tag_name":"v9.0.0", "draft":true},
            {"tag_name":"v0.0.1-fork.6", "prerelease":true},
            {"tag_name":"v1.0.0", "prerelease":false}
        ]"#;
        for current in [Some("1.0.0"), Some("0.0.1-fork.5"), None] {
            assert_eq!(
                select_latest_release(serde_json::from_str(releases).unwrap(), "self", current),
                Some(("0.0.1-fork.6".into(), "v0.0.1-fork.6".into()))
            );
        }
        let releases = r#"[{"tag_name":"v1.0.1"}, {"tag_name":"v0.0.1-fork.5", "prerelease":true}]"#;
        assert_eq!(
            select_latest_release(serde_json::from_str(releases).unwrap(), "self", Some("0.0.1-fork.5")),
            Some(("1.0.1".into(), "v1.0.1".into()))
        );
    }

    #[test]
    fn missing_published_panel_releases_do_not_fall_back() {
        assert_eq!(select_latest_release(vec![], "self", Some("0.0.1-fork.5")), None);
        let draft = serde_json::from_str(r#"[{"tag_name":"v9.0.0", "draft":true}]"#).unwrap();
        assert_eq!(select_latest_release(draft, "self", None), None);
        let settings = UpdaterSettings {
            xray_repo: "https://github.com/zxc-rv/XKeen-UI".into(),
            mihomo_repo: "https://github.com/zxc-rv/XKeen-UI".into(),
            ..UpdaterSettings::default()
        };
        assert_eq!(
            get_repo(&settings, "self").unwrap(),
            crate::release_source::UI_RELEASE_SOURCE.repository
        );
    }

    #[test]
    fn core_selection_keeps_stable_and_mihomo_alpha_behaviour() {
        let releases = r#"[
            {"tag_name":"v9.0.0", "draft":true},
            {"tag_name":"Prerelease-Alpha", "prerelease":true, "assets":[{"name":"mihomo-linux-arm64-alpha-abc123.gz"}]},
            {"tag_name":"v1.0.0"}
        ]"#;
        for core in ["xray", "mihomo"] {
            assert_eq!(
                select_latest_release(serde_json::from_str(releases).unwrap(), core, Some("1.0.0")),
                Some(("1.0.0".into(), "v1.0.0".into()))
            );
        }
        assert_eq!(
            select_latest_release(serde_json::from_str(releases).unwrap(), "mihomo", Some("alpha-old")),
            Some(("alpha-abc123".into(), "Prerelease-Alpha".into()))
        );
    }

    #[test]
    fn slug_from_repo_url() {
        assert_eq!(repo_slug("https://github.com/XTLS/Xray-core"), "XTLS/Xray-core");
        assert_eq!(repo_slug("https://github.com/MetaCubeX/mihomo/"), "MetaCubeX/mihomo");
        assert_eq!(repo_slug("github.com/Shu1t3/XKeen-UI-Shu1t3"), "Shu1t3/XKeen-UI-Shu1t3");
        assert_eq!(repo_slug("XTLS/Xray-core"), "XTLS/Xray-core");
        assert_eq!(repo_slug("https://github.com/owner/repo.git"), "owner/repo");
    }

    #[test]
    fn repo_url_validation() {
        assert!(valid_repo_url("https://github.com/XTLS/Xray-core"));
        assert!(valid_repo_url("XTLS/Xray-core"));
        assert!(!valid_repo_url("https://github.com"));
        assert!(!valid_repo_url(""));
        assert!(!valid_repo_url("   "));
    }

    #[test]
    fn repo_from_settings() {
        let mut s = UpdaterSettings::default();
        assert_eq!(get_repo(&s, "xray").as_deref(), Some("XTLS/Xray-core"));
        assert_eq!(get_repo(&s, "mihomo").as_deref(), Some("MetaCubeX/mihomo"));
        assert_eq!(get_repo(&s, "self").as_deref(), Some("Shu1t3/XKeen-UI-Shu1t3"));

        s.xray_repo = "https://github.com/someone/xray-fork".into();
        assert_eq!(get_repo(&s, "xray").as_deref(), Some("someone/xray-fork"));

        s.xray_repo = "https://github.com".into();
        assert_eq!(get_repo(&s, "xray").as_deref(), Some("XTLS/Xray-core"));
    }

    #[test]
    fn picks_asset_by_arch_and_version() {
        let custom = vec![
            "prizrak-core-linux-arm64-v1.19.31.gz".to_string(),
            "prizrak-core-linux-arm64-compatible-v1.19.31.gz".to_string(),
            "prizrak-core-linux-mipsle-softfloat-v1.19.31.gz".to_string(),
            "prizrak-core-linux-mips-softfloat-v1.19.31.gz".to_string(),
            "prizrak-core-windows-arm64-v1.19.31.gz".to_string(),
            "prizrak-core-linux-arm64-v1.19.31.gz.sha256".to_string(),
        ];
        assert_eq!(
            pick_asset(&custom, "aarch64", "v1.19.31").as_deref(),
            Some("prizrak-core-linux-arm64-v1.19.31.gz")
        );
        if cfg!(target_endian = "little") {
            assert_eq!(
                pick_asset(&custom, "mips", "v1.19.31").as_deref(),
                Some("prizrak-core-linux-mipsle-softfloat-v1.19.31.gz")
            );
        } else {
            assert_eq!(
                pick_asset(&custom, "mips", "v1.19.31").as_deref(),
                Some("prizrak-core-linux-mips-softfloat-v1.19.31.gz")
            );
        }
        assert_eq!(pick_asset(&custom, "x86_64", "v1.19.31"), None);
    }

    #[test]
    fn picks_stock_assets() {
        let xray = vec![
            "Xray-linux-arm64-v8a.zip".to_string(),
            "Xray-linux-64.zip".to_string(),
            "Xray-windows-64.zip".to_string(),
            "Xray-macos-arm64.zip".to_string(),
            "geoip.dat".to_string(),
            "geosite.dat".to_string(),
        ];
        assert_eq!(
            pick_asset(&xray, "aarch64", "v25.9.6").as_deref(),
            Some("Xray-linux-arm64-v8a.zip")
        );

        let mihomo = vec![
            "mihomo-linux-arm64-v1.19.3.gz".to_string(),
            "mihomo-linux-arm64-compatible-v1.19.3.gz".to_string(),
            "mihomo-linux-mipsle-softfloat-v1.19.3.gz".to_string(),
            "mihomo-linux-64-v1.19.3.gz".to_string(),
        ];
        assert_eq!(
            pick_asset(&mihomo, "aarch64", "v1.19.3").as_deref(),
            Some("mihomo-linux-arm64-v1.19.3.gz")
        );

        let alpha = vec![
            "mihomo-linux-arm64-alpha-5a3f7c1e.deb".to_string(),
            "mihomo-linux-arm64-alpha-5a3f7c1e.rpm".to_string(),
            "mihomo-linux-arm64-alpha-5a3f7c1e.gz".to_string(),
            "mihomo-linux-mipsle-softfloat-alpha-5a3f7c1e.gz".to_string(),
        ];
        assert_eq!(
            pick_asset(&alpha, "aarch64", "Prerelease-Alpha").as_deref(),
            Some("mihomo-linux-arm64-alpha-5a3f7c1e.gz")
        );

        assert_eq!(
            pick_asset(&["Xray-linux-64.zip".to_string()], "aarch64", "v25.9.6"),
            None
        );
    }
}

#[cfg(test)]
mod integrity_tests {
    use super::*;
    #[tokio::test]
    async fn gateway_bytes_cannot_pass_without_matching_independently_trusted_hash() {
        use axum::{Router, routing::get};
        let app = Router::new().route("/artifact", get(|| async { "forged executable" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/artifact", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let path = std::env::temp_dir().join(format!("xkeen-gateway-{}", uuid::Uuid::new_v4()));
        let trusted = crate::release_integrity::fixture_asset(url.clone(), b"genuine executable");
        let error = match download_with_trusted_asset(trusted, &[], &path).await {
            Ok(_) => panic!("accepted forged gateway executable"),
            Err(error) => error,
        };
        assert!(error.contains("SHA-256"));
        assert!(!path.exists());
        let trusted = crate::release_integrity::fixture_asset(url, b"forged executable");
        assert!(download_with_trusted_asset(trusted, &[], &path).await.is_ok());
        server.abort();
    }
}
