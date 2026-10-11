//! First installation only: published upstream release, verified before execution.
use crate::{controller, logger::log, types::*, update_transaction::Workspace};
use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{Read, Seek, SeekFrom},
    os::unix::fs::PermissionsExt,
    path::{Component, Path},
    process::Stdio,
    sync::Mutex,
    time::Duration,
};
use tokio::{fs, process::Command};

const BIN: &str = opt_path!("/sbin/xkeen");
const MODULES: &str = opt_path!("/sbin/.xkeen");
const INSTALL_LOG: &str = opt_path!("/var/log/xkeen-install.log");
static JOB: Mutex<Job> = Mutex::new(Job {
    phase: Phase::Idle,
    error: None,
});

#[derive(Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Idle,
    Running,
    Complete,
    Failed,
}
#[derive(Clone, Serialize)]
struct Job {
    phase: Phase,
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cores {
    Xray,
    Mihomo,
    All,
    None,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    cores: Cores,
    #[serde(default)]
    geo: bool,
    #[serde(default)]
    autostart: bool,
    #[serde(default)]
    rci_token: String,
}
impl InstallRequest {
    fn args(&self) -> Vec<&'static str> {
        vec![
            "-i",
            "auto",
            match self.cores {
                Cores::Xray => "cores=xray",
                Cores::Mihomo => "cores=mihomo",
                Cores::All => "cores=all",
                Cores::None => "cores=none",
            },
            if self.geo { "geo=on" } else { "geo=off" },
            "geoipset=off",
            "cron=off",
            if self.autostart {
                "autostart=on"
            } else {
                "autostart=off"
            },
        ]
    }
}
pub(crate) fn is_running() -> bool {
    JOB.lock().unwrap().phase == Phase::Running
}

pub(crate) fn is_installed() -> bool {
    Path::new(BIN).is_file()
        && Path::new(MODULES).is_dir()
        && controller::find_init_file(false).is_some_and(|p| Path::new(&p).is_file())
}
fn preflight(request: &InstallRequest) -> Result<(), String> {
    if request.rci_token.len() > 4096 || request.rci_token.chars().any(char::is_control) {
        return Err("Некорректный RCI-токен".into());
    }
    if !request.rci_token.is_empty() && Path::new(XKEEN_CONF).exists() {
        return Err("xkeen.json уже существует. Сохраните RCI-токен в нём и оставьте поле токена пустым".into());
    }
    if cfg!(feature = "local-dev") {
        return Err("Установка доступна только на роутере".into());
    }
    if !nix::unistd::Uid::effective().is_root() || !Path::new(opt_path!("/bin/opkg")).is_file() {
        return Err("Нужны права root и установленная среда Entware".into());
    }
    if Path::new(BIN).exists()
        || Path::new(MODULES).exists()
        || controller::find_init_file(false).is_some_and(|p| Path::new(&p).exists())
    {
        return Err("Найдена существующая или частичная установка XKeen. Первичная установка не заменяет её; используйте SSH для восстановления или обновления".into());
    }
    if matches!(request.cores, Cores::None)
        && !Path::new(opt_path!("/sbin/xray")).is_file()
        && !Path::new(opt_path!("/sbin/mihomo")).is_file()
    {
        return Err("Для установки без ядер сначала установите Xray или Mihomo".into());
    }
    Ok(())
}

pub async fn get_status() -> Json<Value> {
    let job = JOB.lock().unwrap().clone();
    let output = if job.phase == Phase::Idle {
        String::new()
    } else {
        tokio::task::spawn_blocking(|| -> std::io::Result<String> {
            let mut file = std::fs::File::open(INSTALL_LOG)?;
            let size = file.metadata()?.len();
            file.seek(SeekFrom::Start(size.saturating_sub(24 * 1024)))?;
            let mut bytes = Vec::new();
            file.take(24 * 1024).read_to_end(&mut bytes)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
    };
    Json(
        json!({"success": true, "installed": is_installed(), "phase": job.phase, "error": job.error, "output": output, "config_exists": Path::new(XKEEN_CONF).exists(), "partial": Path::new(BIN).exists() || Path::new(MODULES).exists()}),
    )
}

pub async fn post_install(State(state): State<AppState>, Json(request): Json<InstallRequest>) -> Json<Value> {
    // Reserve before spawning; a second POST cannot create another job.
    {
        let mut job = JOB.lock().unwrap();
        if job.phase == Phase::Running {
            return Json(json!({"success": false, "error": "Установка уже выполняется"}));
        }
        if let Err(error) = preflight(&request) {
            return Json(json!({"success": false, "error": error}));
        }
        *job = Job {
            phase: Phase::Running,
            error: None,
        };
    }
    tokio::spawn(async move {
        let result = crate::update_transaction::run_to_completion(install(state, request))
            .await
            .unwrap_or_else(|e| Err(format!("Задача установки: {e}")));
        let error = result.err();
        log(
            if error.is_some() { "ERROR" } else { "INFO" },
            error
                .clone()
                .unwrap_or_else(|| "XKeen установлен. Настройте конфигурацию перед запуском".into()),
        );
        *JOB.lock().unwrap() = Job {
            phase: if error.is_some() {
                Phase::Failed
            } else {
                Phase::Complete
            },
            error,
        };
    });
    Json(json!({"success": true, "pending": true}))
}

fn unpack(archive: &Path, stage: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut size = 0u64;
    let mut count = 0usize;
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err("Небезопасный путь в архиве XKeen".into());
        }
        let normalized: std::path::PathBuf = path.components().filter(|c| *c != Component::CurDir).collect();
        if normalized.as_os_str().is_empty() {
            continue;
        }
        let kind = entry.header().entry_type();
        if !(normalized == Path::new("xkeen") || normalized.starts_with("_xkeen")) || !(kind.is_file() || kind.is_dir())
        {
            return Err("Недопустимое содержимое архива XKeen".into());
        }
        size = size.checked_add(entry.size()).ok_or("Слишком большой архив XKeen")?;
        count += 1;
        if size > 32 * 1024 * 1024 || count > 4096 {
            return Err("Архив XKeen превышает лимит распаковки".into());
        }
        if !entry.unpack_in(stage).map_err(|e| e.to_string())? {
            return Err("Путь вне staging".into());
        }
    }
    if !stage.join("xkeen").is_file() || !stage.join("_xkeen").is_dir() {
        return Err("В архиве нет XKeen и его модулей".into());
    }
    Ok(())
}

async fn install(state: AppState, request: InstallRequest) -> Result<(), String> {
    let _control = controller::CONTROL_LOCK
        .try_lock()
        .map_err(|_| "Другая операция управления уже выполняется")?;
    preflight(&request)?;
    let work = Workspace::create(Path::new(opt_path!("/sbin")))?;
    fs::create_dir_all(opt_path!("/var/log"))
        .await
        .map_err(|e| e.to_string())?;
    // Truncate only after we own both locks, so rejected jobs keep their previous log.
    fs::write(
        INSTALL_LOG,
        "Загрузка стабильного релиза jameszeroX/XKeen и проверка SHA-256…\n",
    )
    .await
    .map_err(|e| e.to_string())?;
    let proxies = state.settings.read().unwrap().updater.github_proxy.clone();
    let archive = work.path.join("xkeen.tar.gz");
    crate::updater::download_xkeen(&proxies, &archive).await?;
    let stage = work.path.clone();
    tokio::task::spawn_blocking(move || unpack(&archive, &stage))
        .await
        .map_err(|e| e.to_string())??;
    fs::set_permissions(work.path.join("xkeen"), std::fs::Permissions::from_mode(0o755))
        .await
        .map_err(|e| e.to_string())?;
    // curl is used during module import, before upstream installs its dependencies.
    run_command(opt_path!("/bin/opkg"), &["update"]).await?;
    run_command(opt_path!("/bin/opkg"), &["install", "curl", "tar"]).await?;
    if !request.rci_token.is_empty() {
        use tokio::io::AsyncWriteExt;
        fs::create_dir_all(XKEEN_CONF_DIR).await.map_err(|e| e.to_string())?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(XKEEN_CONF)
            .await
            .map_err(|e| e.to_string())?;
        let data = serde_json::to_vec_pretty(&json!({"xkeen": {"rci_token": request.rci_token}}))
            .map_err(|e| e.to_string())?;
        file.write_all(&data).await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
    }
    fs::rename(work.path.join("_xkeen"), MODULES)
        .await
        .map_err(|e| e.to_string())?;
    if let Err(e) = fs::rename(work.path.join("xkeen"), BIN).await {
        let _ = fs::remove_dir_all(MODULES).await;
        return Err(e.to_string());
    }
    let result = run_command(BIN, &request.args()).await;
    // Upstream can partially install dependencies/configs: preserve them for SSH recovery.
    *state.init_file.write().unwrap() = controller::find_init_file(false);
    *state.core.write().unwrap() = crate::detect_core(state.init_file.read().unwrap().as_deref());
    *state.rci_token.write().unwrap() = load_rci_token();
    result?;
    if !is_installed() || !Path::new(XKEEN_CONF).is_file() {
        return Err("Установщик завершился, но файлы сервиса XKeen не найдены; проверьте журнал".into());
    }
    let has_core = match request.cores {
        Cores::Xray => Path::new(opt_path!("/sbin/xray")).is_file(),
        Cores::Mihomo => Path::new(opt_path!("/sbin/mihomo")).is_file(),
        Cores::All => Path::new(opt_path!("/sbin/xray")).is_file() && Path::new(opt_path!("/sbin/mihomo")).is_file(),
        Cores::None => Path::new(opt_path!("/sbin/xray")).is_file() || Path::new(opt_path!("/sbin/mihomo")).is_file(),
    };
    if !has_core {
        return Err("Установлены не все выбранные ядра; проверьте журнал".into());
    }
    Ok(())
}

async fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
    let output = std::fs::OpenOptions::new()
        .append(true)
        .open(INSTALL_LOG)
        .map_err(|e| e.to_string())?;
    let child = Command::new(program)
        .args(args)
        .env("PATH", "/opt/sbin:/opt/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .current_dir(opt_path!("/sbin"))
        .stdin(Stdio::null())
        .stdout(output.try_clone().map_err(|e| e.to_string())?)
        .stderr(output)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    wait_installer(child, Duration::from_secs(1800)).await
}

async fn wait_installer(mut child: tokio::process::Child, timeout: Duration) -> Result<(), String> {
    let pid = child.id();
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(result) => Err(format!(
            "Установка XKeen завершилась с ошибкой: {result:?}. Частично установленные файлы сохранены; восстановление через SSH"
        )),
        Err(_) => {
            if let Some(pid) = pid {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(format!(
                "Истекло время установки ({} с). Частичные файлы сохранены; проверьте журнал и восстановите установку через SSH",
                timeout.as_secs()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_parameters_are_typed_and_do_not_accept_shell_input() {
        let request: InstallRequest = serde_json::from_value(json!({"cores":"mihomo", "geo":true})).unwrap();
        assert_eq!(
            request.args(),
            [
                "-i",
                "auto",
                "cores=mihomo",
                "geo=on",
                "geoipset=off",
                "cron=off",
                "autostart=off"
            ]
        );
        for value in [
            json!({"cores":"xray; reboot"}),
            json!({"cores":"xray", "url":"https://example.com"}),
        ] {
            assert!(serde_json::from_value::<InstallRequest>(value).is_err());
        }
    }

    #[tokio::test]
    async fn installer_exit_and_timeout_are_not_reported_as_success() {
        for (script, duration, success) in [
            ("exit 0", Duration::from_secs(2), true),
            ("exit 3", Duration::from_secs(2), false),
            ("exec sleep 10", Duration::from_millis(50), false),
        ] {
            let child = Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            assert_eq!(wait_installer(child, duration).await.is_ok(), success);
        }
    }

    fn archive(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gzip);
        for (name, kind, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(*kind);
            if kind.is_symlink() {
                header.set_link_name("/etc/passwd").unwrap();
            }
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn unpack_accepts_upstream_layout_and_rejects_links_unrelated_files_and_incomplete_archives() {
        let root = std::env::temp_dir().join(format!("xkeen-install-test-{}", uuid::Uuid::new_v4()));
        let work = Workspace::create(&root).unwrap();
        let file = work.path.join("download.tar.gz");
        let stage = work.path.join("unpacked");
        for entries in [
            vec![
                ("xkeen", tar::EntryType::Regular, b"#!/bin/sh\n".as_slice()),
                ("_xkeen/import.sh", tar::EntryType::Regular, b"# modules".as_slice()),
            ],
            vec![("xkeen", tar::EntryType::Symlink, b"".as_slice())],
            vec![("etc/passwd", tar::EntryType::Regular, b"bad".as_slice())],
            vec![("xkeen", tar::EntryType::Regular, b"incomplete".as_slice())],
        ] {
            std::fs::create_dir(&stage).unwrap();
            std::fs::write(&file, archive(&entries)).unwrap();
            let valid = entries.len() == 2;
            assert_eq!(unpack(&file, &stage).is_ok(), valid);
            std::fs::remove_dir_all(&stage).unwrap();
        }
        drop(work);
        std::fs::remove_dir_all(root).unwrap();
    }
}
