use crate::logger::log;
use crate::types::*;
use axum::extract::State;
use axum::response::{IntoResponse, Json};

#[cfg(test)]
tokio::task_local! {
    pub static TEST_SETTINGS_CONFIG_OVERRIDE: (std::path::PathBuf, std::path::PathBuf);
}

fn current_config_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    #[cfg(test)]
    {
        if let Ok(paths) = TEST_SETTINGS_CONFIG_OVERRIDE.try_with(|p| p.clone()) {
            return paths;
        }
    }
    (
        std::path::PathBuf::from(APP_CONFIG),
        std::path::PathBuf::from(XKEEN_CONF_DIR),
    )
}

pub fn load_settings() -> AppSettings {
    let (content, path) = match std::fs::read_to_string(APP_CONFIG) {
        Ok(c) => (c, APP_CONFIG),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Ok(c) = std::fs::read_to_string(APP_CONFIG_LEGACY) {
                if let Err(e) = std::fs::create_dir_all(XKEEN_CONF_DIR) {
                    log("WARN", format!("Не удалось создать {}: {}", XKEEN_CONF_DIR, e));
                }
                if std::fs::rename(APP_CONFIG_LEGACY, APP_CONFIG).is_ok() {
                    log(
                        "INFO",
                        format!("Успешная миграция конфига: {} -> {}", APP_CONFIG_LEGACY, APP_CONFIG),
                    );
                } else {
                    log("WARN", "Не удалось выполнить миграцию конфига".into());
                }
                (c, APP_CONFIG_LEGACY)
            } else {
                return AppSettings::default();
            }
        }
        Err(e) => {
            log("ERROR", format!("Ошибка чтения {}: {}", APP_CONFIG, e));
            return AppSettings::default();
        }
    };

    match serde_json::from_str::<AppSettings>(&content) {
        Ok(mut s) => {
            s.normalize_proxies();
            s
        }
        Err(e) => {
            log("ERROR", format!("Ошибка парсинга {}: {}", path, e));
            AppSettings::default()
        }
    }
}

pub async fn get_settings(State(state): State<AppState>) -> impl IntoResponse {
    let s = state.settings.read().unwrap();
    Json(
        serde_json::json!({ "success": true, "gui": s.gui, "updater": s.updater, "log": s.log, "clash_api": s.clash_api, "auth": { "enabled": s.auth.enabled }, "plugins": s.plugins, "dns": s.dns }),
    )
}

pub async fn patch_settings(State(state): State<AppState>, Json(patch): Json<serde_json::Value>) -> impl IntoResponse {
    let (config_path, conf_dir) = current_config_paths();
    patch_settings_at(&state, patch, &config_path, &conf_dir).await
}

pub async fn patch_settings_at(
    state: &AppState,
    patch: serde_json::Value,
    config_path: &std::path::Path,
    conf_dir: &std::path::Path,
) -> Json<serde_json::Value> {
    let _guard = state.app_config_lock.lock().await;
    let mut file_json = tokio::fs::read_to_string(config_path)
        .await
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| {
            let current = state.settings.read().unwrap();
            serde_json::to_value(&*current).unwrap_or(serde_json::json!({}))
        });

    if !file_json.is_object() {
        let current = state.settings.read().unwrap();
        file_json = serde_json::to_value(&*current).unwrap_or(serde_json::json!({}));
    }

    let patch_sets_tz = patch.get("log").and_then(|l| l.get("timezone")).is_some();

    json_merge(&mut file_json, patch);

    if let serde_json::Value::Object(ref mut map) = file_json
        && let Some(legacy) = map.remove("timezoneOffset")
        && !patch_sets_tz
        && let Some(log) = map.entry("log").or_insert(serde_json::json!({})).as_object_mut() {
        log.insert("timezone".into(), legacy);
    }

    let mut settings: AppSettings = match serde_json::from_value(file_json.clone()) {
        Ok(s) => s,
        Err(e) => return Json(serde_json::json!({"success": false, "error": e.to_string()})),
    };

    if settings.log.timezone < -12 || settings.log.timezone > 14 {
        return Json(serde_json::json!({"success": false, "error": "Неверный часовой пояс"}));
    }
    settings.clash_api.ping_url = settings.clash_api.ping_url.trim().to_string();
    if settings.clash_api.ping_url.is_empty() {
        return Json(serde_json::json!({"success": false, "error": "URL пинг-теста не может быть пустым"}));
    }
    if settings.clash_api.ping_timeout == 0 {
        return Json(serde_json::json!({"success": false, "error": "Таймаут пинг-теста должен быть больше 0"}));
    }
    if let Err(e) = settings.validate_plugins() {
        return Json(serde_json::json!({"success": false, "error": e}));
    }
    for (name, url) in [("Xray", &settings.updater.xray_repo), ("Mihomo", &settings.updater.mihomo_repo)] {
        if url.trim().is_empty() {
            return Json(
                serde_json::json!({"success": false, "error": format!("URL репозитория {} не может быть пустым", name)}),
            );
        }
        if !crate::updater::valid_repo_url(url) {
            return Json(
                serde_json::json!({"success": false, "error": format!("Некорректный URL репозитория {}", name)}),
            );
        }
    }
    for ip in &mut settings.dns.corporate_fallback {
        *ip = ip.trim().to_string();
        if ip.is_empty() {
            return Json(serde_json::json!({"success": false, "error": "Адрес DNS-сервера не может быть пустым"}));
        }
        if ip.parse::<std::net::IpAddr>().is_err() {
            return Json(serde_json::json!({"success": false, "error": format!("Некорректный IP-адрес DNS-сервера: '{ip}'")}));
        }
    }
    settings.normalize_proxies();

    if let Ok(updater_val) = serde_json::to_value(&settings.updater) {
        file_json["updater"] = updater_val;
    }
    if let Ok(clash_val) = serde_json::to_value(&settings.clash_api) {
        file_json["clash_api"] = clash_val;
    }
    if let Ok(dns_val) = serde_json::to_value(&settings.dns) {
        file_json["dns"] = dns_val;
    }

    if let Err(e) = tokio::fs::create_dir_all(conf_dir).await {
        return Json(serde_json::json!({"success": false, "error": e.to_string()}));
    }

    let serialized = match serde_json::to_string_pretty(&file_json) {
        Ok(s) => s,
        Err(e) => return Json(serde_json::json!({"success": false, "error": e.to_string()})),
    };

    let tmp = format!("{}.tmp", config_path.display());
    if let Err(e) = tokio::fs::write(&tmp, &serialized).await {
        return Json(serde_json::json!({"success": false, "error": e.to_string()}));
    }
    if let Err(e) = tokio::fs::rename(&tmp, config_path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Json(serde_json::json!({"success": false, "error": e.to_string()}));
    }

    // Disk commit confirmed: publish to in-memory state and notify subscribers
    *state.settings.write().unwrap() = settings;
    state.auth_changes.send_modify(|version| *version = version.wrapping_add(1));

    Json(serde_json::json!({"success": true}))
}

fn json_merge(a: &mut serde_json::Value, b: serde_json::Value) {
    match (a, b) {
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                json_merge(a.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    fn create_test_state() -> AppState {
        let (log_tx, _) = tokio::sync::broadcast::channel(16);
        AppState {
            core: std::sync::Arc::new(RwLock::new(crate::types::CoreInfo {
                name: "xray".into(),
                conf_dir: String::new(),
                is_json: true,
            })),
            settings: std::sync::Arc::new(RwLock::new(AppSettings::default())),
            init_file: std::sync::Arc::new(RwLock::new(None)),
            http_client: reqwest::Client::new(),
            update_checker: crate::types::UpdateChecker::default(),
            geo_cache: std::sync::Arc::new(RwLock::new(Default::default())),
            log_tx: std::sync::Arc::new(log_tx),
            log_watcher: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
            auth_changes: tokio::sync::watch::channel(0).0,
            app_config_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            enrollment_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            debug: false,
            rci_token: std::sync::Arc::new(RwLock::new(None)),
        }
    }

    #[tokio::test]
    async fn patch_settings_persists_before_publishing_and_normalizes() {
        let dir = std::env::temp_dir().join(format!("xkeen-settings-test-{}", uuid::Uuid::new_v4()));
        let config_path = dir.join("xkeen-ui.json");
        let conf_dir = dir.clone();
        let state = create_test_state();

        let patch = serde_json::json!({
            "updater": {
                "github_proxy": ["proxy.example.com", "https://proxy2.example.com"]
            },
            "clash_api": {
                "ping_url": "  https://cp.cloudflare.com/generate_204  "
            }
        });

        let mut auth_rx = state.auth_changes.subscribe();
        let res = patch_settings_at(&state, patch, &config_path, &conf_dir).await;
        assert_eq!(res.0.get("success").and_then(|v| v.as_bool()), Some(true));

        // Disk must contain the normalized values
        let disk_content = tokio::fs::read_to_string(&config_path).await.unwrap();
        let disk_json: serde_json::Value = serde_json::from_str(&disk_content).unwrap();
        assert_eq!(
            disk_json["updater"]["github_proxy"],
            serde_json::json!(["https://proxy.example.com", "https://proxy2.example.com"])
        );
        assert_eq!(
            disk_json["clash_api"]["ping_url"],
            serde_json::json!("https://cp.cloudflare.com/generate_204")
        );

        // Memory must match disk and subscribers notified
        {
            let mem = state.settings.read().unwrap();
            assert_eq!(
                mem.updater.github_proxy,
                vec!["https://proxy.example.com", "https://proxy2.example.com"]
            );
            assert_eq!(mem.clash_api.ping_url, "https://cp.cloudflare.com/generate_204");
        }
        assert_eq!(*auth_rx.borrow_and_update(), 1);

        _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn patch_settings_io_failure_preserves_memory_and_version() {
        let dir = std::env::temp_dir().join(format!("xkeen-settings-fail-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // Create a regular file where directory is expected so create_dir_all fails
        let blocker = dir.join("blocker_file");
        tokio::fs::write(&blocker, b"blocking file").await.unwrap();
        let config_path = blocker.join("xkeen-ui.json");
        let conf_dir = blocker.clone();

        let state = create_test_state();
        let mut initial_settings = AppSettings::default();
        initial_settings.clash_api.ping_url = "https://initial.test".into();
        *state.settings.write().unwrap() = initial_settings.clone();

        let patch = serde_json::json!({
            "clash_api": {
                "ping_url": "https://new.test"
            }
        });

        let mut auth_rx = state.auth_changes.subscribe();
        let res = patch_settings_at(&state, patch, &config_path, &conf_dir).await;
        assert_eq!(res.0.get("success").and_then(|v| v.as_bool()), Some(false));

        // In-memory settings must NOT have changed
        assert_eq!(state.settings.read().unwrap().clash_api.ping_url, "https://initial.test");
        // auth_changes must NOT have been incremented
        assert_eq!(*auth_rx.borrow_and_update(), 0);

        _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn patch_settings_handler_with_task_local_override() {
        let dir = std::env::temp_dir().join(format!("xkeen-settings-override-{}", uuid::Uuid::new_v4()));
        let config_path = dir.join("xkeen-ui.json");
        let conf_dir = dir.clone();
        let state = create_test_state();

        TEST_SETTINGS_CONFIG_OVERRIDE
            .scope((config_path.clone(), conf_dir), async {
                let patch = serde_json::json!({
                    "log": { "timezone": 3 }
                });
                let response = patch_settings(State(state.clone()), Json(patch)).await.into_response();
                let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
                assert_eq!(body_json["success"], true);
                assert_eq!(state.settings.read().unwrap().log.timezone, 3);
                assert!(config_path.exists());
            })
            .await;

        _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
