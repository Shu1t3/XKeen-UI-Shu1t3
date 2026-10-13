use crate::logger::log;
use crate::types::*;
use axum::extract::State;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::sync::{LazyLock, RwLock};
use tokio::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KillswitchStatus {
    pub enabled: bool,
    pub engaged: bool,
    pub mode: String,
    pub trigger_reason: Option<String>,
}

static KILLSWITCH_STATE: LazyLock<RwLock<KillswitchStatus>> = LazyLock::new(|| {
    RwLock::new(KillswitchStatus {
        enabled: false,
        engaged: false,
        mode: "blackhole".into(),
        trigger_reason: None,
    })
});

#[derive(Deserialize)]
pub struct KillswitchToggleReq {
    pub enabled: bool,
    #[serde(default)]
    pub mode: Option<String>,
}

pub fn is_killswitch_engaged() -> bool {
    KILLSWITCH_STATE.read().unwrap().engaged
}

pub fn is_killswitch_enabled() -> bool {
    KILLSWITCH_STATE.read().unwrap().enabled
}

pub async fn engage(reason: &str) -> Result<(), String> {
    {
        let mut guard = KILLSWITCH_STATE.write().unwrap();
        if guard.engaged {
            return Ok(());
        }
        guard.engaged = true;
        guard.trigger_reason = Some(reason.to_string());
    }

    log(
        "WARN",
        format!("Killswitch АКТИВИРОВАН: трафик заблокирован во избежание утечки. Причина: {reason}"),
    );

    if !cfg!(feature = "local-dev") && !cfg!(test) {
        let mode = KILLSWITCH_STATE.read().unwrap().mode.clone();
        if let Err(e) = apply_system_rules(&mode).await {
            log("ERROR", format!("Ошибка применения правил Killswitch: {e}"));
            return Err(e);
        }
    }
    Ok(())
}

pub async fn disengage() -> Result<(), String> {
    {
        let mut guard = KILLSWITCH_STATE.write().unwrap();
        if !guard.engaged {
            return Ok(());
        }
        guard.engaged = false;
        guard.trigger_reason = None;
    }

    log("INFO", "Killswitch ДЕАКТИВИРОВАН: штатная маршрутизация восстановлена".into());

    if !cfg!(feature = "local-dev") && !cfg!(test)
        && let Err(e) = remove_system_rules().await
    {
        log("ERROR", format!("Ошибка удаления правил Killswitch: {e}"));
        return Err(e);
    }
    Ok(())
}

pub async fn emergency_reset() -> Result<(), String> {
    {
        let mut guard = KILLSWITCH_STATE.write().unwrap();
        guard.engaged = false;
        guard.trigger_reason = None;
    }

    log("INFO", "Killswitch: экстренный сброс блокировок (failsafe reset)".into());

    if !cfg!(feature = "local-dev") && !cfg!(test) {
        remove_system_rules().await?;
    }
    Ok(())
}

pub async fn sync_settings(settings: &KillswitchSettings, is_core_running: bool) {
    let (prev_enabled, was_engaged) = {
        let mut guard = KILLSWITCH_STATE.write().unwrap();
        let prev = guard.enabled;
        let engaged = guard.engaged;
        guard.enabled = settings.enabled;
        guard.mode = if settings.mode == "firewall" {
            "firewall".into()
        } else {
            "blackhole".into()
        };
        (prev, engaged)
    };

    if prev_enabled != settings.enabled {
        log(
            "INFO",
            format!(
                "Killswitch: режим {}",
                if settings.enabled { "включен" } else { "отключен" }
            ),
        );
    }

    if !settings.enabled {
        if was_engaged {
            _ = disengage().await;
        }
    } else if !is_core_running && !was_engaged {
        _ = engage("Служба прокси не запущена").await;
    }
}

async fn apply_system_rules(_mode: &str) -> Result<(), String> {
    // 1. Создаем цепочку XKEEN_KILLSWITCH в таблице filter
    _ = Command::new("iptables").args(["-N", "XKEEN_KILLSWITCH"]).output().await;
    _ = Command::new("iptables").args(["-F", "XKEEN_KILLSWITCH"]).output().await;

    // 2. Белый список локальных сетей (RFC1918) и служебных адресов (LAN гарантированно доступна)
    for subnet in [
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "224.0.0.0/4",
    ] {
        let _ = Command::new("iptables")
            .args(["-A", "XKEEN_KILLSWITCH", "-d", subnet, "-j", "RETURN"])
            .output()
            .await;
    }

    // 3. Белый список портов управления роутером (Web UI 80/443, SSH 22/222, панель XKeen-UI 1000/11000, DNS 53)
    let _ = Command::new("iptables")
        .args([
            "-A",
            "XKEEN_KILLSWITCH",
            "-p",
            "tcp",
            "-m",
            "multiport",
            "--dports",
            "80,443,22,222,1000,11000,53",
            "-j",
            "RETURN",
        ])
        .output()
        .await;

    let _ = Command::new("iptables")
        .args(["-A", "XKEEN_KILLSWITCH", "-p", "udp", "--dport", "53", "-j", "RETURN"])
        .output()
        .await;

    // 4. Запрет транзитного трафика через WAN с немедленным отклонением (ICMP prohibited)
    let _ = Command::new("iptables")
        .args(["-A", "XKEEN_KILLSWITCH", "-j", "REJECT", "--reject-with", "icmp-net-prohibited"])
        .output()
        .await;

    // 5. Встраивание в цепочку FORWARD на первой позиции
    let check = Command::new("iptables")
        .args(["-C", "FORWARD", "-j", "XKEEN_KILLSWITCH"])
        .output()
        .await;

    if check.map(|c| !c.status.success()).unwrap_or(true) {
        let _ = Command::new("iptables")
            .args(["-I", "FORWARD", "1", "-j", "XKEEN_KILLSWITCH"])
            .output()
            .await;
    }

    // 6. Таблица маршрутизации 100: заменяем маршрут по умолчанию на unreachable
    let _ = Command::new("ip")
        .args(["route", "replace", "unreachable", "default", "table", "100"])
        .output()
        .await;

    let _ = Command::new("ip").args(["route", "flush", "cache"]).output().await;

    Ok(())
}

async fn remove_system_rules() -> Result<(), String> {
    // 1. Удаление вызова из FORWARD
    let _ = Command::new("iptables")
        .args(["-D", "FORWARD", "-j", "XKEEN_KILLSWITCH"])
        .output()
        .await;

    // 2. Очистка и удаление цепочки
    let _ = Command::new("iptables").args(["-F", "XKEEN_KILLSWITCH"]).output().await;
    let _ = Command::new("iptables").args(["-X", "XKEEN_KILLSWITCH"]).output().await;

    // 3. Удаление unreachable маршрута из таблицы 100
    let _ = Command::new("ip")
        .args(["route", "del", "unreachable", "default", "table", "100"])
        .output()
        .await;

    let _ = Command::new("ip").args(["route", "flush", "cache"]).output().await;

    Ok(())
}

// ─── HTTP API Handlers ────────────────────────────────────────────────────────

pub async fn get_killswitch_status(State(state): State<AppState>) -> impl IntoResponse {
    let settings_guard = state.settings.read().unwrap();
    let current_status = KILLSWITCH_STATE.read().unwrap().clone();

    Json(serde_json::json!({
        "success": true,
        "enabled": settings_guard.killswitch.enabled,
        "engaged": current_status.engaged,
        "mode": settings_guard.killswitch.mode,
        "trigger_reason": current_status.trigger_reason,
    }))
}

pub async fn post_killswitch(
    State(state): State<AppState>,
    Json(req): Json<KillswitchToggleReq>,
) -> impl IntoResponse {
    let mut patch = serde_json::json!({
        "killswitch": {
            "enabled": req.enabled,
        }
    });

    if let Some(ref mode) = req.mode {
        patch["killswitch"]["mode"] = serde_json::Value::String(mode.clone());
    }

    let res = crate::settings::patch_settings(State(state.clone()), Json(patch)).await;

    // Проверяем статус запущенного ядра
    let core_name = state.core.read().unwrap().name.clone();
    let is_running = tokio::task::spawn_blocking(move || !crate::controller::get_pid(&core_name).is_empty())
        .await
        .unwrap_or(false);

    let current_ks_settings = state.settings.read().unwrap().killswitch.clone();
    sync_settings(&current_ks_settings, is_running).await;

    res
}

pub async fn reset_killswitch(State(_state): State<AppState>) -> impl IntoResponse {
    match emergency_reset().await {
        Ok(()) => Json(serde_json::json!({ "success": true })),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_killswitch_toggle_and_state() {
        // Reset state before test
        _ = emergency_reset().await;
        assert!(!is_killswitch_engaged());

        // Engage
        engage("Тестовое падение").await.unwrap();
        assert!(is_killswitch_engaged());
        {
            let guard = KILLSWITCH_STATE.read().unwrap();
            assert_eq!(guard.trigger_reason.as_deref(), Some("Тестовое падение"));
        }

        // Disengage
        disengage().await.unwrap();
        assert!(!is_killswitch_engaged());
        {
            let guard = KILLSWITCH_STATE.read().unwrap();
            assert!(guard.trigger_reason.is_none());
        }
    }

    #[tokio::test]
    async fn test_sync_settings_with_core_status() {
        _ = emergency_reset().await;

        let ks_settings = KillswitchSettings {
            enabled: true,
            mode: "blackhole".into(),
        };

        // If core is not running, sync_settings should engage
        sync_settings(&ks_settings, false).await;
        assert!(is_killswitch_engaged());

        // If core is running, sync_settings when disabled should disengage
        let ks_disabled = KillswitchSettings {
            enabled: false,
            mode: "blackhole".into(),
        };
        sync_settings(&ks_disabled, true).await;
        assert!(!is_killswitch_engaged());
    }

    #[tokio::test]
    async fn test_emergency_reset_always_clears_engaged() {
        engage("Авария").await.unwrap();
        assert!(is_killswitch_engaged());

        emergency_reset().await.unwrap();
        assert!(!is_killswitch_engaged());
    }
}
