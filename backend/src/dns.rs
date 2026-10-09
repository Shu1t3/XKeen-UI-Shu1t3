use crate::logger::log;
use crate::types::*;
use axum::extract::State;
use axum::response::{IntoResponse, Json};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

#[derive(Deserialize)]
pub struct DnsEnableReq {
    pub config_content: String,
    #[serde(default = "default_true")]
    pub setup_filter: bool,
}

#[derive(Deserialize)]
pub struct DnsDeleteReq {}

fn default_true() -> bool {
    true
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsStatusFields {
    pub dns_override: bool,
    pub dns_mihomo: bool,
    pub provider_ignored: bool,
}

fn check_dns_mihomo() -> bool {
    if let Some(config_path) = find_mihomo_config()
        && let Ok(content) = std::fs::read_to_string(&config_path)
        && let Ok(yaml) = yaml_rust2::YamlLoader::load_from_str(&content)
        && let Some(doc) = yaml.first()
        && let Some(dns) = doc["dns"].as_hash() {
        let enable = dns
            .get(&yaml_rust2::Yaml::String("enable".into()))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let listen = dns
            .get(&yaml_rust2::Yaml::String("listen".into()))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return enable && listen == "0.0.0.0:53";
    }
    false
}

fn parse_dns_status(output: &str) -> DnsStatusFields {
    DnsStatusFields {
        dns_override: output.contains("opkg dns-override"),
        dns_mihomo: check_dns_mihomo(),
        provider_ignored: output.contains("ip no name-servers"),
    }
}

#[derive(serde::Serialize)]
pub struct DnsResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<DnsStatusFields>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanSegment {
    pub iface: String,
    pub ip: String,
}

pub fn parse_ip_addr_segments(stdout: &str) -> Vec<LanSegment> {
    let mut segments = Vec::new();
    let mut current_iface: Option<String> = None;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(first_char) = trimmed.chars().next()
            && first_char.is_ascii_digit()
            && let Some((_idx, rest)) = trimmed.split_once(':')
        {
            let iface_part = rest
                .trim_start()
                .split(&[':', ' ', '@'][..])
                .next()
                .unwrap_or("")
                .trim();
            if !iface_part.is_empty() {
                current_iface = Some(iface_part.to_string());
            }
        }

        if trimmed.starts_with("inet ") || trimmed.contains(" inet ") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            let mut iface_name = current_iface.clone().unwrap_or_default();
            let mut ip_str = None;

            for (i, &word) in parts.iter().enumerate() {
                if word == "inet" && i + 1 < parts.len() {
                    ip_str = parts[i + 1].split('/').next().map(String::from);
                }
                if i == 1 && !word.starts_with("inet") && iface_name.is_empty() {
                    iface_name = word.trim_end_matches(':').to_string();
                }
            }

            if let Some(ip) = ip_str
                && !iface_name.is_empty()
                && iface_name != "lo"
                && (iface_name.starts_with("br")
                    || iface_name.starts_with("bridge")
                    || is_private_ipv4(&ip))
            {
                segments.push(LanSegment {
                    iface: iface_name,
                    ip,
                });
            }
        }
    }

    segments.sort_by(|a, b| {
        let a_score = if a.iface == "br0" {
            0
        } else if a.iface.starts_with("br") {
            1
        } else {
            2
        };
        let b_score = if b.iface == "br0" {
            0
        } else if b.iface.starts_with("br") {
            1
        } else {
            2
        };
        a_score.cmp(&b_score).then_with(|| a.iface.cmp(&b.iface))
    });
    segments.dedup_by(|a, b| a.ip == b.ip);
    segments
}

fn is_private_ipv4(ip: &str) -> bool {
    if let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() {
        addr.is_private()
    } else {
        false
    }
}

pub fn get_all_segment_ips() -> Result<Vec<LanSegment>, String> {
    let output = std::process::Command::new("ip")
        .args(["-4", "a"])
        .output()
        .map_err(|e| format!("Ошибка выполнения ip: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let segments = parse_ip_addr_segments(&stdout);
    if segments.is_empty() {
        let br0_out = std::process::Command::new("ip")
            .args(["-4", "a", "s", "br0"])
            .output()
            .map_err(|e| format!("Ошибка выполнения ip: {e}"))?;
        let br0_stdout = String::from_utf8_lossy(&br0_out.stdout);
        let br0_segs = parse_ip_addr_segments(&br0_stdout);
        if !br0_segs.is_empty() {
            return Ok(br0_segs);
        }
        return Err("Не удалось получить IP адреса сетевых сегментов".into());
    }
    Ok(segments)
}

#[allow(dead_code)]
pub fn get_br0_ip() -> Result<String, String> {
    let segs = get_all_segment_ips()?;
    segs.first()
        .map(|s| s.ip.clone())
        .ok_or_else(|| "Не удалось получить IP адрес br0".into())
}

async fn fetch_rci(state: &AppState, endpoint: &str) -> Result<serde_json::Value, String> {
    let mut req = state
        .http_client
        .get(format!("http://127.0.0.1:79/rci/show/{endpoint}"))
        .timeout(Duration::from_secs(5));
    let token = state.rci_token.read().unwrap().clone();
    if let Some(ref token) = token {
        req = req.header("X-Ndma-Tkn", token);
    }

    let response = req
        .send()
        .await
        .map_err(|e| format!("Ошибка запроса RCI ({endpoint}): {e}"))?;
    if !response.status().is_success() {
        return Err(format!("RCI ({endpoint}) вернул {}", response.status()));
    }

    response
        .json()
        .await
        .map_err(|e| format!("Ошибка парсинга RCI ({endpoint}): {e}"))
}

async fn fetch_running_config(state: &AppState) -> Result<String, String> {
    let data = fetch_rci(state, "running-config").await?;
    let lines = data
        .get("message")
        .and_then(|m| m.as_array())
        .ok_or_else(|| "В ответе RCI отсутствует массив 'message'".to_string())?;

    let config_str = lines
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<&str>>()
        .join("\n");

    Ok(config_str)
}

async fn req_rci(
    state: &AppState, method: Method, path: &str, payload: serde_json::Value, ignore_not_found: bool,
) -> Result<bool, String> {
    let mut req = state
        .http_client
        .request(method.clone(), format!("http://127.0.0.1:79/rci/{path}"))
        .json(&payload)
        .timeout(Duration::from_secs(5));

    let token = state.rci_token.read().unwrap().clone();
    if let Some(ref token) = token {
        req = req.header("X-Ndma-Tkn", token);
    }

    let response = req
        .send()
        .await
        .map_err(|e| format!("Ошибка {method} RCI (/{path}): {e}"))?;

    let status = response.status();
    if ignore_not_found && status == StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !status.is_success() {
        let err = response.text().await.unwrap_or_default();
        return Err(format!("RCI вернул код {}, ответ: {}", status, err));
    }

    Ok(true)
}

async fn run_rci_step(
    state: &AppState, step: &str, method: Method, path: &str, payload: serde_json::Value,
) -> Result<(), String> {
    let optional_component = match path {
        "dns-proxy/https/upstream" => Some("DoH"),
        "dns-proxy/tls/upstream" => Some("DoT"),
        _ => None,
    };

    match req_rci(state, method.clone(), path, payload, optional_component.is_some()).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            log(
                "WARN",
                format!(
                    "Не удалось очистить {} (компонент не установлен?)",
                    optional_component.unwrap_or_default()
                ),
            );
            Ok(())
        }
        Err(e) => {
            log("ERROR", format!("DNS: '{step}' ({method} /{path}) — ошибка: {e}"));
            Err(format!("{step}: {e}"))
        }
    }
}

fn find_mihomo_config() -> Option<String> {
    let dir = std::fs::read_dir(MIHOMO_CONF_DIR).ok()?;
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            return path.to_str().map(String::from);
        }
    }
    None
}

pub async fn get_dns(State(state): State<AppState>) -> impl IntoResponse {
    match fetch_running_config(&state).await {
        Ok(output) => Json(DnsResponse {
            success: true,
            error: None,
            status: Some(parse_dns_status(&output)),
        }),
        Err(e) => Json(DnsResponse {
            success: false,
            error: Some(e),
            status: None,
        }),
    }
}

pub async fn post_dns(State(state): State<AppState>, Json(req): Json<DnsEnableReq>) -> impl IntoResponse {
    let segments = match get_all_segment_ips() {
        Ok(segs) => segs,
        Err(e) => {
            log("ERROR", e.clone());
            return Json(DnsResponse {
                success: false,
                error: Some(e),
                status: None,
            });
        }
    };

    let mut steps: Vec<(&str, Method, &str, serde_json::Value)> = vec![
        (
            "Включение opkg dns-override",
            Method::POST,
            "opkg/dns-override",
            json!({}),
        ),
        (
            "Сохранение конфигурации",
            Method::POST,
            "system/configuration/save",
            json!({}),
        ),
    ];

    if req.setup_filter {
        steps.insert(
            0,
            (
                "Отключение HTTPS DNS-прокси",
                Method::DELETE,
                "dns-proxy/https/upstream",
                json!({}),
            ),
        );
        steps.insert(
            1,
            (
                "Отключение TLS DNS-прокси",
                Method::DELETE,
                "dns-proxy/tls/upstream",
                json!({}),
            ),
        );
        steps.insert(
            2,
            (
                "Сброс системных DNS-серверов",
                Method::DELETE,
                "ip/name-server",
                json!({}),
            ),
        );
        for seg in &segments {
            steps.push((
                "Установка name-server для сегмента",
                Method::POST,
                "ip/name-server",
                json!({"address": seg.ip, "port": 53}),
            ));
        }
    }

    for (step, method, path, payload) in &steps {
        if let Err(e) = run_rci_step(&state, step, method.clone(), path, payload.clone()).await {
            return Json(DnsResponse {
                success: false,
                error: Some(e),
                status: None,
            });
        }
        if step.contains("dns-override") {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    let content = req.config_content;
    let write_result = crate::config_transaction::run(async move {
        let config_path = find_mihomo_config().ok_or_else(|| "config.yaml не найден, конфигурация не записана".to_string())?;
        tokio::task::spawn_blocking(move || {
            crate::config_transaction::write_atomic(std::path::Path::new(&config_path), content.as_bytes(), false)
        }).await.map_err(|e| e.to_string())?
    }).await;
    match write_result {
        Ok(Ok(())) => (),
        Ok(Err(e)) => log("ERROR", format!("Ошибка записи config.yaml: {e}")),
        Err(e) => log("ERROR", format!("Ошибка записи config.yaml: {e}")),
    }

    let message = if req.setup_filter {
        let seg_list: Vec<String> = segments
            .iter()
            .map(|s| format!("{}:53 ({})", s.ip, s.iface))
            .collect();
        format!("Управление DNS включено, name-server установлен: {}", seg_list.join(", "))
    } else {
        "Управление DNS включено".into()
    };
    log("INFO", message);
    Json(DnsResponse {
        success: true,
        error: None,
        status: None,
    })
}

pub async fn delete_dns(State(state): State<AppState>, Json(_req): Json<DnsDeleteReq>) -> impl IntoResponse {
    let mut steps: Vec<(&str, Method, &str, serde_json::Value)> = vec![
        (
            "Отключение opkg dns-override",
            Method::DELETE,
            "opkg/dns-override",
            json!({}),
        ),
        (
            "Сохранение конфигурации",
            Method::POST,
            "system/configuration/save",
            json!({}),
        ),
    ];

    let segments = get_all_segment_ips().unwrap_or_default();
    let has_configured_dns = match fetch_running_config(&state).await {
        Ok(output) => {
            output.contains("opkg dns-override")
                || segments.iter().any(|s| output.contains(&format!("ip name-server {}", s.ip)))
                || output.contains("ip name-server 192.168.")
                || output.contains("ip name-server 10.")
                || output.contains("ip name-server 172.")
        }
        Err(_) => false,
    };

    if has_configured_dns {
        steps.insert(
            1,
            (
                "Сброс системных DNS-серверов",
                Method::DELETE,
                "ip/name-server",
                json!({}),
            ),
        );
        steps.insert(
            2,
            (
                "Установка name-server на 77.88.8.8",
                Method::POST,
                "ip/name-server",
                json!({"address": "77.88.8.8", "port": 53}),
            ),
        );
    }

    for (step, method, path, payload) in &steps {
        if let Err(e) = run_rci_step(&state, step, method.clone(), path, payload.clone()).await {
            return Json(DnsResponse {
                success: false,
                error: Some(e),
                status: None,
            });
        }
        if step.contains("dns-override") {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    let message = if has_configured_dns {
        "Управление DNS отключено, name-server установлен: 77.88.8.8:53"
    } else {
        "Управление DNS отключено"
    };
    log("INFO", message.into());
    Json(DnsResponse {
        success: true,
        error: None,
        status: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ip_addr_segments_detects_multiple_bridges() {
        let sample = r#"
1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN group default
    inet 127.0.0.1/8 scope host lo
       valid_lft forever preferred_lft forever
2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel state UP group default
    inet 192.168.0.50/24 brd 192.168.0.255 scope global eth0
       valid_lft forever preferred_lft forever
3: br0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP group default
    inet 192.168.1.1/24 brd 192.168.1.255 scope global br0
       valid_lft forever preferred_lft forever
4: br1: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP group default
    inet 192.168.2.1/24 brd 192.168.2.255 scope global br1
       valid_lft forever preferred_lft forever
5: br2: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue state UP group default
    inet 10.10.0.1/24 brd 10.10.0.255 scope global br2
       valid_lft forever preferred_lft forever
"#;
        let segments = parse_ip_addr_segments(sample);
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0], LanSegment { iface: "br0".into(), ip: "192.168.1.1".into() });
        assert_eq!(segments[1], LanSegment { iface: "br1".into(), ip: "192.168.2.1".into() });
        assert_eq!(segments[2], LanSegment { iface: "br2".into(), ip: "10.10.0.1".into() });
        assert_eq!(segments[3], LanSegment { iface: "eth0".into(), ip: "192.168.0.50".into() });
    }

    #[test]
    fn parse_ip_addr_segments_one_line_mode() {
        let sample = r#"
1: lo    inet 127.0.0.1/8 scope host lo
2: br0    inet 192.168.1.1/24 brd 192.168.1.255 scope global br0
3: br1    inet 192.168.2.1/24 brd 192.168.2.255 scope global br1
"#;
        let segments = parse_ip_addr_segments(sample);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0], LanSegment { iface: "br0".into(), ip: "192.168.1.1".into() });
        assert_eq!(segments[1], LanSegment { iface: "br1".into(), ip: "192.168.2.1".into() });
    }
}
