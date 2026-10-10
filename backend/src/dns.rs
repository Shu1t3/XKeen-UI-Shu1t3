use crate::logger::log;
use crate::types::*;
use axum::extract::State;
use axum::response::{IntoResponse, Json};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(test)]
type DnsProbeOverrideFn = std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>;
#[cfg(test)]
type RciOverrideFn = std::sync::Arc<dyn Fn(&str, &str, &serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync>;
#[cfg(test)]
type ReloadOverrideFn = std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

#[cfg(test)]
tokio::task_local! {
    pub static TEST_DNS_SNAPSHOT_OVERRIDE: PathBuf;
    pub static TEST_MIHOMO_CONFIG_OVERRIDE: PathBuf;
    pub static TEST_DNS_PROBE_OVERRIDE: DnsProbeOverrideFn;
    pub static TEST_RCI_OVERRIDE: RciOverrideFn;
    pub static TEST_SEGMENTS_OVERRIDE: Vec<LanSegment>;
    pub static TEST_RELOAD_OVERRIDE: ReloadOverrideFn;
}

#[derive(Deserialize)]
pub struct DnsEnableReq {
    pub config_content: String,
    #[serde(default = "default_true")]
    pub setup_filter: bool,
    #[serde(default)]
    pub config_file: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct DnsDeleteReq {
    #[serde(default)]
    #[allow(dead_code)]
    pub config_file: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DnsStatusFields {
    pub dns_override: bool,
    pub dns_mihomo: bool,
    pub provider_ignored: bool,
    pub has_snapshot: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DnsNameServerEntry {
    pub address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DnsSnapshot {
    pub dns_override: bool,
    pub name_servers: Vec<DnsNameServerEntry>,
    pub https_upstreams: Vec<String>,
    pub tls_upstreams: Vec<String>,
    pub timestamp: u64,
}

pub fn dns_snapshot_path() -> PathBuf {
    #[cfg(test)]
    if let Ok(p) = TEST_DNS_SNAPSHOT_OVERRIDE.try_with(|p| p.clone()) {
        return p;
    }
    PathBuf::from(crate::types::DNS_SNAPSHOT_FILE)
}

pub fn save_dns_snapshot(snapshot: &DnsSnapshot) -> std::io::Result<()> {
    let path = dns_snapshot_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(snapshot)?;
    let tmp = format!("{}.tmp", path.display());
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load_dns_snapshot() -> Option<DnsSnapshot> {
    let path = dns_snapshot_path();
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn remove_dns_snapshot() {
    let path = dns_snapshot_path();
    let _ = std::fs::remove_file(path);
}

/// R23: Returns the single explicit active Mihomo configuration path.
/// Prioritizes `config.yaml`, then `config.yml`, falling back to `config.yaml`.
/// Never returns arbitrary provider or rule YAML files.
pub fn active_mihomo_config_path() -> PathBuf {
    #[cfg(test)]
    if let Ok(p) = TEST_MIHOMO_CONFIG_OVERRIDE.try_with(|p| p.clone()) {
        return p;
    }
    let default_file = Path::new(crate::types::MIHOMO_CONFIG_FILE);
    if default_file.exists() {
        return default_file.to_path_buf();
    }
    let base = Path::new(MIHOMO_CONF_DIR);
    let yml = base.join("config.yml");
    if yml.exists() {
        return yml;
    }
    default_file.to_path_buf()
}

pub fn resolve_active_mihomo_config(state: &AppState, requested: Option<&str>) -> Result<PathBuf, String> {
    if let Some(file) = requested {
        let allowed = crate::configs::is_path_allowed(file, &crate::configs::get_allowed_prefixes(state, false));
        if !allowed {
            return Err("Указанный файл конфигурации недопустим".into());
        }
        let path = PathBuf::from(file);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            return Err("Файл конфигурации должен иметь расширение .yaml или .yml".into());
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("config") {
            return Err("Файл конфигурации должен быть основным файлом ядра (config.yaml)".into());
        }
        return Ok(path);
    }
    Ok(active_mihomo_config_path())
}

pub fn check_dns_mihomo() -> bool {
    let config_path = active_mihomo_config_path();
    if let Ok(content) = std::fs::read_to_string(&config_path)
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
        return enable && listen.ends_with(":53");
    }
    false
}

fn parse_dns_status(output: &str) -> DnsStatusFields {
    DnsStatusFields {
        dns_override: output.contains("opkg dns-override"),
        dns_mihomo: check_dns_mihomo(),
        provider_ignored: output.contains("ip no name-servers"),
        has_snapshot: load_dns_snapshot().is_some(),
    }
}

#[derive(Serialize)]
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
    #[cfg(test)]
    if let Ok(override_segs) = TEST_SEGMENTS_OVERRIDE.try_with(|s| s.clone()) {
        return Ok(override_segs);
    }

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

pub fn parse_running_config_dns(running_config: &str, segments: &[LanSegment]) -> DnsSnapshot {
    let mut name_servers = Vec::new();
    let mut https_upstreams = Vec::new();
    let mut tls_upstreams = Vec::new();
    let mut dns_override = false;

    let segment_ips: std::collections::HashSet<&str> = segments.iter().map(|s| s.ip.as_str()).collect();

    for line in running_config.lines() {
        let trimmed = line.trim();
        if trimmed == "opkg dns-override" {
            dns_override = true;
        } else if let Some(rest) = trimmed.strip_prefix("ip name-server ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if let Some(&first) = parts.first() {
                let (addr, port_from_addr) = if let Some((ip, port_str)) = first.split_once(':') {
                    (ip, port_str.parse::<u16>().ok())
                } else {
                    (first, None)
                };

                // Skip local segment IPs (which are configured when DNS redirection is active)
                if segment_ips.contains(addr) {
                    continue;
                }

                let mut port = port_from_addr;
                let mut domain = None;

                for &part in parts.iter().skip(1) {
                    let cleaned = part.trim_matches('"').trim_matches('\'');
                    if let Ok(p) = cleaned.parse::<u16>() {
                        port = Some(p);
                    } else if !cleaned.is_empty() {
                        domain = Some(cleaned.to_string());
                    }
                }

                name_servers.push(DnsNameServerEntry {
                    address: addr.to_string(),
                    port,
                    domain,
                });
            }
        } else if let Some(rest) = trimmed.strip_prefix("dns-proxy https upstream ") {
            let upstream = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !upstream.is_empty() && !https_upstreams.contains(&upstream) {
                https_upstreams.push(upstream);
            }
        } else if let Some(rest) = trimmed.strip_prefix("dns-proxy tls upstream ") {
            let upstream = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !upstream.is_empty() && !tls_upstreams.contains(&upstream) {
                tls_upstreams.push(upstream);
            }
        }
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    DnsSnapshot {
        dns_override,
        name_servers,
        https_upstreams,
        tls_upstreams,
        timestamp,
    }
}

pub fn is_currently_redirected(running_config: &str, segments: &[LanSegment]) -> bool {
    if running_config.contains("opkg dns-override") {
        return true;
    }
    for seg in segments {
        if running_config.contains(&format!("ip name-server {}", seg.ip)) {
            return true;
        }
    }
    false
}

pub fn validate_mihomo_dns_config(content: &str) -> Result<(), String> {
    let docs = yaml_rust2::YamlLoader::load_from_str(content)
        .map_err(|e| format!("Некорректный синтаксис YAML: {e}"))?;
    let doc = docs.first().ok_or_else(|| "YAML документ пуст".to_string())?;
    let dns = doc["dns"]
        .as_hash()
        .ok_or_else(|| "В конфигурации отсутствует секция 'dns'".to_string())?;

    let enable = dns
        .get(&yaml_rust2::Yaml::String("enable".into()))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !enable {
        return Err("В секции 'dns' параметр 'enable' должен быть true".into());
    }

    let listen = dns
        .get(&yaml_rust2::Yaml::String("listen".into()))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !listen.ends_with(":53") {
        return Err(format!("В секции 'dns' параметр 'listen' должен слушать порт 53 (текущее: '{listen}')"));
    }

    Ok(())
}

async fn fetch_rci(state: &AppState, endpoint: &str) -> Result<serde_json::Value, String> {
    #[cfg(test)]
    if let Ok(override_fn) = TEST_RCI_OVERRIDE.try_with(|f| f.clone()) {
        return override_fn("GET", endpoint, &json!({}));
    }

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
    #[cfg(test)]
    if let Ok(override_fn) = TEST_RCI_OVERRIDE.try_with(|f| f.clone()) {
        let res = override_fn(method.as_str(), path, &payload)?;
        if ignore_not_found && res.get("not_found").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(false);
        }
        return Ok(true);
    }

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

pub async fn reload_or_restart_mihomo(_state: &AppState, config_path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if let Ok(override_fn) = TEST_RELOAD_OVERRIDE.try_with(|f| f.clone()) {
        return override_fn();
    }

    // 1. Check if Mihomo process is running
    let pids = crate::controller::get_pid("mihomo");
    if pids.is_empty() {
        return crate::controller::soft_restart("mihomo").await;
    }

    // 2. Mihomo is running: attempt config reload via Clash API
    if let Ok(content) = tokio::fs::read_to_string(config_path).await
        && let Ok(docs) = yaml_rust2::YamlLoader::load_from_str(&content)
        && let Some(doc) = docs.first()
    {
        let secret = doc["secret"].as_str().unwrap_or("");
        let unix_socket = doc["external-controller-unix"].as_str().unwrap_or("");
        let external_controller = doc["external-controller"].as_str().unwrap_or("");

        if !unix_socket.is_empty()
            && let Ok(client) = crate::api_relay::relay_http_client(Some(unix_socket))
        {
            let mut req = client
                .request(Method::PUT, "http://localhost/configs")
                .json(&json!({}))
                .timeout(Duration::from_secs(3));
            if !secret.is_empty() {
                req = req.header("Authorization", format!("Bearer {secret}"));
            }
            if let Ok(res) = req.send().await
                && res.status().is_success()
            {
                return Ok(());
            }
        }

        if !external_controller.is_empty() {
            let port = external_controller.split(':').next_back().unwrap_or("").trim();
            if let Ok(port_num) = port.parse::<u16>()
                && let Ok(client) = crate::api_relay::relay_http_client(None)
            {
                let mut req = client
                    .request(Method::PUT, format!("http://127.0.0.1:{port_num}/configs"))
                    .json(&json!({}))
                    .timeout(Duration::from_secs(3));
                if !secret.is_empty() {
                    req = req.header("Authorization", format!("Bearer {secret}"));
                }
                if let Ok(res) = req.send().await
                    && res.status().is_success()
                {
                    return Ok(());
                }
            }
        }
    }

    // Fall back to safe soft_restart
    crate::controller::soft_restart("mihomo").await
}

pub async fn verify_dns_listener(timeout_dur: Duration) -> Result<(), String> {
    #[cfg(test)]
    if let Ok(override_fn) = TEST_DNS_PROBE_OVERRIDE.try_with(|f| f.clone()) {
        return override_fn();
    }

    let start = std::time::Instant::now();
    let query_packet = [
        0x12, 0x34, // Transaction ID
        0x01, 0x00, // Flags: standard query
        0x00, 0x01, // Questions: 1
        0x00, 0x00, // Answer RRs: 0
        0x00, 0x00, // Authority RRs: 0
        0x00, 0x00, // Additional RRs: 0
        0x00,       // Name: root (.)
        0x00, 0x01, // Type: A
        0x00, 0x01, // Class: IN
    ];

    while start.elapsed() < timeout_dur {
        // Try TCP connect to 127.0.0.1:53
        if let Ok(Ok(_stream)) = tokio::time::timeout(
            Duration::from_millis(200),
            tokio::net::TcpStream::connect("127.0.0.1:53"),
        )
        .await
        {
            return Ok(());
        }

        // Try UDP probe to 127.0.0.1:53
        if let Ok(socket) = tokio::net::UdpSocket::bind("127.0.0.1:0").await {
            let _ = socket.send_to(&query_packet, "127.0.0.1:53").await;
            let mut buf = [0u8; 512];
            if let Ok(Ok((n, _))) = tokio::time::timeout(
                Duration::from_millis(200),
                socket.recv_from(&mut buf),
            )
            .await
                && n >= 12
            {
                return Ok(());
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    Err("DNS listener Mihomo на 127.0.0.1:53 не ответил в течение установленного таймаута".into())
}

pub async fn rollback_dns(
    state: &AppState,
    snapshot: &Option<DnsSnapshot>,
    old_config_content: Option<&str>,
    config_path: &Path,
) -> Result<(), String> {
    log("WARN", "Откат DNS: восстановление конфигурации ядра и системных настроек роутера...".to_string());

    if let Some(snap) = snapshot {
        let mut steps: Vec<(&str, Method, &str, serde_json::Value)> = Vec::new();

        if !snap.dns_override {
            steps.push(("Откат opkg dns-override", Method::DELETE, "opkg/dns-override", json!({})));
        } else {
            steps.push(("Восстановление opkg dns-override", Method::POST, "opkg/dns-override", json!({})));
        }

        steps.push(("Очистка name-server", Method::DELETE, "ip/name-server", json!({})));

        for ns in &snap.name_servers {
            let mut payload = json!({"address": ns.address});
            if let Some(p) = ns.port {
                payload["port"] = json!(p);
            }
            if let Some(ref d) = ns.domain {
                payload["domain"] = json!(d);
            }
            steps.push(("Восстановление исходного name-server", Method::POST, "ip/name-server", payload));
        }

        for u in &snap.https_upstreams {
            steps.push(("Восстановление HTTPS DNS-прокси", Method::POST, "dns-proxy/https/upstream", json!({"address": u})));
        }

        for u in &snap.tls_upstreams {
            steps.push(("Восстановление TLS DNS-прокси", Method::POST, "dns-proxy/tls/upstream", json!({"address": u})));
        }

        steps.push(("Сохранение конфигурации", Method::POST, "system/configuration/save", json!({})));

        for (step, method, path, payload) in steps {
            if let Err(e) = run_rci_step(state, step, method, path, payload).await {
                log("ERROR", format!("Откат DNS: ошибка шага '{step}': {e}"));
            }
        }
    }

    if let Some(old_content) = old_config_content {
        let path_clone = config_path.to_path_buf();
        let content_clone = old_content.to_string();
        let _ = crate::config_transaction::run(async move {
            tokio::task::spawn_blocking(move || {
                crate::config_transaction::write_atomic(&path_clone, content_clone.as_bytes(), false)
            })
            .await
            .map_err(|e| e.to_string())?
        })
        .await;
        let _ = reload_or_restart_mihomo(state, config_path).await;
    }

    Ok(())
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
    let _guard = state.app_config_lock.lock().await;

    // 0. Ensure active core is Mihomo
    let core_name = state.core.read().unwrap().name.clone();
    if core_name != "mihomo" {
        return Json(DnsResponse {
            success: false,
            error: Some("Управление DNS поддерживается только для ядра Mihomo. Переключите активное ядро на Mihomo.".into()),
            status: None,
        });
    }

    // 1. Resolve active Mihomo config path (R23)
    let config_path = match resolve_active_mihomo_config(&state, req.config_file.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            return Json(DnsResponse {
                success: false,
                error: Some(e),
                status: None,
            });
        }
    };

    // 2. Preflight validation of config YAML (R09)
    if let Err(e) = validate_mihomo_dns_config(&req.config_content) {
        return Json(DnsResponse {
            success: false,
            error: Some(format!("Ошибка валидации конфигурации DNS: {e}")),
            status: None,
        });
    }

    // 3. Obtain LAN segments
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

    // 4. Fetch router running-config to create snapshot before modifying anything (R09, R24)
    let running_config = match fetch_running_config(&state).await {
        Ok(cfg) => cfg,
        Err(e) => {
            return Json(DnsResponse {
                success: false,
                error: Some(format!("Не удалось получить конфигурацию роутера для создания снимка: {e}")),
                status: None,
            });
        }
    };

    // 5. Create or retain snapshot
    let snapshot = if let Some(existing) = load_dns_snapshot() {
        Some(existing)
    } else {
        let is_redirected = is_currently_redirected(&running_config, &segments);
        let snap = if is_redirected {
            let corporate_fallback = state.settings.read().unwrap().dns.corporate_fallback.clone();
            DnsSnapshot {
                dns_override: false,
                name_servers: corporate_fallback
                    .into_iter()
                    .map(|ip| DnsNameServerEntry {
                        address: ip,
                        port: Some(53),
                        domain: None,
                    })
                    .collect(),
                https_upstreams: Vec::new(),
                tls_upstreams: Vec::new(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            }
        } else {
            parse_running_config_dns(&running_config, &segments)
        };

        if let Err(e) = save_dns_snapshot(&snap) {
            log("WARN", format!("Не удалось сохранить снимок DNS на диск: {e}"));
        }
        Some(snap)
    };

    // 6. Backup existing config.yaml content for rollback
    let old_config_content = tokio::fs::read_to_string(&config_path).await.ok();

    // 7. Write new YAML config atomically (R09, R23)
    let content = req.config_content;
    let config_path_clone = config_path.clone();
    let write_result = crate::config_transaction::run(async move {
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = config_path_clone.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            crate::config_transaction::write_atomic(&config_path_clone, content.as_bytes(), false)
        })
        .await
        .map_err(|e| e.to_string())?
    })
    .await;

    match write_result {
        Ok(Ok(())) => {
            crate::ruleset_inspector::invalidate_mihomo_yaml_cache();
        }
        Ok(Err(e)) => {
            return Json(DnsResponse {
                success: false,
                error: Some(format!("Ошибка записи конфигурации Mihomo: {e}. Системный DNS не изменялся.")),
                status: None,
            });
        }
        Err(e) => {
            return Json(DnsResponse {
                success: false,
                error: Some(format!("Ошибка выполнения задачи записи: {e}. Системный DNS не изменялся.")),
                status: None,
            });
        }
    }

    // 8. Reload Mihomo with new config (R09)
    if let Err(e) = reload_or_restart_mihomo(&state, &config_path).await {
        log("ERROR", format!("Ошибка перезапуска/перезагрузки Mihomo: {e}. Выполняется откат конфигурации..."));
        if let Some(ref old_content) = old_config_content {
            let cp = config_path.clone();
            let oc = old_content.clone();
            let _ = crate::config_transaction::run(async move {
                tokio::task::spawn_blocking(move || {
                    crate::config_transaction::write_atomic(&cp, oc.as_bytes(), false)
                })
                .await
                .map_err(|e| e.to_string())?
            })
            .await;
            crate::ruleset_inspector::invalidate_mihomo_yaml_cache();
            let _ = reload_or_restart_mihomo(&state, &config_path).await;
        }
        return Json(DnsResponse {
            success: false,
            error: Some(format!("Ошибка активации Mihomo: {e}. Конфигурация откатана, системный DNS не изменялся.")),
            status: None,
        });
    }

    // 9. Verify DNS listener on port 53 (R09)
    if let Err(e) = verify_dns_listener(Duration::from_secs(3)).await {
        log("ERROR", format!("Проверка DNS listener не прошла: {e}. Выполняется откат конфигурации..."));
        if let Some(ref old_content) = old_config_content {
            let cp = config_path.clone();
            let oc = old_content.clone();
            let _ = crate::config_transaction::run(async move {
                tokio::task::spawn_blocking(move || {
                    crate::config_transaction::write_atomic(&cp, oc.as_bytes(), false)
                })
                .await
                .map_err(|e| e.to_string())?
            })
            .await;
            let _ = reload_or_restart_mihomo(&state, &config_path).await;
        }
        return Json(DnsResponse {
            success: false,
            error: Some(format!("DNS listener Mihomo не готов на порту 53: {e}. Конфигурация откатана, системный DNS не изменялся.")),
            status: None,
        });
    }

    // 10. Mihomo DNS listener confirmed ready! Now apply system DNS via RCI (R09)
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
            log("ERROR", format!("Ошибка RCI шага '{step}': {e}. Выполняется откат настроек DNS..."));
            let _ = rollback_dns(&state, &snapshot, old_config_content.as_deref(), &config_path).await;
            return Json(DnsResponse {
                success: false,
                error: Some(format!("Ошибка применения настроек роутера ({step}: {e}). Выполнен откат настроек.")),
                status: None,
            });
        }
        if step.contains("dns-override") {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
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
    let _guard = state.app_config_lock.lock().await;

    let snapshot = load_dns_snapshot();
    let corporate_fallback = state.settings.read().unwrap().dns.corporate_fallback.clone();

    let mut steps: Vec<(&str, Method, &str, serde_json::Value)> = vec![
        (
            "Отключение opkg dns-override",
            Method::DELETE,
            "opkg/dns-override",
            json!({}),
        ),
        (
            "Сброс системных DNS-серверов",
            Method::DELETE,
            "ip/name-server",
            json!({}),
        ),
    ];

    let restore_message: String;

    if let Some(ref snap) = snapshot {
        for ns in &snap.name_servers {
            let mut payload = json!({"address": ns.address});
            if let Some(p) = ns.port {
                payload["port"] = json!(p);
            }
            if let Some(ref d) = ns.domain {
                payload["domain"] = json!(d);
            }
            steps.push((
                "Восстановление исходного name-server",
                Method::POST,
                "ip/name-server",
                payload,
            ));
        }

        for u in &snap.https_upstreams {
            steps.push((
                "Восстановление HTTPS DNS-прокси",
                Method::POST,
                "dns-proxy/https/upstream",
                json!({"address": u}),
            ));
        }

        for u in &snap.tls_upstreams {
            steps.push((
                "Восстановление TLS DNS-прокси",
                Method::POST,
                "dns-proxy/tls/upstream",
                json!({"address": u}),
            ));
        }

        let ns_summary: Vec<String> = snap.name_servers.iter().map(|n| n.address.clone()).collect();
        restore_message = if !ns_summary.is_empty() {
            format!("Управление DNS отключено, исходные серверы восстановлены из снимка: {}", ns_summary.join(", "))
        } else {
            "Управление DNS отключено, исходные системные настройки восстановлены из снимка (системный резолвер ISP)".into()
        };
    } else if !corporate_fallback.is_empty() {
        for server in &corporate_fallback {
            steps.push((
                "Установка корпоративного name-server",
                Method::POST,
                "ip/name-server",
                json!({"address": server, "port": 53}),
            ));
        }
        restore_message = format!(
            "Управление DNS отключено, установлены корпоративные DNS-серверы: {}",
            corporate_fallback.join(", ")
        );
    } else {
        restore_message = "Управление DNS отключено, сброс к системным resolver-настройкам по умолчанию (ISP)".into();
    }

    steps.push((
        "Сохранение конфигурации",
        Method::POST,
        "system/configuration/save",
        json!({}),
    ));

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

    remove_dns_snapshot();

    log("INFO", restore_message);
    Json(DnsResponse {
        success: true,
        error: None,
        status: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    type MockRciFn = Arc<dyn Fn(&str, &str, &serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync>;
    type MockVoidFn = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

    fn create_test_state() -> AppState {
        let (log_tx, _) = tokio::sync::broadcast::channel(16);
        let mut app_settings = AppSettings::default();
        app_settings.auth.enabled = false;
        AppState {
            settings: Arc::new(std::sync::RwLock::new(app_settings)),
            core: Arc::new(std::sync::RwLock::new(CoreInfo {
                name: "mihomo".into(),
                conf_dir: MIHOMO_CONF_DIR.into(),
                is_json: false,
            })),
            init_file: Arc::new(std::sync::RwLock::new(None)),
            http_client: reqwest::Client::new(),
            update_checker: crate::types::UpdateChecker::default(),
            geo_cache: Arc::new(std::sync::RwLock::new(Default::default())),
            log_tx: Arc::new(log_tx),
            log_watcher: Arc::new(std::sync::Mutex::new(Default::default())),
            auth_changes: tokio::sync::watch::channel(0).0,
            app_config_lock: Arc::new(tokio::sync::Mutex::new(())),
            enrollment_lock: Arc::new(tokio::sync::Mutex::new(())),
            debug: false,
            rci_token: Arc::new(std::sync::RwLock::new(None)),
        }
    }

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

    #[test]
    fn r23_active_mihomo_config_path_picks_config_yaml_over_other_yamls() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-dns-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Create provider.yaml, rules.yaml, and config.yaml in the directory
        std::fs::write(temp_dir.join("antifilter.yaml"), "payload: []").unwrap();
        std::fs::write(temp_dir.join("provider.yaml"), "payload: []").unwrap();
        std::fs::write(temp_dir.join("config.yaml"), "dns:\n  enable: true\n  listen: 0.0.0.0:53\n").unwrap();
        std::fs::write(temp_dir.join("rules.yaml"), "rules: []").unwrap();

        let state = create_test_state();

        // With test override targeting temp_dir/config.yaml
        let active = temp_dir.join("config.yaml");
        TEST_MIHOMO_CONFIG_OVERRIDE
            .sync_scope(active.clone(), || {
                let resolved = active_mihomo_config_path();
                assert_eq!(resolved, active);
                assert!(check_dns_mihomo());

                // resolve_active_mihomo_config with None picks config.yaml
                let res = resolve_active_mihomo_config(&state, None).unwrap();
                assert_eq!(res, active);

                // Rejecting non-config files
                let bad = resolve_active_mihomo_config(&state, Some(temp_dir.join("provider.yaml").to_str().unwrap()));
                assert!(bad.is_err());
            });

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn r09_validation_rejects_invalid_yaml_and_non_53_listen_without_side_effects() {
        assert!(validate_mihomo_dns_config("not valid yaml: [").is_err());
        assert!(validate_mihomo_dns_config("port: 7890\n").is_err()); // missing dns
        assert!(validate_mihomo_dns_config("dns:\n  enable: false\n  listen: 0.0.0.0:53\n").is_err()); // enable: false
        assert!(validate_mihomo_dns_config("dns:\n  enable: true\n  listen: 0.0.0.0:1053\n").is_err()); // not 53
        assert!(validate_mihomo_dns_config("dns:\n  enable: true\n  listen: 0.0.0.0:53\n").is_ok()); // valid
        assert!(validate_mihomo_dns_config("dns:\n  enable: true\n  listen: ':53'\n").is_ok()); // valid
    }

    #[test]
    fn r24_parse_running_config_dns_captures_original_resolvers_and_filters_segment_ips() {
        let sample = r#"
ip name-server 10.0.0.1
ip name-server 10.0.0.2 53
ip name-server 1.1.1.1 "corp.local" 53
ip name-server 192.168.1.1 53
dns-proxy https upstream https://dns.google/dns-query
dns-proxy tls upstream 1.0.0.1
opkg dns-override
"#;
        let segments = vec![LanSegment {
            iface: "br0".into(),
            ip: "192.168.1.1".into(),
        }];

        let snapshot = parse_running_config_dns(sample, &segments);
        assert!(snapshot.dns_override);
        // 192.168.1.1 must be omitted because it's a segment IP
        assert_eq!(snapshot.name_servers.len(), 3);
        assert_eq!(snapshot.name_servers[0].address, "10.0.0.1");
        assert_eq!(snapshot.name_servers[1].address, "10.0.0.2");
        assert_eq!(snapshot.name_servers[1].port, Some(53));
        assert_eq!(snapshot.name_servers[2].address, "1.1.1.1");
        assert_eq!(snapshot.name_servers[2].domain, Some("corp.local".into()));

        assert_eq!(snapshot.https_upstreams, vec!["https://dns.google/dns-query"]);
        assert_eq!(snapshot.tls_upstreams, vec!["1.0.0.1"]);
    }

    #[tokio::test]
    async fn r09_post_dns_listener_failure_rolls_back_config_and_leaves_rci_untouched() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-dns-fail-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let config_path = temp_dir.join("config.yaml");
        let snapshot_path = temp_dir.join("dns-snapshot.json");

        let initial_yaml = "dns:\n  enable: false\n  listen: 0.0.0.0:53\n";
        tokio::fs::write(&config_path, initial_yaml).await.unwrap();

        let state = create_test_state();
        let segments = vec![LanSegment { iface: "br0".into(), ip: "192.168.1.1".into() }];

        let rci_calls = Arc::new(AtomicUsize::new(0));
        let rci_calls_clone = rci_calls.clone();

        let mock_rci: MockRciFn =
            Arc::new(move |_method, endpoint, _payload| {
                rci_calls_clone.fetch_add(1, Ordering::SeqCst);
                if endpoint == "running-config" {
                    Ok(json!({
                        "message": [
                            "ip name-server 10.0.0.1",
                            "system configuration save"
                        ]
                    }))
                } else {
                    Ok(json!({ "success": true }))
                }
            });

        let mock_probe_fail: MockVoidFn =
            Arc::new(|| Err("Listener probe failed".into()));
        let mock_reload_ok: MockVoidFn = Arc::new(|| Ok(()));

        TEST_DNS_SNAPSHOT_OVERRIDE
            .scope(snapshot_path.clone(), async {
                TEST_MIHOMO_CONFIG_OVERRIDE
                    .scope(config_path.clone(), async {
                        TEST_SEGMENTS_OVERRIDE
                            .scope(segments, async {
                                TEST_RCI_OVERRIDE
                                    .scope(mock_rci, async {
                                        TEST_RELOAD_OVERRIDE
                                            .scope(mock_reload_ok, async {
                                                TEST_DNS_PROBE_OVERRIDE
                                                    .scope(mock_probe_fail, async {
                                                        let req = DnsEnableReq {
                                                            config_content: "dns:\n  enable: true\n  listen: 0.0.0.0:53\n".into(),
                                                            setup_filter: true,
                                                            config_file: None,
                                                        };
                                                        let res = post_dns(State(state), Json(req)).await.into_response();
                                                        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
                                                        let json_res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

                                                        assert_eq!(json_res["success"], false);
                                                        // Config must be rolled back to initial_yaml
                                                        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
                                                        assert_eq!(content, initial_yaml);
                                                        // Only running-config was queried (1 call), NO mutative RCI steps were executed!
                                                        assert_eq!(rci_calls.load(Ordering::SeqCst), 1);
                                                    })
                                                    .await;
                                            })
                                            .await;
                                    })
                                    .await;
                            })
                            .await;
                    })
                    .await;
            })
            .await;

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn r09_rci_failure_triggers_complete_rollback_of_rci_and_config() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-dns-rcifail-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let config_path = temp_dir.join("config.yaml");
        let snapshot_path = temp_dir.join("dns-snapshot.json");

        let initial_yaml = "dns:\n  enable: false\n  listen: 0.0.0.0:53\n";
        tokio::fs::write(&config_path, initial_yaml).await.unwrap();

        let state = create_test_state();
        let segments = vec![LanSegment { iface: "br0".into(), ip: "192.168.1.1".into() }];

        let executed_steps = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executed_steps_clone = executed_steps.clone();

        let mock_rci: MockRciFn =
            Arc::new(move |method, path, payload| {
                let mut guard = executed_steps_clone.lock().unwrap();
                guard.push((method.to_string(), path.to_string()));
                if path == "running-config" {
                    return Ok(json!({
                        "message": [
                            "ip name-server 10.10.10.10"
                        ]
                    }));
                }
                // Simulate failure on system/configuration/save
                if path == "system/configuration/save" && guard.len() <= 6 {
                    return Err("Flash write failed".into());
                }
                Ok(json!({ "success": true, "payload": payload }))
            });

        let mock_probe_ok: MockVoidFn = Arc::new(|| Ok(()));
        let mock_reload_ok: MockVoidFn = Arc::new(|| Ok(()));

        TEST_DNS_SNAPSHOT_OVERRIDE
            .scope(snapshot_path.clone(), async {
                TEST_MIHOMO_CONFIG_OVERRIDE
                    .scope(config_path.clone(), async {
                        TEST_SEGMENTS_OVERRIDE
                            .scope(segments, async {
                                TEST_RCI_OVERRIDE
                                    .scope(mock_rci, async {
                                        TEST_RELOAD_OVERRIDE
                                            .scope(mock_reload_ok, async {
                                                TEST_DNS_PROBE_OVERRIDE
                                                    .scope(mock_probe_ok, async {
                                                        let req = DnsEnableReq {
                                                            config_content: "dns:\n  enable: true\n  listen: 0.0.0.0:53\n".into(),
                                                            setup_filter: true,
                                                            config_file: None,
                                                        };
                                                        let res = post_dns(State(state), Json(req)).await.into_response();
                                                        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
                                                        let json_res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

                                                        assert_eq!(json_res["success"], false);
                                                        // Config rolled back to initial_yaml
                                                        let content = tokio::fs::read_to_string(&config_path).await.unwrap();
                                                        assert_eq!(content, initial_yaml);

                                                        // Check that rollback called restore of 10.10.10.10
                                                        let steps = executed_steps.lock().unwrap();
                                                        assert!(steps.iter().any(|(m, p)| m == "DELETE" && p == "opkg/dns-override"));
                                                        assert!(steps.iter().any(|(m, p)| m == "POST" && p == "ip/name-server"));
                                                    })
                                                    .await;
                                            })
                                            .await;
                                    })
                                    .await;
                            })
                            .await;
                    })
                    .await;
            })
            .await;

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn r24_delete_dns_restores_snapshot_and_never_uses_77_88_8_8() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-dns-del-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let snapshot_path = temp_dir.join("dns-snapshot.json");

        // Seed snapshot with corporate DNS and DoH
        let snapshot = DnsSnapshot {
            dns_override: false,
            name_servers: vec![
                DnsNameServerEntry {
                    address: "10.20.30.40".into(),
                    port: Some(53),
                    domain: Some("corp.internal".into()),
                },
                DnsNameServerEntry {
                    address: "10.20.30.41".into(),
                    port: None,
                    domain: None,
                },
            ],
            https_upstreams: vec!["https://dns.corp.internal/dns-query".into()],
            tls_upstreams: vec!["10.20.30.42".into()],
            timestamp: 12345678,
        };
        std::fs::write(&snapshot_path, serde_json::to_string(&snapshot).unwrap()).unwrap();

        let state = create_test_state();

        let restored_ns = Arc::new(std::sync::Mutex::new(Vec::new()));
        let restored_ns_clone = restored_ns.clone();

        let mock_rci: MockRciFn =
            Arc::new(move |method, path, payload| {
                if method == "POST" && path == "ip/name-server" {
                    restored_ns_clone.lock().unwrap().push(payload.clone());
                }
                Ok(json!({ "success": true }))
            });

        TEST_DNS_SNAPSHOT_OVERRIDE
            .scope(snapshot_path.clone(), async {
                TEST_RCI_OVERRIDE
                    .scope(mock_rci, async {
                        let res = delete_dns(State(state), Json(DnsDeleteReq::default())).await.into_response();
                        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
                        let json_res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(json_res["success"], true);

                        // Snapshot must be deleted
                        assert!(!snapshot_path.exists());

                        // Check restored servers
                        let ns = restored_ns.lock().unwrap();
                        assert_eq!(ns.len(), 2);
                        assert_eq!(ns[0]["address"], "10.20.30.40");
                        assert_eq!(ns[0]["domain"], "corp.internal");
                        assert_eq!(ns[1]["address"], "10.20.30.41");

                        // 77.88.8.8 must NEVER appear!
                        assert!(!ns.iter().any(|v| v["address"] == "77.88.8.8"));
                    })
                    .await;
            })
            .await;

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn r24_delete_dns_without_snapshot_uses_corporate_fallback_when_configured() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-dns-corp-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let snapshot_path = temp_dir.join("nonexistent-dns-snapshot.json");

        let state = create_test_state();
        state.settings.write().unwrap().dns.corporate_fallback = vec!["172.16.0.10".into(), "172.16.0.20".into()];

        let restored_ns = Arc::new(std::sync::Mutex::new(Vec::new()));
        let restored_ns_clone = restored_ns.clone();

        let mock_rci: MockRciFn =
            Arc::new(move |method, path, payload| {
                if method == "POST" && path == "ip/name-server" {
                    restored_ns_clone.lock().unwrap().push(payload.clone());
                }
                Ok(json!({ "success": true }))
            });

        TEST_DNS_SNAPSHOT_OVERRIDE
            .scope(snapshot_path, async {
                TEST_RCI_OVERRIDE
                    .scope(mock_rci, async {
                        let res = delete_dns(State(state), Json(DnsDeleteReq::default())).await.into_response();
                        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
                        let json_res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(json_res["success"], true);

                        let ns = restored_ns.lock().unwrap();
                        assert_eq!(ns.len(), 2);
                        assert_eq!(ns[0]["address"], "172.16.0.10");
                        assert_eq!(ns[1]["address"], "172.16.0.20");
                        assert!(!ns.iter().any(|v| v["address"] == "77.88.8.8"));
                    })
                    .await;
            })
            .await;

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn r24_delete_dns_without_snapshot_and_without_fallback_clears_override_to_isp_default() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-dns-isp-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let snapshot_path = temp_dir.join("nonexistent-dns-snapshot.json");

        let state = create_test_state();
        assert!(state.settings.read().unwrap().dns.corporate_fallback.is_empty());

        let rci_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rci_calls_clone = rci_calls.clone();

        let mock_rci: MockRciFn =
            Arc::new(move |method, path, payload| {
                rci_calls_clone.lock().unwrap().push((method.to_string(), path.to_string(), payload.clone()));
                Ok(json!({ "success": true }))
            });

        TEST_DNS_SNAPSHOT_OVERRIDE
            .scope(snapshot_path, async {
                TEST_RCI_OVERRIDE
                    .scope(mock_rci, async {
                        let res = delete_dns(State(state), Json(DnsDeleteReq::default())).await.into_response();
                        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
                        let json_res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(json_res["success"], true);

                        let calls = rci_calls.lock().unwrap();
                        // DELETE opkg/dns-override, DELETE ip/name-server, POST system/configuration/save
                        assert!(calls.iter().any(|(m, p, _)| m == "DELETE" && p == "opkg/dns-override"));
                        assert!(calls.iter().any(|(m, p, _)| m == "DELETE" && p == "ip/name-server"));
                        assert!(calls.iter().any(|(m, p, _)| m == "POST" && p == "system/configuration/save"));
                        // No POST ip/name-server at all!
                        assert!(!calls.iter().any(|(m, p, _)| m == "POST" && p == "ip/name-server"));
                    })
                    .await;
            })
            .await;

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn r09_post_dns_success_applies_full_pipeline_and_records_snapshot() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-dns-ok-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let config_path = temp_dir.join("config.yaml");
        let snapshot_path = temp_dir.join("dns-snapshot.json");

        let initial_yaml = "dns:\n  enable: false\n  listen: 0.0.0.0:53\n";
        tokio::fs::write(&config_path, initial_yaml).await.unwrap();

        let state = create_test_state();
        let segments = vec![LanSegment { iface: "br0".into(), ip: "192.168.1.1".into() }];

        let executed_rci = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executed_rci_clone = executed_rci.clone();

        let mock_rci: MockRciFn =
            Arc::new(move |method, path, payload| {
                executed_rci_clone.lock().unwrap().push((method.to_string(), path.to_string()));
                if path == "running-config" {
                    return Ok(json!({
                        "message": [
                            "ip name-server 8.8.8.8",
                            "dns-proxy https upstream https://dns.google/dns-query"
                        ]
                    }));
                }
                Ok(json!({ "success": true, "payload": payload }))
            });

        let mock_reload_ok: MockVoidFn = Arc::new(|| Ok(()));
        let mock_probe_ok: MockVoidFn = Arc::new(|| Ok(()));

        let new_yaml = "dns:\n  enable: true\n  listen: 0.0.0.0:53\n";

        TEST_DNS_SNAPSHOT_OVERRIDE
            .scope(snapshot_path.clone(), async {
                TEST_MIHOMO_CONFIG_OVERRIDE
                    .scope(config_path.clone(), async {
                        TEST_SEGMENTS_OVERRIDE
                            .scope(segments, async {
                                TEST_RCI_OVERRIDE
                                    .scope(mock_rci, async {
                                        TEST_RELOAD_OVERRIDE
                                            .scope(mock_reload_ok, async {
                                                TEST_DNS_PROBE_OVERRIDE
                                                    .scope(mock_probe_ok, async {
                                                        let req = DnsEnableReq {
                                                            config_content: new_yaml.into(),
                                                            setup_filter: true,
                                                            config_file: None,
                                                        };
                                                        let res = post_dns(State(state), Json(req)).await.into_response();
                                                        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
                                                        let json_res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

                                                        assert_eq!(json_res["success"], true);

                                                        // New config written
                                                        let written = tokio::fs::read_to_string(&config_path).await.unwrap();
                                                        assert_eq!(written, new_yaml);

                                                        // Snapshot created and preserved on disk
                                                        assert!(snapshot_path.exists());
                                                        let snap_content = tokio::fs::read_to_string(&snapshot_path).await.unwrap();
                                                        let snap: DnsSnapshot = serde_json::from_str(&snap_content).unwrap();
                                                        assert_eq!(snap.name_servers.len(), 1);
                                                        assert_eq!(snap.name_servers[0].address, "8.8.8.8");
                                                        assert_eq!(snap.https_upstreams, vec!["https://dns.google/dns-query"]);

                                                        // RCI steps executed: override, save, deletes, name-server for segment
                                                        let steps = executed_rci.lock().unwrap();
                                                        assert!(steps.iter().any(|(m, p)| m == "POST" && p == "opkg/dns-override"));
                                                        assert!(steps.iter().any(|(m, p)| m == "POST" && p == "ip/name-server"));
                                                        assert!(steps.iter().any(|(m, p)| m == "POST" && p == "system/configuration/save"));
                                                    })
                                                    .await;
                                            })
                                            .await;
                                    })
                                    .await;
                            })
                            .await;
                    })
                    .await;
            })
            .await;

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn r09_post_dns_reload_failure_rolls_back_config() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-dns-reloadfail-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let config_path = temp_dir.join("config.yaml");
        let snapshot_path = temp_dir.join("dns-snapshot.json");

        let initial_yaml = "dns:\n  enable: false\n  listen: 0.0.0.0:53\n";
        tokio::fs::write(&config_path, initial_yaml).await.unwrap();

        let state = create_test_state();
        let segments = vec![LanSegment { iface: "br0".into(), ip: "192.168.1.1".into() }];

        let rci_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rci_calls_clone = rci_calls.clone();

        let mock_rci: MockRciFn =
            Arc::new(move |_method, _endpoint, _payload| {
                rci_calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(json!({ "message": ["ip name-server 8.8.8.8"] }))
            });

        let mock_reload_fail: MockVoidFn =
            Arc::new(|| Err("Mihomo failed to reload config".into()));
        let mock_probe_ok: MockVoidFn = Arc::new(|| Ok(()));

        TEST_DNS_SNAPSHOT_OVERRIDE
            .scope(snapshot_path.clone(), async {
                TEST_MIHOMO_CONFIG_OVERRIDE
                    .scope(config_path.clone(), async {
                        TEST_SEGMENTS_OVERRIDE
                            .scope(segments, async {
                                TEST_RCI_OVERRIDE
                                    .scope(mock_rci, async {
                                        TEST_RELOAD_OVERRIDE
                                            .scope(mock_reload_fail, async {
                                                TEST_DNS_PROBE_OVERRIDE
                                                    .scope(mock_probe_ok, async {
                                                        let req = DnsEnableReq {
                                                            config_content: "dns:\n  enable: true\n  listen: 0.0.0.0:53\n".into(),
                                                            setup_filter: true,
                                                            config_file: None,
                                                        };
                                                        let res = post_dns(State(state), Json(req)).await.into_response();
                                                        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
                                                        let json_res: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

                                                        assert_eq!(json_res["success"], false);
                                                        // Config rolled back to initial_yaml
                                                        let written = tokio::fs::read_to_string(&config_path).await.unwrap();
                                                        assert_eq!(written, initial_yaml);

                                                        // Mutative RCI was never called (only running-config at most)
                                                        assert_eq!(rci_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
                                                    })
                                                    .await;
                                            })
                                            .await;
                                    })
                                    .await;
                            })
                            .await;
                    })
                    .await;
            })
            .await;

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}
