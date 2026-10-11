use crate::logger::log;
use crate::types::*;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

#[derive(Serialize)]
struct ConfigItem {
    file: String,
    content: String,
}
#[derive(Deserialize)]
pub struct ConfigReq {
    file: String,
    content: String,
}
#[derive(Deserialize)]
pub struct DeleteReq {
    file: String,
}
#[derive(Deserialize)]
pub struct RenameReq {
    file: String,
    new_file: String,
}

async fn collect_configs(paths: &[String], is_mihomo: bool) -> Vec<ConfigItem> {
    let mut results = Vec::new();
    for path_str in paths {
        let path = Path::new(path_str);
        if path.is_dir() {
            let canonical_dir = tokio::fs::canonicalize(path).await.ok();
            match tokio::fs::read_dir(path).await {
                Err(e) => {
                    log("ERROR", format!("Не удалось открыть директорию {}: {}", path_str, e));
                }
                Ok(mut entries) => {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        let entry_path = entry.path();
                        let matches = if is_mihomo {
                            entry_path.extension().is_some_and(|e| e == "yaml" || e == "yml")
                        } else {
                            entry_path.extension().is_some_and(|e| e == "json")
                        };
                        if matches {
                            if let Ok(meta) = tokio::fs::symlink_metadata(&entry_path).await
                                && meta.file_type().is_symlink()
                            {
                                if let Ok(resolved) = tokio::fs::canonicalize(&entry_path).await {
                                    if let Some(ref cdir) = canonical_dir
                                        && !resolved.starts_with(cdir)
                                    {
                                        log("WARN", format!("Пропуск symlink вне разрешенного каталога: {}", entry_path.display()));
                                        continue;
                                    }
                                    if resolved.file_name().and_then(|n| n.to_str()) == Some("xkeen-ui.json") {
                                        log("WARN", format!("Пропуск symlink на xkeen-ui.json: {}", entry_path.display()));
                                        continue;
                                    }
                                } else {
                                    continue;
                                }
                            }
                            match tokio::fs::read_to_string(&entry_path).await {
                                Ok(content) => results.push(ConfigItem {
                                    file: entry_path.to_string_lossy().into(),
                                    content,
                                }),
                                Err(e) => {
                                    log(
                                        "ERROR",
                                        format!("Не удалось прочитать файл {}: {}", entry_path.display(), e),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        } else if path.exists() {
            if let Ok(meta) = tokio::fs::symlink_metadata(path).await
                && meta.file_type().is_symlink()
                && let Ok(resolved) = tokio::fs::canonicalize(path).await
                && resolved.file_name().and_then(|n| n.to_str()) == Some("xkeen-ui.json")
            {
                log("WARN", format!("Пропуск symlink на xkeen-ui.json: {}", path_str));
                continue;
            }
            match tokio::fs::read_to_string(path).await {
                Ok(content) => results.push(ConfigItem {
                    file: path_str.clone(),
                    content,
                }),
                Err(e) => {
                    log("ERROR", format!("Не удалось прочитать файл {}: {}", path_str, e));
                }
            }
        } else {
            log("WARN", format!("Файл не найден: {}", path_str));
        }
    }
    results.sort_by(|a, b| a.file.cmp(&b.file));
    results.dedup_by(|a, b| a.file == b.file);
    results
}

pub async fn get_configs(
    State(state): State<AppState>, Query(parameters): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let target_core = parameters
        .get("core")
        .cloned()
        .unwrap_or_else(|| state.core.read().unwrap().name.clone());
    let is_mihomo = target_core == "mihomo";

    let core_paths = {
        let settings = state.settings.read().unwrap();
        let default_path = if is_mihomo {
            MIHOMO_CONF_DIR.to_string()
        } else {
            XRAY_CONF_DIR.to_string()
        };
        let mut paths = vec![default_path];
        let extra = if is_mihomo {
            settings.append_config_paths.mihomo.clone()
        } else {
            settings.append_config_paths.xray.clone()
        };
        paths.extend(extra);
        paths
    };

    let mut core_configs = collect_configs(&core_paths, is_mihomo).await;
    let mut lst_configs = Vec::new();

    if let Ok(mut entries) = tokio::fs::read_dir(XKEEN_CONF_DIR).await {
        let canonical_xkeen = tokio::fs::canonicalize(XKEEN_CONF_DIR).await.ok();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.extension().is_some_and(|e| e == "lst") || name == "xkeen.json" {
                if let Ok(meta) = tokio::fs::symlink_metadata(&path).await
                    && meta.file_type().is_symlink()
                {
                    if let Ok(resolved) = tokio::fs::canonicalize(&path).await {
                        if let Some(ref cdir) = canonical_xkeen
                            && !resolved.starts_with(cdir)
                        {
                            continue;
                        }
                        if resolved.file_name().and_then(|n| n.to_str()) == Some("xkeen-ui.json") {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                if let Ok(content) = tokio::fs::read_to_string(&path).await {
                    lst_configs.push(ConfigItem {
                        file: path.to_string_lossy().into(),
                        content,
                    });
                }
            }
        }
    }

    lst_configs.sort_by(|a, b| a.file.cmp(&b.file));
    core_configs.append(&mut lst_configs);

    Json(serde_json::json!({ "success": true, "configs": core_configs }))
}

pub(crate) fn get_allowed_prefixes(state: &AppState, is_lst: bool) -> Vec<String> {
    if is_lst {
        return vec![XKEEN_CONF_DIR.to_string()];
    }
    let settings = state.settings.read().unwrap();
    let core = state.core.read().unwrap();
    let default_path = if core.name == "mihomo" {
        MIHOMO_CONF_DIR.to_string()
    } else {
        XRAY_CONF_DIR.to_string()
    };
    let extra = if core.name == "mihomo" {
        settings.append_config_paths.mihomo.clone()
    } else {
        settings.append_config_paths.xray.clone()
    };
    let mut paths = vec![default_path];
    paths.extend(extra);
    paths
}

pub(crate) fn is_path_allowed(file: &str, prefixes: &[String]) -> bool {
    if file.contains("..") {
        return false;
    }
    let file_path = Path::new(file);
    let fname = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if fname == "xkeen-ui.json" {
        return false;
    }

    let lexical_match = prefixes.iter().any(|prefix| {
        let prefix_path = Path::new(prefix.as_str());
        if prefix_path.is_dir() {
            file_path.starts_with(prefix_path)
        } else {
            file == prefix
        }
    });
    if !lexical_match {
        return false;
    }

    let canonical_file = if let Ok(canon) = std::fs::canonicalize(file_path) {
        canon
    } else if let Some(parent) = file_path.parent() {
        if let Ok(canon_parent) = std::fs::canonicalize(parent) {
            if let Some(name) = file_path.file_name() {
                canon_parent.join(name)
            } else {
                return false;
            }
        } else {
            return true;
        }
    } else {
        return false;
    };

    if canonical_file.file_name().and_then(|n| n.to_str()) == Some("xkeen-ui.json") {
        return false;
    }

    prefixes.iter().any(|prefix| {
        let prefix_path = Path::new(prefix.as_str());
        let canon_prefix = std::fs::canonicalize(prefix_path).unwrap_or_else(|_| prefix_path.to_path_buf());
        if prefix_path.is_dir() || canon_prefix.is_dir() {
            canonical_file.starts_with(&canon_prefix)
        } else {
            canonical_file == canon_prefix
        }
    })
}

fn check_access(file: &str, state: &AppState) -> Result<bool, &'static str> {
    if file.contains("..") {
        return Err("Invalid path");
    }
    let file_path = Path::new(file);
    let fname = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if fname == "xkeen-ui.json" {
        return Err("Access to settings file is forbidden");
    }
    let is_xkeen = file.ends_with(".lst") || (fname == "xkeen.json" && file.starts_with(XKEEN_CONF_DIR));
    let prefixes = get_allowed_prefixes(state, is_xkeen);
    if !is_path_allowed(file, &prefixes) {
        return Err("Path not allowed");
    }
    Ok(file.ends_with(".lst"))
}

fn is_mihomo_config_file(path: &str) -> bool {
    path.starts_with(MIHOMO_CONF_DIR) || path == MIHOMO_CONFIG_FILE
}

pub async fn put_config(
    State(state): State<AppState>, Query(params): Query<HashMap<String, String>>,
    Json(req): Json<ConfigReq>,
) -> impl IntoResponse {
    match crate::config_transaction::run(async move { put_config_inner(state, params, req).await }).await {
        Ok(response) => response,
        Err(e) => Json(ApiResponse::<()> {
            success: false,
            error: Some(format!("Configuration transaction failed: {e}")),
            data: None,
        }),
    }
}

async fn put_config_inner(state: AppState, params: HashMap<String, String>, req: ConfigReq) -> Json<ApiResponse<()>> {
    let is_lst = match check_access(&req.file, &state) {
        Ok(val) => val,
        Err(error) => return api_error(error),
    };
    let content = if is_lst {
        req.content.replace("\r\n", "\n")
    } else {
        req.content
    };

    if let Some(core_type) = params.get("validate") {
        if core_type == "mihomo" {
            let close_conns = params
                .get("closeConns")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);
            return apply_mihomo_hot_reload(&state, &req.file, &content, close_conns).await;
        }
        if core_type == "xray" {
            let validate_files = match validation_snapshot(core_type, Path::new(XRAY_CONF_DIR), &req.file, &content) {
                Ok(files) => files,
                Err(e) => {
                    return Json(ApiResponse::<()> {
                        success: false,
                        error: Some(format!("Cannot read validation snapshot: {e}")),
                        data: None,
                    });
                }
            };

            if let Err(err_msg) = validate_core(&validate_files).await {
                log("ERROR", err_msg);
                return api_error("Validation failed");
            }
        }
    }

    if let Err(e) = crate::config_transaction::write_atomic(Path::new(&req.file), content.as_bytes(), false) {
        return api_error(e);
    }
    if is_mihomo_config_file(&req.file) {
        crate::provider_inspector::invalidate_mihomo_yaml_cache();
    }
    api_ok()
}

pub async fn post_config(State(state): State<AppState>, Json(req): Json<ConfigReq>) -> impl IntoResponse {
    match crate::config_transaction::run(async move { post_config_inner(state, req).await }).await {
        Ok(response) => response,
        Err(e) => Json(ApiResponse::<()> {
            success: false,
            error: Some(format!("Configuration transaction failed: {e}")),
            data: None,
        }),
    }
}

async fn post_config_inner(state: AppState, req: ConfigReq) -> Json<ApiResponse<()>> {
    let is_lst = match check_access(&req.file, &state) {
        Ok(val) => val,
        Err(error) => return api_error(error),
    };
    if Path::new(&req.file).exists() {
        return api_error("File already exists");
    }
    let content = if is_lst {
        req.content.replace("\r\n", "\n")
    } else {
        req.content
    };
    if let Err(e) = crate::config_transaction::write_atomic(Path::new(&req.file), content.as_bytes(), true) {
        return api_error(e);
    }
    if is_mihomo_config_file(&req.file) {
        crate::provider_inspector::invalidate_mihomo_yaml_cache();
    }
    api_ok()
}

pub async fn delete_config(State(state): State<AppState>, Json(req): Json<DeleteReq>) -> impl IntoResponse {
    match crate::config_transaction::run(async move { delete_config_inner(state, req).await }).await {
        Ok(response) => response,
        Err(e) => Json(ApiResponse::<()> {
            success: false,
            error: Some(format!("Configuration transaction failed: {e}")),
            data: None,
        }),
    }
}

async fn delete_config_inner(state: AppState, req: DeleteReq) -> Json<ApiResponse<()>> {
    if let Err(e) = check_access(&req.file, &state) {
        return api_error(e);
    }
    if fs::remove_file(&req.file).is_err() {
        return api_error("Delete error");
    }
    if is_mihomo_config_file(&req.file) {
        crate::provider_inspector::invalidate_mihomo_yaml_cache();
    }
    api_ok()
}

pub async fn patch_config(State(state): State<AppState>, Json(req): Json<RenameReq>) -> impl IntoResponse {
    match crate::config_transaction::run(async move { patch_config_inner(state, req).await }).await {
        Ok(response) => response,
        Err(e) => Json(ApiResponse::<()> {
            success: false,
            error: Some(format!("Configuration transaction failed: {e}")),
            data: None,
        }),
    }
}

async fn patch_config_inner(state: AppState, req: RenameReq) -> Json<ApiResponse<()>> {
    if let Err(e) = check_access(&req.file, &state) {
        return api_error(e);
    }
    if let Err(e) = check_access(&req.new_file, &state) {
        return api_error(e);
    }
    if Path::new(&req.new_file).exists() {
        return api_error("File already exists");
    }
    if fs::rename(&req.file, &req.new_file).is_err() {
        return api_error("Rename error");
    }
    if is_mihomo_config_file(&req.file) || is_mihomo_config_file(&req.new_file) {
        crate::provider_inspector::invalidate_mihomo_yaml_cache();
    }
    api_ok()
}

fn validation_snapshot(core: &str, directory: &Path, current: &str, content: &str) -> Result<Vec<ConfigReq>, String> {
    if core == "mihomo" {
        return Ok(vec![ConfigReq {
            file: current.into(),
            content: content.into(),
        }]);
    }
    if core != "xray" {
        return Err("Unknown validation core".into());
    }
    let current_path = Path::new(current);
    let current_resolved = fs::canonicalize(current_path).ok();
    let mut found = false;
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let resolved = fs::canonicalize(&path).map_err(|e| e.to_string())?;
            let same = path == current_path || current_resolved.as_ref() == Some(&resolved);
            let contents = if same {
                found = true;
                content.into()
            } else {
                fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?
            };
            files.push(ConfigReq {
                file: path.to_string_lossy().into(),
                content: contents,
            });
        }
    }
    if !found {
        files.push(ConfigReq {
            file: current.into(),
            content: content.into(),
        });
    }
    Ok(files)
}

async fn validate_core(files: &[ConfigReq]) -> Result<(), String> {
    let temp_dir = std::env::temp_dir().join(format!(
        "xkeen-ui-validation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    tokio::fs::create_dir_all(&temp_dir).await.map_err(|e| e.to_string())?;

    for item in files {
        let Some(name) = Path::new(&item.file).file_name() else {
            continue;
        };
        if let Err(e) = tokio::fs::write(temp_dir.join(name), &item.content).await {
            _ = tokio::fs::remove_dir_all(&temp_dir).await;
            return Err(e.to_string());
        }
    }

    let mut cmd = tokio::process::Command::new("xray");
    cmd.args(["-test", "-confdir"]).arg(&temp_dir);
    cmd.env("XRAY_LOCATION_ASSET", XRAY_ASSET_DIR);
    cmd.kill_on_drop(true);

    let output = match tokio::time::timeout(Duration::from_secs(30), cmd.output()).await {
        Ok(res) => res.map_err(|e| e.to_string())?,
        Err(_) => {
            _ = tokio::fs::remove_dir_all(&temp_dir).await;
            return Err("Configuration validation timed out".to_string());
        }
    };
    _ = tokio::fs::remove_dir_all(&temp_dir).await;

    if output.status.success() {
        return Ok(());
    }

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Err(combined)
}

#[derive(Deserialize)]
struct MihomoError {
    message: String,
}

fn api_ok() -> Json<ApiResponse<()>> {
    Json(ApiResponse { success: true, error: None, data: None })
}

fn api_error(error: impl Into<String>) -> Json<ApiResponse<()>> {
    Json(ApiResponse { success: false, error: Some(error.into()), data: None })
}

struct MihomoApiCreds {
    port: Option<u16>,
    secret: Option<String>,
    unix_path: Option<String>,
}

fn yaml_value_to_string(value: &yaml_rust2::Yaml) -> Option<String> {
    match value {
        yaml_rust2::Yaml::String(s) => Some(s.clone()),
        yaml_rust2::Yaml::Integer(i) => Some(i.to_string()),
        yaml_rust2::Yaml::Real(s) => Some(s.clone()),
        yaml_rust2::Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn parse_mihomo_api_creds(yaml_content: &str) -> MihomoApiCreds {
    let mut creds = MihomoApiCreds { port: None, secret: None, unix_path: None };
    let Ok(docs) = yaml_rust2::YamlLoader::load_from_str(yaml_content) else {
        return creds;
    };
    let Some(doc) = docs.first() else { return creds };
    if let Some(unix_val) = yaml_value_to_string(&doc["external-controller-unix"]) {
        let trimmed = unix_val.trim();
        if !trimmed.is_empty() {
            if let Some(name) = Path::new(trimmed)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
            {
                creds.unix_path = Some(format!("{}/{}", MIHOMO_CONF_DIR, name));
            }
        }
    }
    if let Some(controller) = yaml_value_to_string(&doc["external-controller"]) {
        if let Some(port_str) = controller.trim().rsplit(':').next() {
            let port_str = port_str.trim().trim_matches(|c| c == '"' || c == '\'');
            if let Ok(port) = port_str.parse::<u16>() {
                creds.port = Some(port);
            }
        }
    }
    if let Some(secret) = yaml_value_to_string(&doc["secret"]) {
        let trimmed = secret.trim().to_string();
        if !trimmed.is_empty() {
            creds.secret = Some(trimmed);
        }
    }
    creds
}

async fn apply_mihomo_hot_reload(
    state: &AppState, file: &str, content: &str, close_conns: bool,
) -> Json<ApiResponse<()>> {
    let main_config = format!("{}/config.yaml", MIHOMO_CONF_DIR);
    let creds_source = tokio::fs::read_to_string(&main_config).await.unwrap_or_default();
    let creds = parse_mihomo_api_creds(&creds_source);

    enum Target {
        Unix { client: reqwest::Client },
        Tcp { port: u16, secret: Option<String> },
    }
    let target = match (creds.unix_path, creds.port) {
        (Some(socket_path), _) => match reqwest::Client::builder().unix_socket(socket_path).build() {
            Ok(client) => Target::Unix { client },
            Err(error) => return api_error(error.to_string()),
        },
        (None, Some(port)) => Target::Tcp { port, secret: creds.secret.clone() },
        (None, None) => return api_error("external-controller не найден"),
    };

    let previous = tokio::fs::read_to_string(file).await.ok();
    if let Err(error) = crate::config_transaction::write_atomic(Path::new(file), content.as_bytes(), false) {
        return api_error(format!("Write error: {error}"));
    }

    let (client, configs_url, conns_url, secret) = match &target {
        Target::Unix { client } => (
            client.clone(),
            "http://127.0.0.1/configs?force=true".to_string(),
            "http://127.0.0.1/connections".to_string(),
            None,
        ),
        Target::Tcp { port, secret } => (
            state.http_client.clone(),
            format!("http://127.0.0.1:{port}/configs?force=true"),
            format!("http://127.0.0.1:{port}/connections"),
            secret.clone(),
        ),
    };

    let mut request = client.put(configs_url).json(&serde_json::json!({})).timeout(Duration::from_secs(15));
    if let Some(secret) = secret.as_deref() {
        request = request.bearer_auth(secret);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            let message = format!("Ошибка Mihomo: {error}");
            log("ERROR", message.clone());
            if let Some(old) = previous {
                _ = crate::config_transaction::write_atomic(Path::new(file), old.as_bytes(), false);
            }
            return api_error(message);
        }
    };

    let status = response.status();
    if status == StatusCode::NO_CONTENT {
        if close_conns {
            let mut req = client.delete(conns_url).timeout(Duration::from_secs(15));
            if let Some(secret) = secret.as_deref() {
                req = req.bearer_auth(secret);
            }
            if let Err(error) = req.send().await {
                log("ERROR", format!("Ошибка Mihomo: {error}"));
            }
        }
        if is_mihomo_config_file(file) {
            crate::provider_inspector::invalidate_mihomo_yaml_cache();
        }
        return api_ok();
    }

    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<MihomoError>(&body).map_or(body, |error| error.message);
    let detail = match message.trim() {
        "" => format!("Mihomo вернул {status}"),
        trimmed => trimmed.chars().take(2000).collect(),
    };

    let message = format!("Ошибка Mihomo: {detail}");
    log("ERROR", message.clone());

    if let Some(old) = previous {
        _ = crate::config_transaction::write_atomic(Path::new(file), old.as_bytes(), false);
    } else {
        _ = tokio::fs::remove_file(file).await;
    }

    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return api_error(message);
    }

    api_error("Validation failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_transaction::{run, write_atomic};
    fn fixture() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("snapshot-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn unreadable_snapshot_is_rejected_instead_of_empty_contents() {
        let dir = fixture();
        let p = dir.join("a.json");
        fs::write(&p, "old").unwrap();
        fs::write(dir.join("broken.json"), [0xff]).unwrap();
        assert!(validation_snapshot("xray", &dir, p.to_str().unwrap(), "new").is_err());
        assert_eq!(fs::read_to_string(&p).unwrap(), "old");
        fs::remove_dir_all(&dir).unwrap();
        assert!(validation_snapshot("xray", &dir, "a.json", "new").is_err());
    }
    #[tokio::test]
    async fn competing_validated_snapshots_cannot_commit_invalid_combination() {
        // Each single change is valid against initial contents, but both together are invalid.
        let dir = fixture();
        fs::write(dir.join("a.json"), "0").unwrap();
        fs::write(dir.join("b.json"), "0").unwrap();
        let mut tasks = Vec::new();
        for name in ["a.json", "b.json"] {
            let dir = dir.clone();
            tasks.push(tokio::spawn(async move {
                run(async move {
                    let files = validation_snapshot("xray", &dir, dir.join(name).to_str().unwrap(), "1").unwrap();
                    let sum: i32 = files.iter().map(|f| f.content.parse::<i32>().unwrap()).sum();
                    if sum > 1 {
                        return Err("Invalid combined sum".to_string());
                    }
                    write_atomic(&dir.join(name), b"1", false).map_err(|e| e.to_string())?;
                    Ok(())
                })
                .await
            }));
        }
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await);
        }
        let mut successes = 0;
        for res in results {
            if let Ok(Ok(Ok(()))) = res {
                successes += 1;
            }
        }
        assert_eq!(successes, 1, "Exactly one transaction should pass validation");
        let a = fs::read_to_string(dir.join("a.json")).unwrap().parse::<i32>().unwrap();
        let b = fs::read_to_string(dir.join("b.json")).unwrap().parse::<i32>().unwrap();
        assert_eq!(a + b, 1, "Combined snapshot state should never exceed 1");
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn r37_collect_configs_ignores_symlinks_escaping_target_dir() {
        use std::os::unix::fs::symlink;
        let config_dir = fixture();
        let outside_dir = fixture();

        // Valid internal config
        let valid_config = config_dir.join("01_valid.json");
        fs::write(&valid_config, "{}").unwrap();

        // External target file
        let secret_file = outside_dir.join("secret.json");
        fs::write(&secret_file, "{\"secret\": true}").unwrap();

        // Malicious symlink pointing outside
        let malicious_symlink = config_dir.join("02_escaped.json");
        symlink(&secret_file, &malicious_symlink).unwrap();

        // Symlink pointing to xkeen-ui.json
        let settings_file = outside_dir.join("xkeen-ui.json");
        fs::write(&settings_file, "{}").unwrap();
        let settings_symlink = config_dir.join("03_settings.json");
        symlink(&settings_file, &settings_symlink).unwrap();

        let paths = vec![config_dir.to_str().unwrap().to_string()];
        let collected = collect_configs(&paths, false).await;

        assert_eq!(collected.len(), 1, "Должен собраться только валидный файл внутри каталога");
        assert_eq!(collected[0].file, valid_config.to_str().unwrap());

        fs::remove_dir_all(config_dir).unwrap();
        fs::remove_dir_all(outside_dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn r37_is_path_allowed_rejects_symlink_traversal_and_settings_file() {
        use std::os::unix::fs::symlink;
        let allowed_dir = fixture();
        let outside_dir = fixture();

        let normal_config = allowed_dir.join("config.json");
        fs::write(&normal_config, "{}").unwrap();

        let prefixes = vec![allowed_dir.to_str().unwrap().to_string()];
        assert!(is_path_allowed(normal_config.to_str().unwrap(), &prefixes));

        // Symlink pointing outside
        let outside_file = outside_dir.join("secret.json");
        fs::write(&outside_file, "{}").unwrap();
        let symlink_escape = allowed_dir.join("link_escape.json");
        symlink(&outside_file, &symlink_escape).unwrap();
        assert!(!is_path_allowed(symlink_escape.to_str().unwrap(), &prefixes), "Symlink наружу должен быть отклонен");

        // Target pointing to xkeen-ui.json
        let settings_file = outside_dir.join("xkeen-ui.json");
        fs::write(&settings_file, "{}").unwrap();
        let symlink_settings = allowed_dir.join("link_settings.json");
        symlink(&settings_file, &symlink_settings).unwrap();
        assert!(!is_path_allowed(symlink_settings.to_str().unwrap(), &prefixes), "Symlink на xkeen-ui.json должен быть отклонен");

        // Direct xkeen-ui.json
        assert!(!is_path_allowed(allowed_dir.join("xkeen-ui.json").to_str().unwrap(), &prefixes));

        fs::remove_dir_all(allowed_dir).unwrap();
        fs::remove_dir_all(outside_dir).unwrap();
    }
}
