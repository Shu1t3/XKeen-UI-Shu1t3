use crate::logger::log;
use crate::types::*;
use axum::extract::State;
use axum::response::{IntoResponse, Json};
use nix::sys::resource::{Resource, setrlimit};
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Gid, Pid, setgid, setsid};
use serde::Deserialize;
use std::path::Path;

use tokio::fs;
use tokio::process::Command;

static CONTROL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Deserialize)]
pub struct ControlReq {
    action: String,
    #[serde(default)]
    core: Option<Core>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Core {
    Xray,
    Mihomo,
}

impl Core {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "xray" => Ok(Self::Xray),
            "mihomo" => Ok(Self::Mihomo),
            _ => Err("Допустимые ядра: xray, mihomo".into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Xray => "xray",
            Self::Mihomo => "mihomo",
        }
    }

    fn executable(self) -> &'static str {
        match self {
            Self::Xray => opt_path!("/sbin/xray"),
            Self::Mihomo => opt_path!("/sbin/mihomo"),
        }
    }
}

impl ControlReq {
    fn requested_core(&self) -> Result<Core, String> {
        self.core.ok_or_else(|| "Укажите ядро: xray или mihomo".into())
    }
}

pub fn find_init_file(log_enabled: bool) -> Option<String> {
    let (mut path, mut source) = (None, "fallback");

    if let Ok(content) = std::fs::read_to_string(opt_path!("/sbin/.xkeen/01_info/01_info_variable.sh")) {
        let (mut dir, mut file) = (None, None);
        for line in content.lines() {
            let clean = line.split('#').next().unwrap_or("").trim();
            if let Some(v) = clean.strip_prefix("initd_dir=") {
                dir = Some(v.trim_matches(&['"', '\''][..]));
            } else if let Some(v) = clean.strip_prefix("initd_file=") {
                file = Some(v.trim_matches(&['"', '\''][..]));
            }
        }
        if let (Some(d), Some(f)) = (dir, file) {
            path = Some(f.replace("$initd_dir", d));
            source = "var";
        }
    }

    let final_path = path.or_else(|| {
        [S99XKEEN, S24XRAY]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .map(String::from)
    });

    if log_enabled
        && let Some(p) = &final_path {
        println!("{} [INFO] Defined initd_file ({}): {}", crate::logger::ts(), source, p);
    }

    final_path
}

async fn resolve_init_file(state: &AppState) -> Result<String, String> {
    if let Some(path) = state.init_file.read().unwrap().clone()
        && Path::new(&path).exists() {
            return Ok(path);
        }
    let new_path = tokio::task::spawn_blocking(|| find_init_file(false))
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Не найден init файл XKeen".to_string())?;
    println!("{} [INFO] Updated initd_file: {}", crate::logger::ts(), new_path);
    *state.init_file.write().unwrap() = Some(new_path.clone());
    Ok(new_path)
}

pub(crate) fn init_timeout_for_args(args: &[&str]) -> u64 {
    // Operations that configure iptables, ipset, and spawn cores take 40-60s on single-core MIPS (e.g. KN-1713).
    // Differentiate timeout: 120s for start/restart, 30s for quick stop/status operations.
    if args.iter().any(|&a| a == "start" || a == "restart") {
        120
    } else {
        30
    }
}

pub async fn run_init_command(state: &AppState, args: &[&str]) -> Result<(), String> {
    let path = resolve_init_file(state).await?;
    let mut command = Command::new(&path);
    command.args(args).kill_on_drop(true);
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true).append(true).open(error_log_path())
    {
        command.stdout(f.try_clone().map_err(|e| e.to_string())?).stderr(f);
    }
    let timeout_secs = init_timeout_for_args(args);
    let result = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), command.status())
        .await.map_err(|_| format!("Таймаут init ({timeout_secs} с): {path}"))?;
    match result {
        Ok(status) if status.success() => {
            if matches!(args.first(), Some(&"start") | Some(&"restart")) {
                let core = state.core.read().unwrap().name.clone();
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let probe_core = core.clone();
                let running = tokio::task::spawn_blocking(move || !get_pid(&probe_core).is_empty())
                    .await.map_err(|e| e.to_string())?;
                ensure_running(&core, running)?;
                flush_network_state().await;
            }
            Ok(())
        },
        Ok(status) => Err(format!("{path}: {status}")),
        Err(e) => {
            *state.init_file.write().unwrap() = None;
            Err(format!("{path}: {e}"))
        }
    }
}

pub async fn flush_network_state() {
    let _ = Command::new("ip")
        .args(["route", "flush", "cache"])
        .output()
        .await;

    let conntrack_binaries = [
        opt_path!("/sbin/conntrack"),
        opt_path!("/usr/sbin/conntrack"),
        "/usr/sbin/conntrack",
        "/sbin/conntrack",
        "conntrack",
    ];

    for bin in conntrack_binaries {
        if bin == "conntrack" || Path::new(bin).exists() {
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                Command::new(bin).arg("-F").output(),
            )
            .await;
            if let Ok(Ok(out)) = res
                && out.status.success()
            {
                break;
            }
        }
    }
}

async fn stop_core_gracefully(name: &str) {
    let pids = get_pid(name);
    if pids.is_empty() {
        return;
    }
    for &pid in &pids {
        _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        if get_pid(name).is_empty() {
            return;
        }
    }
    for &pid in &get_pid(name) {
        _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

fn ensure_running(core: &str, running: bool) -> Result<(), String> {
    if running { Ok(()) } else { Err(format!("Init завершился успешно, но ядро {core} не запущено")) }
}

fn get_core_info(name: &str) -> CoreInfo {
    match name {
        "mihomo" => CoreInfo {
            name: "mihomo".into(),
            conf_dir: MIHOMO_CONF_DIR.into(),
            is_json: false,
        },
        _ => CoreInfo {
            name: "xray".into(),
            conf_dir: XRAY_CONF_DIR.into(),
            is_json: true,
        },
    }
}

pub fn get_pids_for(names: &[&str]) -> std::collections::HashMap<String, Vec<i32>> {
    let mut map: std::collections::HashMap<String, Vec<i32>> = names
        .iter()
        .map(|n| (n.to_string(), Vec::new()))
        .collect();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            let comm = comm.trim_end_matches('\n');
            if let Some(list) = map.get_mut(comm) {
                list.push(pid);
            }
        }
    }
    map
}

pub fn get_pid(name: &str) -> Vec<i32> {
    get_pids_for(&[name]).remove(name).unwrap_or_default()
}

pub async fn soft_restart(core: &str) -> Result<(), String> {
    soft_restart_core(Core::parse(core)?).await
}

async fn soft_restart_core(core: Core) -> Result<(), String> {
    stop_core_gracefully(core.name()).await;

    let mut cmd = Command::new(core.executable());
    match core {
        Core::Mihomo => {
            cmd.env("CLASH_HOME_DIR", MIHOMO_CONF_DIR);
            cmd.args(["-d", MIHOMO_CONF_DIR]);
        }
        Core::Xray => {
            cmd.envs([
                ("XRAY_LOCATION_CONFDIR", XRAY_CONF_DIR),
                ("XRAY_LOCATION_ASSET", XRAY_ASSET_DIR),
            ]);
            cmd.args(["run", "-confdir", XRAY_CONF_DIR]);
        }
    }

    let lim = if cfg!(target_arch = "aarch64") { 40000 } else { 10000 };
    unsafe {
        cmd.pre_exec(move || {
            setsid()?;
            setgid(Gid::from_raw(11111))?;
            setrlimit(Resource::RLIMIT_NOFILE, lim, lim)?;
            Ok(())
        });
    }

    if let Ok(f) = std::fs::File::options()
        .append(true)
        .create(true)
        .open(error_log_path())
    {
        cmd.stdout(f.try_clone().map_err(|e| e.to_string())?).stderr(f);
    }

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    confirm_started(&mut child, core.name(), std::time::Duration::from_secs(3)).await?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    flush_network_state().await;

    Ok(())
}

async fn confirm_started(
    child: &mut tokio::process::Child, name: &str, window: std::time::Duration,
) -> Result<(), String> {
    tokio::time::sleep(window).await;
    match child.try_wait() {
        Ok(Some(status)) => Err(format!("Не удалось перезапустить {name}: {status}")),
        Err(e) => Err(format!("Не удалось проверить процесс {name}: {e}")),
        Ok(None) => Ok(()),
    }
}

pub async fn get_control(State(state): State<AppState>) -> impl IntoResponse {
    let _control_guard = CONTROL_LOCK.lock().await;

    // Single-pass check of both cores in /proc avoids repeated scanning on weak MIPS CPUs.
    let running_map = tokio::task::spawn_blocking(|| get_pids_for(&["xray", "mihomo"]))
        .await
        .unwrap_or_default();
    let xray_running = running_map.get("xray").is_some_and(|pids| !pids.is_empty());
    let mihomo_running = running_map.get("mihomo").is_some_and(|pids| !pids.is_empty());

    let mut current_core = state.core.read().unwrap().clone();
    let is_current_running = if current_core.name == "mihomo" { mihomo_running } else { xray_running };

    if !is_current_running {
        let (alt_core, is_alt_running) = if current_core.name == "mihomo" {
            ("xray", xray_running)
        } else {
            ("mihomo", mihomo_running)
        };

        current_core = if is_alt_running {
            get_core_info(alt_core)
        } else {
            let configuration = {
                let path = state.init_file.read().unwrap().clone();
                if let Some(p) = path {
                    tokio::fs::read_to_string(p).await.unwrap_or_default()
                } else {
                    String::new()
                }
            };
            get_core_info(if configuration.contains("name_client=\"mihomo\"") {
                "mihomo"
            } else {
                "xray"
            })
        };
        *state.core.write().unwrap() = current_core.clone();
    }

    let (xray_exists, mihomo_exists) = tokio::join!(
        async { tokio::fs::metadata(opt_path!("/sbin/xray")).await.is_ok() },
        async { tokio::fs::metadata(opt_path!("/sbin/mihomo")).await.is_ok() }
    );

    let mut available_cores = Vec::new();
    if xray_exists {
        available_cores.push("xray".to_string());
    }
    if mihomo_exists {
        available_cores.push("mihomo".to_string());
    }
    let running_status = (xray_exists && xray_running) || (mihomo_exists && mihomo_running);

    Json(
        serde_json::json!({ "success": true, "cores": available_cores, "currentCore": current_core.name, "running": running_status }),
    )
}

async fn check_core_config(core: &str) -> Result<(), String> {
    if Core::parse(core)? == Core::Xray {
        fs::create_dir_all(XRAY_CONF_DIR).await.ok();
        let has_json = std::fs::read_dir(XRAY_CONF_DIR)
            .map(|dir| {
                dir.flatten()
                    .any(|e| e.path().extension().is_some_and(|x| x == "json"))
            })
            .unwrap_or(false);
        if !has_json {
            return Err(
                "Не найдены конфигурационные файлы. Настройте их в /opt/etc/xray/configs перед запуском".into(),
            );
        }
    }
    Ok(())
}

async fn switch_core(state: &AppState, init_file: &str, old: &str, new: &str) -> Result<(), String> {
    let mut work = crate::update_transaction::Workspace::create(Path::new(opt_path!("/sbin")))?;
    let was_running = !get_pid(old).is_empty();
    let candidate = Core::parse(new)?;
    let resolved_init = fs::canonicalize(init_file).await.map_err(|e| e.to_string())?;
    crate::core_switch::transact(
        &resolved_init, (old, new), was_running, &mut work,
        || async {
            check_core_config(new).await?;
            crate::update_transaction::check_elf(Path::new(candidate.executable()), std::env::consts::ARCH, cfg!(target_endian = "little"))?;
            crate::update_transaction::check_candidate_commands(Path::new(candidate.executable()), new, "").await
        },
        |action, health| async move {
            let args: &[&str] = if action == "start" { &["start", "on"] } else { &["stop"] };
            run_init_command(state, args).await?;
            if !health && (!get_pid("xray").is_empty() || !get_pid("mihomo").is_empty()) {
                return Err("Ядро не остановлено".into());
            }
            Ok(())
        },
        |name| { *state.core.write().unwrap() = get_core_info(name); },
    ).await
}

pub async fn post_control(State(state): State<AppState>, Json(req): Json<ControlReq>) -> impl IntoResponse {
    // The detached task owns the guard and recovery even if the HTTP waiter disconnects.
    let result = crate::update_transaction::run_to_completion(async move {
        let _control_guard = CONTROL_LOCK.lock().await;
        post_control_inner(state, req).await
    }).await;
    match result {
        Ok(response) => response,
        Err(e) => Json(ApiResponse { success: false, error: Some(e.to_string()), data: None }),
    }
}

async fn post_control_inner(state: AppState, req: ControlReq) -> Json<ApiResponse<()>> {
    match req.action.as_str() {
        "switchCore" => {
            let core = match req.requested_core() {
                Ok(core) => core.name(),
                Err(e) => {
                    return Json(ApiResponse {
                        success: false,
                        error: Some(e),
                        data: None,
                    });
                }
            };
            let old = state.core.read().unwrap().name.clone();
            if old == core {
                return Json(ApiResponse {
                    success: true,
                    error: None,
                    data: None,
                });
            }

            let init_file = match resolve_init_file(&state).await {
                Ok(p) => p,
                Err(e) => {
                    return Json(ApiResponse {
                        success: false,
                        error: Some(e),
                        data: None,
                    });
                }
            };
            let result = switch_core(&state, &init_file, &old, core).await;
            if let Err(e) = result {
                log("ERROR", e.clone());
                return Json(ApiResponse { success: false, error: Some(e), data: None });
            }
        }
        "softRestart" => {
            let core = match req.requested_core() {
                Ok(core) => core,
                Err(e) => {
                    return Json(ApiResponse {
                        success: false,
                        error: Some(e),
                        data: None,
                    });
                }
            };
            if let Err(e) = soft_restart_core(core).await {
                return Json(ApiResponse {
                    success: false,
                    error: Some(e),
                    data: None,
                });
            }
        }
        a if ["start", "stop", "hardRestart"].contains(&a) => {
            let arg = match a {
                "start" => "start",
                "stop" => "stop",
                _ => "restart",
            };

            let cur_name = state.core.read().unwrap().name.clone();
            if (a == "start" || a == "hardRestart")
                && let Err(e) = check_core_config(&cur_name).await {
                log("ERROR", e);
                return Json(ApiResponse {
                    success: false,
                    error: Some(format!(
                        "Не удалось запустить {}{}",
                        cur_name[..1].to_uppercase(),
                        &cur_name[1..]
                    )),
                    data: None,
                });
            }
            if cur_name == "mihomo" && (a == "start" || a == "hardRestart") {
                _ = fs::write(error_log_path(), b"").await;
            }

            let args: &[&str] = match a {
                "start" => &["start", "on"],
                "hardRestart" => &["restart", "on"],
                _ => &[arg],
            };

            if let Err(e) = run_init_command(&state, args).await {
                return Json(ApiResponse {
                    success: false,
                    error: Some(e),
                    data: None,
                });
            }
        }
        _ => {
            return Json(ApiResponse {
                success: false,
                error: Some("Bad action".into()),
                data: None,
            });
        }
    }
    Json(ApiResponse::<()> {
        success: true,
        error: None,
        data: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::post};
    use std::sync::{Arc, RwLock};
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use tokio::fs::set_permissions;
    use tokio::sync::{Mutex, broadcast};

    #[test]
    fn core_names_and_executables_are_fixed() {
        for (name, core, executable) in [
            ("xray", Core::Xray, opt_path!("/sbin/xray")),
            ("mihomo", Core::Mihomo, opt_path!("/sbin/mihomo")),
        ] {
            assert_eq!(Core::parse(name).unwrap(), core);
            assert_eq!(core.name(), name);
            assert_eq!(core.executable(), executable);
            let req: ControlReq = serde_json::from_value(serde_json::json!({
                "action": "switchCore", "core": name
            }))
            .unwrap();
            assert_eq!(req.requested_core().unwrap(), core);
        }
    }

    #[tokio::test]
    async fn startup_confirmation_rejects_any_early_exit_including_success() {
        for exit in [0, 7] {
            let mut child = Command::new("sh")
                .args(["-c", &format!("exit {exit}")])
                .spawn()
                .unwrap();
            assert!(
                confirm_started(&mut child, "fixture", std::time::Duration::from_millis(50))
                    .await
                    .is_err()
            );
        }
        let mut child = Command::new("sh").args(["-c", "exec sleep 10"]).spawn().unwrap();
        assert!(
            confirm_started(&mut child, "fixture", std::time::Duration::from_millis(50))
                .await
                .is_ok()
        );
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn internal_restart_and_config_checks_reject_unknown_cores() {
        for core in ["", "sleep", "/bin/sh", "../xray", "Xray", "$(id)", "xray\n"] {
            assert!(soft_restart(core).await.is_err(), "{core:?}");
            assert!(check_core_config(core).await.is_err(), "{core:?}");
        }
    }

    #[tokio::test]
    async fn control_rejects_invalid_cores_before_touching_init_or_state() {
        let temp = std::env::temp_dir().join(format!("xkeen-control-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&temp).await.unwrap();
        let init = temp.join("init.sh");
        let executed = temp.join("executed");
        let injected = temp.join("injected");
        let content = format!(
            "#!/bin/sh\nname_client=\"xray\"\nprintf executed > '{}'\n",
            executed.display()
        );
        fs::write(&init, &content).await.unwrap();
        set_permissions(&init, Permissions::from_mode(0o755)).await.unwrap();
        let (log_tx, _) = broadcast::channel(16);
        let state = AppState {
            core: Arc::new(RwLock::new(get_core_info("xray"))),
            settings: Arc::new(RwLock::new(AppSettings::default())),
            init_file: Arc::new(RwLock::new(Some(init.to_str().unwrap().into()))),
            http_client: reqwest::Client::new(),
            update_checker: UpdateChecker::default(),
            geo_cache: Arc::new(RwLock::new(Default::default())),
            log_tx: Arc::new(log_tx),
            log_watcher: Arc::new(std::sync::Mutex::new(Default::default())),
            auth_changes: tokio::sync::watch::channel(0).0,
            app_config_lock: Arc::new(Mutex::new(())),
            debug: false,
            rci_token: Arc::new(RwLock::new(None)),
        };
        // Exercise the real JSON extractor and handler without local-dev's route blocker.
        let app = Router::new()
            .route("/api/control", post(post_control))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/control", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let invalid = [
            "".to_string(),
            "sleep".into(),
            "/bin/sh".into(),
            "../xray".into(),
            "Xray".into(),
            "xray\n".into(),
            "ядро".into(),
            format!("$(printf injected > '{}')", injected.display()),
            format!("`printf injected > '{}'`", injected.display()),
            format!("xray\"; printf injected > '{}'; #", injected.display()),
        ];
        for action in ["switchCore", "softRestart"] {
            for core in &invalid {
                let response = client
                    .post(&url)
                    .json(&serde_json::json!({
                        "action": action, "core": core
                    }))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                    "{action}: {core:?}"
                );
            }
            for body in [
                serde_json::json!({"action": action}),
                serde_json::json!({"action": action, "core": null}),
            ] {
                let response: serde_json::Value = client
                    .post(&url)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(response["success"], false);
            }
        }
        assert_eq!(state.core.read().unwrap().name, "xray");
        assert_eq!(fs::read_to_string(&init).await.unwrap(), content);
        assert_eq!(fs::metadata(&init).await.unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!executed.exists());
        assert!(!injected.exists());

        // Existing wire names still work; switching to the current core is a no-op.
        for core in ["xray", "mihomo"] {
            *state.core.write().unwrap() = get_core_info(core);
            let response: serde_json::Value = client
                .post(&url)
                .json(&serde_json::json!({
                    "action": "switchCore", "core": core
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(response["success"], true);
        }
        // A real request cannot enter while another control transaction owns the lock.
        let guard = CONTROL_LOCK.lock().await;
        let waiting_client = client.clone();
        let waiting_url = url.clone();
        let mut waiting = tokio::spawn(async move {
            waiting_client.post(waiting_url).json(&serde_json::json!({
                "action": "switchCore", "core": "mihomo"
            })).send().await.unwrap()
        });
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), &mut waiting).await.is_err());
        drop(guard);
        let response: serde_json::Value = waiting.await.unwrap().json().await.unwrap();
        assert_eq!(response["success"], true);
        let req: ControlReq = serde_json::from_value(serde_json::json!({"action": "stop"})).unwrap();
        assert!(req.core.is_none());
        server.abort();
        let _ = server.await;
        fs::remove_dir_all(temp).await.unwrap();
    }

    fn fixture_state(init: &Path) -> AppState {
        let (log_tx, _) = broadcast::channel(16);
        AppState {
            core: Arc::new(RwLock::new(get_core_info("mihomo"))),
            settings: Arc::new(RwLock::new(AppSettings::default())),
            init_file: Arc::new(RwLock::new(Some(init.to_string_lossy().into_owned()))),
            http_client: reqwest::Client::new(),
            update_checker: UpdateChecker::default(),
            geo_cache: Arc::new(RwLock::new(Default::default())),
            log_tx: Arc::new(log_tx),
            log_watcher: Arc::new(std::sync::Mutex::new(Default::default())),
            auth_changes: tokio::sync::watch::channel(0).0,
            app_config_lock: Arc::new(Mutex::new(())),
            debug: false,
            rci_token: Arc::new(RwLock::new(None)),
        }
    }

    #[tokio::test]
    async fn control_api_reports_nonzero_init_exit_for_every_service_action() {
        let temp = std::env::temp_dir().join(format!("xkeen-init-exit-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&temp).await.unwrap();
        let init = temp.join("init");
        fs::write(&init, "#!/bin/sh\nexit 7\n").await.unwrap();
        set_permissions(&init, Permissions::from_mode(0o755)).await.unwrap();
        let state = fixture_state(&init);
        let app = Router::new().route("/api/control", post(post_control)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/control", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for action in ["start", "stop", "hardRestart"] {
            let response: serde_json::Value = reqwest::Client::new().post(&url)
                .json(&serde_json::json!({"action":action})).send().await.unwrap().json().await.unwrap();
            assert_eq!(response["success"], false, "{action}");
            assert!(response["error"].as_str().unwrap().contains("7"), "{response}");
        }
        server.abort();
        fs::remove_dir_all(temp).await.unwrap();
    }

    #[tokio::test]
    async fn successful_init_exit_without_core_process_is_not_successful_start_or_restart() {
        let temp = std::env::temp_dir().join(format!("xkeen-init-health-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&temp).await.unwrap();
        let init = temp.join("init");
        fs::write(&init, "#!/bin/sh\nexit 0\n").await.unwrap();
        set_permissions(&init, Permissions::from_mode(0o755)).await.unwrap();
        let state = fixture_state(&init);
        // Unique nonexistent comm makes the real /proc probe independent of installed cores.
        state.core.write().unwrap().name = format!("fixture-{}", uuid::Uuid::new_v4());
        for args in [["start", "on"], ["restart", "on"]] {
            let error = run_init_command(&state, &args).await.unwrap_err();
            assert!(error.contains("не запущено"), "{error}");
        }
        // Stop has no startup requirement; successful init exit remains accepted.
        run_init_command(&state, &["stop"]).await.unwrap();
        fs::remove_dir_all(temp).await.unwrap();
    }

    #[test]
    fn init_timeout_differentiates_heavy_and_quick_commands() {
        assert_eq!(init_timeout_for_args(&["start", "on"]), 120);
        assert_eq!(init_timeout_for_args(&["restart", "on"]), 120);
        assert_eq!(init_timeout_for_args(&["start"]), 120);
        assert_eq!(init_timeout_for_args(&["restart"]), 120);
        assert_eq!(init_timeout_for_args(&["stop"]), 30);
        assert_eq!(init_timeout_for_args(&["status"]), 30);
        assert_eq!(init_timeout_for_args(&[]), 30);
    }

    #[test]
    fn single_pass_proc_scanner_handles_multiple_names_and_empty_results() {
        let nonexistent_1 = format!("none-{}", uuid::Uuid::new_v4());
        let nonexistent_2 = format!("none-{}", uuid::Uuid::new_v4());
        let pids = get_pids_for(&[&nonexistent_1, &nonexistent_2]);
        assert_eq!(pids.len(), 2);
        assert!(pids[&nonexistent_1].is_empty());
        assert!(pids[&nonexistent_2].is_empty());
        assert!(get_pid(&nonexistent_1).is_empty());
    }

    #[tokio::test]
    async fn flush_network_state_runs_without_panic() {
        flush_network_state().await;
    }

    #[tokio::test]
    async fn stop_core_gracefully_handles_nonexistent_process() {
        stop_core_gracefully("nonexistent_core_process_xyz").await;
    }
}


