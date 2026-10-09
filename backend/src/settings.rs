use crate::logger::log;
use crate::types::*;
use axum::extract::State;
use axum::response::{IntoResponse, Json};

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
        serde_json::json!({ "success": true, "gui": s.gui, "updater": s.updater, "log": s.log, "clash_api": s.clash_api, "auth": { "enabled": s.auth.enabled }, "plugins": s.plugins }),
    )
}

pub async fn patch_settings(State(state): State<AppState>, Json(patch): Json<serde_json::Value>) -> impl IntoResponse {
    let _guard = state.app_config_lock.lock().await;
    let mut file_json = tokio::fs::read_to_string(APP_CONFIG)
        .await
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or(serde_json::json!({}));

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
    settings.normalize_proxies();

    *state.settings.write().unwrap() = settings;
    state.auth_changes.send_modify(|version| *version = version.wrapping_add(1));

    if let Err(e) = tokio::fs::create_dir_all(XKEEN_CONF_DIR).await {
        return Json(serde_json::json!({"success": false, "error": e.to_string()}));
    }

    let serialized = serde_json::to_string_pretty(&file_json).unwrap();
    let tmp = format!("{}.tmp", APP_CONFIG);
    match tokio::fs::write(&tmp, &serialized).await {
        Ok(_) => match tokio::fs::rename(&tmp, APP_CONFIG).await {
            Ok(_) => Json(serde_json::json!({"success": true})),
            Err(e) => Json(serde_json::json!({"success": false, "error": e.to_string()})),
        },
        Err(e) => Json(serde_json::json!({"success": false, "error": e.to_string()})),
    }
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
