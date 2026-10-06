use crate::logger::log;
use crate::types::*;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if (path.extension().is_some_and(|e| e == "lst") || name == "xkeen.json")
                && let Ok(content) = tokio::fs::read_to_string(&path).await {
                lst_configs.push(ConfigItem {
                    file: path.to_string_lossy().into(),
                    content,
                });
            }
        }
    }

    lst_configs.sort_by(|a, b| a.file.cmp(&b.file));
    core_configs.append(&mut lst_configs);

    Json(serde_json::json!({ "success": true, "configs": core_configs }))
}

fn get_allowed_prefixes(state: &AppState, is_lst: bool) -> Vec<String> {
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

fn is_path_allowed(file: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| {
        let prefix_path = Path::new(prefix.as_str());
        let file_path = Path::new(file);
        if prefix_path.is_dir() {
            file_path.starts_with(prefix_path)
        } else {
            file == prefix
        }
    })
}

fn check_access(file: &str, state: &AppState) -> Result<bool, &'static str> {
    if file.contains("..") {
        return Err("Invalid path");
    }
    let is_xkeen = file.ends_with(".lst") || (file.ends_with(".json") && file.starts_with(XKEEN_CONF_DIR));
    let prefixes = get_allowed_prefixes(state, is_xkeen);
    if !is_path_allowed(file, &prefixes) {
        return Err("Path not allowed");
    }
    Ok(file.ends_with(".lst"))
}

pub async fn put_config(
    State(state): State<AppState>, Query(params): Query<HashMap<String, String>>, Json(req): Json<ConfigReq>,
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
        Err(e) => {
            return Json(ApiResponse::<()> {
                success: false,
                error: Some(e.into()),
                data: None,
            });
        }
    };
    let content = if is_lst {
        req.content.replace("\r\n", "\n")
    } else {
        req.content
    };

    if let Some(core_type) = params.get("validate") {
        let validate_files = match validation_snapshot(core_type, Path::new(XRAY_CONF_DIR), &req.file, &content) {
            Ok(files) => files,
            Err(e) => {
                return Json(ApiResponse::<()> {
                    success: false,
                    error: Some(format!("Cannot read validation snapshot: {e}")),
                    data: None,
                })
            }
        };

        if let Err(err_msg) = validate_core(core_type, &validate_files).await {
            log("ERROR", err_msg);
            return Json(ApiResponse::<()> {
                success: false,
                error: Some("Validation failed".into()),
                data: None,
            });
        }
    }

    if let Err(e) = crate::config_transaction::write_atomic(Path::new(&req.file), content.as_bytes(), false) {
        return Json(ApiResponse::<()> {
            success: false,
            error: Some(e),
            data: None,
        });
    }
    Json(ApiResponse::<()> {
        success: true,
        error: None,
        data: None,
    })
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
        Err(e) => {
            return Json(ApiResponse::<()> {
                success: false,
                error: Some(e.into()),
                data: None,
            });
        }
    };
    if Path::new(&req.file).exists() {
        return Json(ApiResponse::<()> {
            success: false,
            error: Some("File already exists".into()),
            data: None,
        });
    }
    let content = if is_lst {
        req.content.replace("\r\n", "\n")
    } else {
        req.content
    };
    if let Err(e) = crate::config_transaction::write_atomic(Path::new(&req.file), content.as_bytes(), true) {
        return Json(ApiResponse::<()> {
            success: false,
            error: Some(e),
            data: None,
        });
    }
    Json(ApiResponse::<()> {
        success: true,
        error: None,
        data: None,
    })
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
        return Json(ApiResponse::<()> {
            success: false,
            error: Some(e.into()),
            data: None,
        });
    }
    if fs::remove_file(&req.file).is_err() {
        return Json(ApiResponse::<()> {
            success: false,
            error: Some("Delete error".into()),
            data: None,
        });
    }
    Json(ApiResponse::<()> {
        success: true,
        error: None,
        data: None,
    })
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
        return Json(ApiResponse::<()> {
            success: false,
            error: Some(e.into()),
            data: None,
        });
    }
    if let Err(e) = check_access(&req.new_file, &state) {
        return Json(ApiResponse::<()> {
            success: false,
            error: Some(e.into()),
            data: None,
        });
    }
    if Path::new(&req.new_file).exists() {
        return Json(ApiResponse::<()> {
            success: false,
            error: Some("File already exists".into()),
            data: None,
        });
    }
    if fs::rename(&req.file, &req.new_file).is_err() {
        return Json(ApiResponse::<()> {
            success: false,
            error: Some("Rename error".into()),
            data: None,
        });
    }
    Json(ApiResponse::<()> {
        success: true,
        error: None,
        data: None,
    })
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

async fn validate_core(core: &str, files: &[ConfigReq]) -> Result<(), String> {
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

    let mut command = match core {
        "mihomo" => {
            let mut cmd = tokio::process::Command::new("mihomo");
            cmd.args(["-t", "-f"]).arg(temp_dir.join("config.yaml"));
            cmd.env("CLASH_HOME_DIR", MIHOMO_CONF_DIR);
            cmd
        }
        _ => {
            let mut cmd = tokio::process::Command::new("xray");
            cmd.args(["-test", "-confdir"]).arg(&temp_dir);
            cmd.env("XRAY_LOCATION_ASSET", XRAY_ASSET_DIR);
            cmd
        }
    };

    command.kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output()).await;
    _ = tokio::fs::remove_dir_all(&temp_dir).await;

    let output = output
        .map_err(|_| "Configuration validation timed out".to_string())?
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Err(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_transaction::{run, write_atomic};
    use std::sync::Arc;
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
                    let path = dir.join(name);
                    let snapshot = validation_snapshot("xray", &dir, path.to_str().unwrap(), "1").unwrap();
                    tokio::task::yield_now().await;
                    if snapshot.iter().filter(|f| f.content == "1").count() > 1 {
                        return false;
                    }
                    write_atomic(&path, b"1", false).unwrap();
                    true
                })
                .await
                .unwrap()
            }));
        }
        let first = tasks.remove(0).await.unwrap();
        let second = tasks.remove(0).await.unwrap();
        assert_ne!(first, second);
        assert_ne!(
            fs::read(dir.join("a.json")).unwrap(),
            fs::read(dir.join("b.json")).unwrap()
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn create_is_serialized_with_snapshot_validation_and_commit() {
        let dir = fixture();
        let p = dir.join("a.json");
        fs::write(&p, "old").unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let continue_validation = Arc::new(tokio::sync::Notify::new());
        let resume = continue_validation.clone();
        let workdir = dir.clone();
        let first = tokio::spawn(async move {
            run(async move {
                let p = workdir.join("a.json");
                assert_eq!(
                    validation_snapshot("xray", &workdir, p.to_str().unwrap(), "new")
                        .unwrap()
                        .len(),
                    1
                );
                entered_tx.send(()).unwrap();
                resume.notified().await;
                // A create/delete/rename using the same executor cannot mutate the snapshot now.
                assert!(!workdir.join("b.json").exists());
                write_atomic(&p, b"new", false).unwrap();
            })
            .await
            .unwrap()
        });
        entered_rx.await.unwrap();
        let workdir = dir.clone();
        let create = tokio::spawn(async move {
            run(async move {
                assert_eq!(fs::read(workdir.join("a.json")).unwrap(), b"new");
                write_atomic(&workdir.join("b.json"), b"created", true).unwrap();
            })
            .await
            .unwrap()
        });
        tokio::task::yield_now().await;
        continue_validation.notify_one();
        first.await.unwrap();
        create.await.unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn cancelled_request_finishes_transaction_before_next_operation() {
        let dir = fixture();
        let p = dir.join("a.json");
        fs::write(&p, "old").unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let resume = Arc::new(tokio::sync::Notify::new());
        let inner_resume = resume.clone();
        let path = p.clone();
        let request = tokio::spawn(async move {
            run(async move {
                entered_tx.send(()).unwrap();
                inner_resume.notified().await;
                write_atomic(&path, b"committed", false).unwrap();
            })
            .await
        });
        entered_rx.await.unwrap();
        request.abort();
        let _ = request.await;
        resume.notify_one();
        run(async move {
            assert_eq!(fs::read(&p).unwrap(), b"committed");
        })
        .await
        .unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
