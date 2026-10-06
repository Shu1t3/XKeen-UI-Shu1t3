use futures_util::FutureExt;
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::{fs, process::Command};

pub struct Workspace {
    pub path: PathBuf,
    lock: PathBuf,
    preserve: bool,
}

impl Workspace {
    pub fn create(parent: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let lock = parent.join(".xkeen-ui-update.lock");
        std::fs::create_dir(&lock).map_err(|e| format!("Обновление занято или lock недоступен: {e}"))?;
        let path = parent.join(format!(".xkeen-ui-stage.{}", uuid::Uuid::new_v4()));
        let setup = (|| -> std::io::Result<()> {
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o700))?;
            std::fs::create_dir(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
            std::fs::write(lock.join("owner"), format!("{}\n", path.display()))?;
            Ok(())
        })();
        if let Err(e) = setup {
            let _ = std::fs::remove_dir_all(&path);
            let _ = std::fs::remove_file(lock.join("owner"));
            let _ = std::fs::remove_dir(&lock);
            return Err(format!("Ошибка staging: {e}"));
        }
        Ok(Self {
            path,
            lock,
            preserve: false,
        })
    }

    pub fn preserve(&mut self) {
        self.preserve = true;
    }
    pub fn lock_path(&self) -> &Path {
        &self.lock
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if self.preserve {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.path);
        if std::fs::read_to_string(self.lock.join("owner"))
            .is_ok_and(|owner| owner.trim() == self.path.to_string_lossy())
        {
            let _ = std::fs::remove_file(self.lock.join("owner"));
            let _ = std::fs::remove_dir(&self.lock);
        }
    }
}

pub fn check_elf(path: &Path, arch: &str, little_endian: bool) -> Result<(), String> {
    let mut header = [0u8; 20];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|e| format!("Не удалось прочитать ELF: {e}"))?;
    let (class, machine) = match arch {
        "aarch64" => (2, 183),
        "mips" => (1, 8),
        _ => return Err("Архитектура не поддерживается".into()),
    };
    let endian = if little_endian { 1 } else { 2 };
    let actual_machine = if little_endian {
        u16::from_le_bytes([header[18], header[19]])
    } else {
        u16::from_be_bytes([header[18], header[19]])
    };
    if header[..4] != [0x7f, b'E', b'L', b'F']
        || header[4] != class
        || header[5] != endian
        || header[6] != 1
        || actual_machine != machine
    {
        return Err("ELF не соответствует архитектуре/порядку байтов роутера".into());
    }
    Ok(())
}

async fn checked_output(cmd: &mut Command, duration: Duration) -> Result<String, String> {
    cmd.kill_on_drop(true);
    let out = tokio::time::timeout(duration, cmd.output())
        .await
        .map_err(|_| "Таймаут проверки бинарника".to_string())?
        .map_err(|e| format!("Бинарник не запускается: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Проверка бинарника: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn preflight(path: &Path, core: &str, version: &str) -> Result<(), String> {
    check_elf(path, std::env::consts::ARCH, cfg!(target_endian = "little"))?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .await
        .map_err(|e| e.to_string())?;
    check_candidate_commands(path, core, version).await
}

pub(crate) async fn check_candidate_commands(path: &Path, core: &str, version: &str) -> Result<(), String> {
    let args: &[&str] = match core {
        "self" => &["--version"],
        "xray" => &["version"],
        "mihomo" => &["-v"],
        _ => return Err("Неизвестное ядро".into()),
    };
    let output = checked_output(Command::new(path).args(args), Duration::from_secs(15)).await?;
    if output.trim().is_empty() {
        return Err("Бинарник не вернул версию".into());
    }
    if core == "self"
        && !output
            .split_whitespace()
            .any(|v| v.trim_start_matches('v') == version.trim_start_matches('v'))
    {
        return Err("Версия бинарника не совпадает с выбранным релизом".into());
    }
    match core {
        "xray" => {
            checked_output(
                Command::new(path)
                    .args(["run", "-test", "-confdir", crate::types::XRAY_CONF_DIR])
                    .env("XRAY_LOCATION_ASSET", crate::types::XRAY_ASSET_DIR),
                Duration::from_secs(30),
            )
            .await?;
        }
        "mihomo" => {
            checked_output(
                Command::new(path).args(["-t", "-d", crate::types::MIHOMO_CONF_DIR]),
                Duration::from_secs(30),
            )
            .await?;
        }
        _ => (),
    }
    Ok(())
}

pub async fn backup(target: &Path, work: &Workspace) -> Result<bool, String> {
    match fs::symlink_metadata(target).await {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => (),
        Ok(_) => return Err("Целевой бинарник должен быть обычным файлом".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.to_string()),
    }
    let old = work.path.join("old");
    fs::copy(target, &old)
        .await
        .map_err(|e| format!("Не удалось сохранить рабочий бинарник: {e}"))?;
    fs::File::open(&old)
        .await
        .map_err(|e| e.to_string())?
        .sync_all()
        .await
        .map_err(|e| e.to_string())?;
    fs::File::open(&work.path)
        .await
        .map_err(|e| e.to_string())?
        .sync_all()
        .await
        .map_err(|e| e.to_string())?;
    Ok(true)
}

async fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .await
        .map_err(|e| e.to_string())?
        .sync_all()
        .await
        .map_err(|e| e.to_string())
}

pub async fn replace_core<F, Fut>(
    work: &mut Workspace, target: &Path, running: bool, mut restart: F,
) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    fs::File::open(work.path.join("new"))
        .await
        .map_err(|e| e.to_string())?
        .sync_all()
        .await
        .map_err(|e| e.to_string())?;
    let had_old = backup(target, work).await?;
    if running && !had_old {
        return Err("Рабочее ядро запущено, но его бинарник недоступен для backup".into());
    }
    // If an unexpected panic happens after replacement, Drop must retain recovery files.
    work.preserve();
    if let Err(e) = fs::rename(work.path.join("new"), target).await {
        work.preserve = false;
        return Err(format!("Атомарная замена не удалась, рабочий файл сохранён: {e}"));
    }
    let result = match sync_directory(target.parent().unwrap()).await {
        Err(e) => Err(format!("Ошибка фиксации замены: {e}")),
        Ok(()) if running => attempt_restart(&mut restart).await,
        Ok(()) => Ok(()),
    };
    if let Err(error) = result {
        let restore = if had_old {
            match fs::copy(work.path.join("old"), work.path.join("restore")).await {
                Ok(_) => {
                    async {
                        fs::File::open(work.path.join("restore")).await?.sync_all().await?;
                        fs::rename(work.path.join("restore"), target).await
                    }
                    .await
                }
                Err(e) => Err(e),
            }
        } else {
            fs::remove_file(target).await
        };
        if let Err(e) = restore {
            work.preserve();
            return Err(format!(
                "{error}; rollback не удался: {e}; резервная копия: {}",
                work.path.join("old").display()
            ));
        }
        if let Err(e) = sync_directory(target.parent().unwrap()).await {
            return Err(format!(
                "{error}; rollback выполнен, но sync не удался: {e}; backup: {}",
                work.path.join("old").display()
            ));
        }
        if running && had_old
            && let Err(e) = attempt_restart(&mut restart).await {
            work.preserve();
            return Err(format!(
                "{error}; старый бинарник восстановлен, но не запущен: {e}; lock: {}",
                work.lock.display()
            ));
        }
        work.preserve = false;
        return Err(format!("{error}; предыдущая версия восстановлена"));
    }
    work.preserve = false;
    Ok(())
}

async fn attempt_restart<F, Fut>(restart: &mut F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(restart))
        .map_err(|_| "Ошибка запуска (panic)".to_string())?;
    std::panic::AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|_| "Ошибка запуска (panic)".to_string())?
}

pub async fn start_self_update(work: &mut Workspace, target: &Path, init: &Path, port: &str) -> Result<String, String> {
    if !init.is_file() {
        return Err("Init-скрипт панели не найден; обновление отменено".into());
    }
    // Both tools are needed by the independent recovery supervisor.
    for tool in ["curl", "pidof", "readlink"] {
        checked_output(
            Command::new("sh").args(["-c", &format!("command -v {tool}")]),
            Duration::from_secs(5),
        )
        .await?;
    }
    if !backup(target, work).await? {
        return Err("Рабочий бинарник панели не найден".into());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let results = target.parent().unwrap().join(".xkeen-ui-update-results");
    fs::create_dir_all(&results).await.map_err(|e| e.to_string())?;
    fs::set_permissions(&results, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|e| e.to_string())?;
    let status = results.join(format!("{id}.json"));
    fs::write(
        &status,
        serde_json::to_vec(&serde_json::json!({"job_id": id, "state":"pending", "error":""})).unwrap(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let script = work.path.join("supervisor.sh");
    fs::write(&script, include_str!("self_update.sh"))
        .await
        .map_err(|e| e.to_string())?;
    let logfile = std::fs::File::create(results.join(format!("{id}.log"))).map_err(|e| e.to_string())?;
    let mut command = Command::new("sh");
    command
        .arg(&script)
        .arg(target)
        .arg(init)
        .arg(&work.path)
        .arg(work.lock_path())
        .arg(status)
        .arg(&id)
        .arg(port)
        .arg("/proc")
        .stdin(std::process::Stdio::null())
        .stdout(logfile.try_clone().map_err(|e| e.to_string())?)
        .stderr(logfile);
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Не удалось запустить supervisor: {e}"))?;
    work.preserve(); // The supervisor now owns staging, backup and lock, including recovery.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(id)
}

#[derive(serde::Deserialize)]
pub struct StatusQuery {
    job_id: String,
}

pub async fn update_status(axum::extract::Query(query): axum::extract::Query<StatusQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Ok(id) = uuid::Uuid::parse_str(&query.job_id) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let path = Path::new(opt_path!("/sbin"))
        .join(".xkeen-ui-update-results")
        .join(format!("{id}.json"));
    let value = fs::read(&path)
        .await
        .ok()
        .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok());
    match value {
        Some(value) => axum::Json(value).into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

// Keeping the JoinHandle separate lets recovery finish after the HTTP waiter is dropped.
pub async fn run_to_completion<F, T>(future: F) -> Result<T, tokio::task::JoinError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(future).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("xkeen-transaction-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn prepare(dir: &TestDir) -> (Workspace, PathBuf) {
        let work = Workspace::create(&dir.0).unwrap();
        let target = dir.0.join("core");
        std::fs::write(&target, b"OLD").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(work.path.join("new"), b"NEW").unwrap();
        (work, target)
    }

    #[test]
    fn elf_rejects_wrong_architecture_endian_class_and_truncation() {
        let dir = TestDir::new();
        let file = dir.0.join("candidate");
        for (arch, little, class, machine) in
            [("aarch64", true, 2, 183u16), ("mips", true, 1, 8), ("mips", false, 1, 8)]
        {
            let mut header = [0u8; 20];
            header[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
            header[4] = class;
            header[5] = if little { 1 } else { 2 };
            header[6] = 1;
            header[18..20].copy_from_slice(&if little {
                machine.to_le_bytes()
            } else {
                machine.to_be_bytes()
            });
            std::fs::write(&file, header).unwrap();
            assert!(check_elf(&file, arch, little).is_ok());
            assert!(check_elf(&file, arch, !little).is_err());
            header[4] = 0;
            std::fs::write(&file, header).unwrap();
            assert!(check_elf(&file, arch, little).is_err());
        }
        std::fs::write(&file, b"PARTIAL").unwrap();
        assert!(check_elf(&file, "aarch64", true).is_err());
    }

    #[test]
    fn workspace_serializes_updates_and_uses_unique_same_directory_staging() {
        let dir = TestDir::new();
        let first = Workspace::create(&dir.0).unwrap();
        let path = first.path.clone();
        assert_eq!(path.parent(), Some(dir.0.as_path()));
        assert!(Workspace::create(&dir.0).is_err());
        assert_eq!(
            std::fs::read_to_string(first.lock.join("owner")).unwrap().trim(),
            path.to_str().unwrap()
        );
        drop(first);
        assert!(!path.exists());
        let second = Workspace::create(&dir.0).unwrap();
        assert_ne!(path, second.path);
    }

    #[tokio::test]
    async fn failed_backup_or_rename_never_touches_working_binary_or_process() {
        let dir = TestDir::new();
        let (mut work, target) = prepare(&dir);
        fs::create_dir(work.path.join("old")).await.unwrap();
        let mut restarts = 0;
        let error = replace_core(&mut work, &target, true, || {
            restarts += 1;
            async { Ok(()) }
        })
        .await
        .unwrap_err();
        assert!(error.contains("сохранить"));
        assert_eq!(fs::read(&target).await.unwrap(), b"OLD");
        assert_eq!(restarts, 0);
        fs::remove_dir(work.path.join("old")).await.unwrap();
        fs::remove_file(work.path.join("new")).await.unwrap();
        assert!(
            replace_core(&mut work, &target, true, || {
                restarts += 1;
                async { Ok(()) }
            })
            .await
            .is_err()
        );
        assert_eq!(fs::read(&target).await.unwrap(), b"OLD");
        assert_eq!(restarts, 0);
    }

    #[tokio::test]
    async fn running_core_without_a_backup_source_is_not_stopped() {
        let dir = TestDir::new();
        let (mut work, target) = prepare(&dir);
        fs::remove_file(&target).await.unwrap();
        let error = replace_core(&mut work, &target, true, || async {
            panic!("must not stop the running core")
        })
        .await
        .unwrap_err();
        assert!(error.contains("backup"));
        assert!(work.path.join("new").exists());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn failed_new_start_restores_old_binary_permissions_and_restarts_it() {
        let dir = TestDir::new();
        let (mut work, target) = prepare(&dir);
        let mut calls = 0;
        let error = replace_core(&mut work, &target, true, || {
            calls += 1;
            let installed = std::fs::read(&target).unwrap();
            async move {
                if installed == b"NEW" {
                    Err("new startup failed".into())
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap_err();
        assert!(error.contains("предыдущая версия восстановлена"));
        assert_eq!(calls, 2);
        assert_eq!(fs::read(&target).await.unwrap(), b"OLD");
        assert_eq!(fs::metadata(&target).await.unwrap().permissions().mode() & 0o777, 0o755);
        let lock = work.lock.clone();
        drop(work);
        assert!(!lock.exists());
    }

    #[tokio::test]
    async fn failed_restore_retains_backup_and_lock_for_manual_recovery() {
        let dir = TestDir::new();
        let (mut work, target) = prepare(&dir);
        let error = replace_core(&mut work, &target, true, || {
            std::fs::remove_file(&target).unwrap();
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("occupied"), b"obstacle").unwrap();
            async { Err("startup failed".into()) }
        })
        .await
        .unwrap_err();
        assert!(error.contains("rollback не удался"));
        let path = work.path.clone();
        let lock = work.lock.clone();
        drop(work);
        assert_eq!(fs::read(path.join("old")).await.unwrap(), b"OLD");
        assert!(lock.exists());
        assert!(Workspace::create(&dir.0).is_err());
    }

    #[tokio::test]
    async fn failed_old_restart_keeps_restored_binary_backup_and_lock() {
        let dir = TestDir::new();
        let (mut work, target) = prepare(&dir);
        let mut calls = 0;
        assert!(
            replace_core(&mut work, &target, true, || {
                calls += 1;
                async { Err("start failed".into()) }
            })
            .await
            .is_err()
        );
        assert_eq!(calls, 2);
        assert_eq!(fs::read(&target).await.unwrap(), b"OLD");
        let path = work.path.clone();
        let lock = work.lock.clone();
        drop(work);
        assert_eq!(fs::read(path.join("old")).await.unwrap(), b"OLD");
        assert!(lock.exists());
    }

    #[tokio::test]
    async fn startup_panic_also_restores_and_restarts_old_version() {
        let dir = TestDir::new();
        let (mut work, target) = prepare(&dir);
        let mut calls = 0;
        let error = replace_core(&mut work, &target, true, || {
            calls += 1;
            let first = calls == 1;
            async move {
                assert!(!first, "injected startup panic");
                Ok(())
            }
        })
        .await
        .unwrap_err();
        assert!(error.contains("panic"));
        assert_eq!(calls, 2);
        assert_eq!(fs::read(target).await.unwrap(), b"OLD");
    }

    #[tokio::test]
    async fn stopped_core_is_replaced_without_starting_it() {
        let dir = TestDir::new();
        let (mut work, target) = prepare(&dir);
        replace_core(&mut work, &target, false, || async {
            panic!("stopped core must stay stopped")
        })
        .await
        .unwrap();
        assert_eq!(fs::read(target).await.unwrap(), b"NEW");
    }

    #[tokio::test]
    async fn cancelled_http_waiter_does_not_cancel_failed_start_recovery() {
        let dir = TestDir::new();
        let (mut work, target) = prepare(&dir);
        let saved_target = target.clone();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let gate_inner = gate.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(run_to_completion(async move {
            let mut started = Some(started_tx);
            let result = replace_core(&mut work, &target, true, || {
                let call = calls_inner.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    started.take().unwrap().send(()).unwrap();
                }
                let gate = gate_inner.clone();
                async move {
                    if call == 0 {
                        let _permit = gate.acquire().await.unwrap();
                        Err("new startup failed".into())
                    } else {
                        Ok(())
                    }
                }
            })
            .await;
            done_tx.send(result).unwrap();
        }));
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        gate.add_permits(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), done_rx)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(fs::read(saved_target).await.unwrap(), b"OLD");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn preflight_rejects_bad_configs_and_wrong_panel_version_before_replacement() {
        let dir = TestDir::new();
        let candidate = dir.0.join("candidate");
        let live = dir.0.join("live");
        fs::write(&live, b"OLD").await.unwrap();
        fs::write(
            &candidate,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$0.args"
case "$1" in
  version) printf 'Xray 1.2.3\n' ;;
  -v) printf 'Mihomo 1.2.3\n' ;;
  --version) printf 'XKeen UI v1.2.3\n' ;;
  run|-t) printf 'invalid configuration\n' >&2; exit 7 ;;
  *) exit 9 ;;
esac
"#,
        )
        .await
        .unwrap();
        fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
        for core in ["xray", "mihomo"] {
            assert!(
                check_candidate_commands(&candidate, core, "v1.2.3")
                    .await
                    .unwrap_err()
                    .contains("invalid configuration")
            );
            assert_eq!(fs::read(&live).await.unwrap(), b"OLD");
        }
        assert!(check_candidate_commands(&candidate, "self", "v1.2.3").await.is_ok());
        assert!(
            check_candidate_commands(&candidate, "self", "v9.9.9")
                .await
                .unwrap_err()
                .contains("Версия")
        );
        let calls = fs::read_to_string(candidate.with_extension("args")).await.unwrap();
        assert!(calls.contains(&format!("run -test -confdir {}", crate::types::XRAY_CONF_DIR)));
        assert!(calls.contains(&format!("-t -d {}", crate::types::MIHOMO_CONF_DIR)));
        assert_eq!(fs::read(&live).await.unwrap(), b"OLD");
    }

    #[tokio::test]
    async fn command_failure_and_timeout_fail_closed() {
        assert!(
            checked_output(
                Command::new("sh").args(["-c", "printf error >&2; exit 7"]),
                Duration::from_secs(1)
            )
            .await
            .unwrap_err()
            .contains("error")
        );
        assert!(
            checked_output(
                Command::new("sh").args(["-c", "exec sleep 10"]),
                Duration::from_millis(50)
            )
            .await
            .unwrap_err()
            .contains("Таймаут")
        );
        assert_eq!(
            checked_output(
                Command::new("sh").args(["-c", "printf version"]),
                Duration::from_secs(1)
            )
            .await
            .unwrap(),
            "version"
        );
    }
}
