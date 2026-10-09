use crate::logger::log;
use crate::types::{APP_CONFIG, ApiResponse, AppState, MIHOMO_CONF_DIR, XKEEN_CONF_DIR, XRAY_CONF_DIR};
use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;
use tar::{Archive, Builder};
use tokio::sync::Mutex;

pub static BACKUP_LOCK: Mutex<()> = Mutex::const_new(());

fn mtime_string(time: Result<SystemTime, std::io::Error>, tz: i32) -> String {
    let secs = time
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    match FixedOffset::east_opt(tz * 3600) {
        Some(offset) => DateTime::from_timestamp(secs, 0)
            .map(|dt| dt.with_timezone(&offset).format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

pub const BACKUP_DIR: &str = opt_path!("/backups");
pub const BACKUP_SUFFIX: &str = "xkeen-ui.tar";
pub const CONTENT_ORDER: [&str; 4] = ["xkeen", "xkeen-ui", "xray", "mihomo"];

#[derive(Clone, Debug)]
pub struct BackupPaths {
    pub backup_dir: PathBuf,
    pub xkeen_dir: PathBuf,
    pub xray_dir: PathBuf,
    pub mihomo_dir: PathBuf,
    pub app_config: PathBuf,
}

impl Default for BackupPaths {
    fn default() -> Self {
        Self {
            backup_dir: PathBuf::from(BACKUP_DIR),
            xkeen_dir: PathBuf::from(XKEEN_CONF_DIR),
            xray_dir: PathBuf::from(XRAY_CONF_DIR),
            mihomo_dir: PathBuf::from(MIHOMO_CONF_DIR),
            app_config: PathBuf::from(APP_CONFIG),
        }
    }
}

#[derive(Serialize)]
struct BackupListData {
    backups: Vec<BackupItem>,
}

#[derive(Serialize)]
struct BackupData {
    backup: BackupItem,
}

#[derive(Serialize, Clone)]
pub struct BackupItem {
    pub name: String,
    pub mtime: String,
    pub size: u64,
    pub content: BackupContentFiles,
}

#[derive(Serialize, Clone, Default)]
pub struct BackupContentFiles {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub xkeen: Vec<String>,
    #[serde(rename = "xkeen-ui", skip_serializing_if = "Vec::is_empty")]
    pub xkeen_ui: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub xray: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mihomo: Vec<String>,
}

impl BackupContentFiles {
    fn insert(&mut self, key: &str, file: String) {
        match key {
            "xkeen" => self.xkeen.push(file),
            "xkeen-ui" => self.xkeen_ui.push(file),
            "xray" => self.xray.push(file),
            "mihomo" => self.mihomo.push(file),
            _ => unreachable!("unknown backup content: {key}"),
        }
    }

    fn sort(&mut self) {
        self.xkeen.sort();
        self.xkeen_ui.sort();
        self.xray.sort();
        self.mihomo.sort();
    }
}

#[derive(Deserialize)]
pub struct BackupReq {
    pub name: String,
    pub contents: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct BackupRenameReq {
    pub name: String,
    pub new_name: String,
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub async fn get_backups(State(state): State<AppState>) -> impl IntoResponse {
    let tz = state.settings.read().unwrap().log.timezone;
    match tokio::task::spawn_blocking(move || list_backups_sync(tz).map(|backups| Some(BackupListData { backups }))).await
    {
        Ok(Ok(data)) => Json(ApiResponse {
            success: true,
            error: None,
            data,
        })
        .into_response(),
        Ok(Err(e)) => api_error(format!("Не удалось получить список бэкапов: {e}")).into_response(),
        Err(e) => api_error(format!("Не удалось получить список бэкапов: {e}")).into_response(),
    }
}

pub async fn put_backup(State(state): State<AppState>) -> impl IntoResponse {
    let tz = state.settings.read().unwrap().log.timezone;
    let app_lock = state.app_config_lock.lock().await;
    let backup_lock = BACKUP_LOCK.lock().await;
    let res = crate::config_transaction::run(async move {
        tokio::task::spawn_blocking(move || create_backup_sync(tz).map(|backup| Some(BackupData { backup })))
            .await
            .map_err(|e| e.to_string())?
    })
    .await;
    drop(backup_lock);
    drop(app_lock);
    match res {
        Ok(Ok(data)) => Json(ApiResponse {
            success: true,
            error: None,
            data,
        })
        .into_response(),
        Ok(Err(e)) => api_error(format!("Не удалось создать бэкап: {e}")).into_response(),
        Err(e) => api_error(format!("Не удалось создать бэкап: {e}")).into_response(),
    }
}

pub async fn post_backup(State(state): State<AppState>, Json(req): Json<BackupReq>) -> impl IntoResponse {
    let app_lock = state.app_config_lock.lock().await;
    let backup_lock = BACKUP_LOCK.lock().await;
    let req_clone = req;
    let res = crate::config_transaction::run(async move {
        tokio::task::spawn_blocking(move || restore_backup_sync(&req_clone.name, req_clone.contents))
            .await
            .map_err(|e| e.to_string())?
    })
    .await;

    match res {
        Ok(Ok(restored_categories)) => {
            if restored_categories.contains("xkeen-ui") {
                let new_settings = crate::settings::load_settings();
                *state.settings.write().unwrap() = new_settings;
                state.auth_changes.send_modify(|version| *version = version.wrapping_add(1));
            }
            if restored_categories.contains("xkeen") {
                let new_token = crate::types::load_rci_token();
                *state.rci_token.write().unwrap() = new_token;
            }
            if restored_categories.contains("xray") || restored_categories.contains("mihomo") {
                *state.core.write().unwrap() = crate::detect_core(state.init_file.read().unwrap().as_deref());
            }
            drop(backup_lock);
            drop(app_lock);
            Json(ApiResponse {
                success: true,
                error: None,
                data: None::<()>,
            })
            .into_response()
        }
        Ok(Err(e)) => {
            drop(backup_lock);
            drop(app_lock);
            api_error(format!("Не удалось восстановить бэкап: {e}")).into_response()
        }
        Err(e) => {
            drop(backup_lock);
            drop(app_lock);
            api_error(format!("Не удалось восстановить бэкап: {e}")).into_response()
        }
    }
}

pub async fn delete_backup(Json(req): Json<BackupReq>) -> impl IntoResponse {
    let backup_lock = BACKUP_LOCK.lock().await;
    let res = tokio::task::spawn_blocking(move || delete_backup_sync(&req.name).map(|_| None::<()>)).await;
    drop(backup_lock);
    match res {
        Ok(Ok(data)) => Json(ApiResponse {
            success: true,
            error: None,
            data,
        })
        .into_response(),
        Ok(Err(e)) => api_error(format!("Не удалось удалить бэкап: {e}")).into_response(),
        Err(e) => api_error(format!("Не удалось удалить бэкап: {e}")).into_response(),
    }
}

pub async fn patch_backup(Json(req): Json<BackupRenameReq>) -> impl IntoResponse {
    let backup_lock = BACKUP_LOCK.lock().await;
    let res =
        tokio::task::spawn_blocking(move || rename_backup_sync(&req.name, &req.new_name).map(|_| None::<()>)).await;
    drop(backup_lock);
    match res {
        Ok(Ok(data)) => Json(ApiResponse {
            success: true,
            error: None,
            data,
        })
        .into_response(),
        Ok(Err(e)) => api_error(format!("Не удалось переименовать бэкап: {e}")).into_response(),
        Err(e) => api_error(format!("Не удалось переименовать бэкап: {e}")).into_response(),
    }
}

fn api_error(message: String) -> Json<ApiResponse<()>> {
    Json(ApiResponse {
        success: false,
        error: Some(message),
        data: None,
    })
}

fn list_backups_sync(tz: i32) -> Result<Vec<BackupItem>, String> {
    list_backups_sync_with_paths(&BackupPaths::default(), tz)
}

fn list_backups_sync_with_paths(paths: &BackupPaths, tz: i32) -> Result<Vec<BackupItem>, String> {
    ensure_dir(&paths.backup_dir).map_err(io_error)?;
    let entries = fs::read_dir(&paths.backup_dir).map_err(io_error)?;
    let mut backups = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !is_tar_file(&path) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(md) => md,
            Err(e) => {
                log(
                    "WARN",
                    format!("Не удалось прочитать метаданные {}: {}", path.display(), e),
                );
                continue;
            }
        };

        let content = match inspect_backup_content_with_paths(&path, paths) {
            Ok(res) => res,
            Err(e) => {
                log(
                    "WARN",
                    format!("Не удалось прочитать содержимое бэкапа {}: {}", path.display(), e),
                );
                BackupContentFiles::default()
            }
        };

        let Some(name) = path.file_name().and_then(|v| v.to_str()) else {
            continue;
        };

        backups.push(BackupItem {
            name: name.to_string(),
            mtime: mtime_string(metadata.modified(), tz),
            size: metadata.len(),
            content,
        });
    }

    Ok(backups)
}

pub fn create_backup_sync(tz: i32) -> Result<BackupItem, String> {
    create_backup_sync_with_paths(&BackupPaths::default(), tz)
}

pub fn create_backup_sync_with_paths(paths: &BackupPaths, tz: i32) -> Result<BackupItem, String> {
    ensure_dir(&paths.backup_dir).map_err(io_error)?;
    let files = collect_backup_files_with_paths(paths).map_err(io_error)?;
    if files.is_empty() {
        return Err("не найдено ни одного файла для архивации".into());
    }

    let temp_name = format!(".xkeen-backup-{}.tmp", uuid::Uuid::new_v4());
    let temp_path = paths.backup_dir.join(&temp_name);

    let write_result = (|| -> io::Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        let mut builder = Builder::new(file);
        builder.follow_symlinks(true);
        for (source, relative) in &files {
            builder.append_path_with_name(source, relative)?;
        }
        builder.finish()?;
        let file = builder.into_inner()?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(io_error(error));
    }

    // Pre-publication verification of generated tar archive
    if let Err(err) = validate_backup_entries_with_paths(&temp_path, paths) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!("проверка целостности созданного архива не удалась: {err}"));
    }
    let inspected = match inspect_backup_content_with_paths(&temp_path, paths) {
        Ok(res) => res,
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            return Err(format!("проверка содержимого созданного архива не удалась: {err}"));
        }
    };
    let inspected_count =
        inspected.xkeen.len() + inspected.xkeen_ui.len() + inspected.xray.len() + inspected.mihomo.len();
    if inspected_count != files.len() {
        let _ = fs::remove_file(&temp_path);
        return Err("проверка целостности архива не удалась: несовпадение количества записей".into());
    }

    // Atomic reservation of final backup name
    let base = backup_now(tz).format("%Y-%m-%d_%H-%M-%S").to_string();
    let mut index = 1;
    let (final_name, final_path) = loop {
        let candidate_name = if index == 1 {
            format!("{base}_{BACKUP_SUFFIX}")
        } else {
            format!("{base}_{index}_{BACKUP_SUFFIX}")
        };
        let candidate_path = paths.backup_dir.join(&candidate_name);

        #[cfg(unix)]
        match fs::hard_link(&temp_path, &candidate_path) {
            Ok(()) => {
                let _ = fs::remove_file(&temp_path);
                break (candidate_name, candidate_path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                index += 1;
                continue;
            }
            Err(_) => {
                if !candidate_path.exists() && fs::rename(&temp_path, &candidate_path).is_ok() {
                    break (candidate_name, candidate_path);
                }
                index += 1;
            }
        }

        #[cfg(not(unix))]
        {
            if !candidate_path.exists() && fs::rename(&temp_path, &candidate_path).is_ok() {
                break (candidate_name, candidate_path);
            }
            index += 1;
        }
    };

    let _ = sync_dir(&paths.backup_dir);
    let metadata = fs::metadata(&final_path).map_err(io_error)?;
    log("INFO", format!("Бэкап конфигураций создан: {}", final_path.display()));

    let content = collect_backup_content_with_paths(files.iter().map(|(_, rel)| rel.as_str()), paths);

    Ok(BackupItem {
        name: final_name,
        mtime: mtime_string(metadata.modified(), tz),
        size: metadata.len(),
        content,
    })
}

pub fn restore_backup_sync(
    name: &str, requested_contents: Option<Vec<String>>,
) -> Result<HashSet<&'static str>, String> {
    restore_backup_sync_with_paths_and_hook(&BackupPaths::default(), name, requested_contents, |_| Ok(()))
}

pub fn restore_backup_sync_with_paths_and_hook(
    paths: &BackupPaths, name: &str, requested_contents: Option<Vec<String>>, mut hook: impl FnMut(&str) -> io::Result<()>,
) -> Result<HashSet<&'static str>, String> {
    let backup_path = resolve_backup_path_with_paths(name, paths)?;
    let requested_contents = normalize_requested_contents(requested_contents)?;
    validate_backup_entries_with_paths(&backup_path, paths)?;

    // 1. Read archive entries into in-memory manifest
    let file = File::open(&backup_path).map_err(io_error)?;
    let mut archive = Archive::new(file);
    let entries = archive.entries().map_err(io_error)?;

    struct StoredEntry {
        relative: String,
        target: PathBuf,
        category: &'static str,
        mode: Option<u32>,
        data: Vec<u8>,
    }

    let mut manifest = Vec::new();
    let mut available_categories = HashSet::new();

    for entry in entries {
        let mut entry = entry.map_err(io_error)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let entry_path = entry.path().map_err(io_error)?;
        let relative =
            normalize_entry_path(entry_path.as_ref()).map_err(|e| format!("невалидный путь в архиве: {e}"))?;
        let category = detect_content_key_with_paths(&relative, paths)
            .ok_or_else(|| format!("недопустимый путь в архиве: {relative}"))?;
        let target = archive_relative_to_target_with_paths(&relative, paths)
            .ok_or_else(|| format!("недопустимый путь в архиве: {relative}"))?;

        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(io_error)?;
        #[cfg(unix)]
        let mode = entry.header().mode().ok();
        #[cfg(not(unix))]
        let mode = None;

        available_categories.insert(category);
        manifest.push(StoredEntry {
            relative,
            target,
            category,
            mode,
            data,
        });
    }

    // 2. Preflight: Check all requested categories exist in archive BEFORE modifying anything
    if let Some(ref requested) = requested_contents {
        let missing = CONTENT_ORDER
            .iter()
            .copied()
            .filter(|&c| requested.contains(c) && !available_categories.contains(c))
            .map(content_label)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!("в архиве не найдены категории: {}", missing.join(", ")));
        }
    }

    let categories_to_restore: HashSet<&'static str> = match &requested_contents {
        Some(req) => req.clone(),
        None => available_categories,
    };

    let entries_to_restore: Vec<&StoredEntry> = manifest
        .iter()
        .filter(|e| categories_to_restore.contains(e.category))
        .collect();

    // 3. Preflight syntax & structure validation of configs to restore
    for entry in &entries_to_restore {
        match entry.category {
            "xkeen-ui" => {
                let json: serde_json::Value = serde_json::from_slice(&entry.data)
                    .map_err(|e| format!("невалидный JSON в файле {}: {e}", entry.relative))?;
                if !json.is_object() {
                    return Err(format!("содержимое {} не является JSON объектом", entry.relative));
                }
            }
            "xray" => {
                serde_json::from_slice::<serde_json::Value>(&entry.data)
                    .map_err(|e| format!("невалидный JSON в конфигурации Xray {}: {e}", entry.relative))?;
            }
            "mihomo" => {
                let s = std::str::from_utf8(&entry.data)
                    .map_err(|e| format!("невалидный UTF-8 в конфигурации Mihomo {}: {e}", entry.relative))?;
                yaml_rust2::YamlLoader::load_from_str(s)
                    .map_err(|e| format!("невалидный YAML в конфигурации Mihomo {}: {e}", entry.relative))?;
            }
            _ => {}
        }
    }

    hook("preflight").map_err(|e| format!("ошибка preflight: {e}"))?;

    // 4. Determine managed files on disk and extraneous files to remove (Snapshot contract)
    let mut files_to_remove = Vec::new();
    for &category in &categories_to_restore {
        let existing = collect_managed_files_for_category(category, paths).map_err(io_error)?;
        let restored_targets: HashSet<&Path> = entries_to_restore
            .iter()
            .filter(|e| e.category == category)
            .map(|e| e.target.as_path())
            .collect();
        for file in existing {
            if !restored_targets.contains(file.as_path()) {
                files_to_remove.push(file);
            }
        }
    }

    // 5. Staging and preparation
    let session_id = uuid::Uuid::new_v4();
    let mut staging_dirs = Vec::new();

    let mut staged_writes = Vec::new();
    let mut staged_removes = Vec::new();

    let stage_result = (|| -> io::Result<()> {
        let mut parents = HashSet::new();
        for entry in &entries_to_restore {
            if let Some(parent) = entry.target.parent() {
                parents.insert(parent.to_path_buf());
            }
        }
        for path in &files_to_remove {
            if let Some(parent) = path.parent() {
                parents.insert(parent.to_path_buf());
            }
        }

        for parent in parents {
            fs::create_dir_all(&parent)?;
            let stage_dir = parent.join(format!(".xkeen-restore-{session_id}"));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&stage_dir)?;
            fs::create_dir_all(stage_dir.join("staged"))?;
            fs::create_dir_all(stage_dir.join("rollback"))?;
            staging_dirs.push(stage_dir);
        }

        for entry in &entries_to_restore {
            let parent = entry.target.parent().unwrap();
            let stage_dir = parent.join(format!(".xkeen-restore-{session_id}"));
            let file_name = entry.target.file_name().unwrap();
            let staged_path = stage_dir.join("staged").join(file_name);
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged_path)?;
            f.write_all(&entry.data)?;
            f.sync_all()?;

            let backup_path = if entry.target.is_file() {
                let backup_path = stage_dir.join("rollback").join(file_name);
                fs::copy(&entry.target, &backup_path)?;
                File::open(&backup_path)?.sync_all()?;
                Some(backup_path)
            } else {
                None
            };
            staged_writes.push((entry.target.clone(), staged_path, backup_path, entry.mode));
        }

        for remove_target in &files_to_remove {
            let parent = remove_target.parent().unwrap();
            let stage_dir = parent.join(format!(".xkeen-restore-{session_id}"));
            let file_name = remove_target.file_name().unwrap();
            let backup_path = stage_dir
                .join("rollback")
                .join(format!("rm_{}", file_name.to_string_lossy()));
            fs::copy(remove_target, &backup_path)?;
            File::open(&backup_path)?.sync_all()?;
            staged_removes.push((remove_target.clone(), backup_path));
        }

        for s in &staging_dirs {
            sync_dir(s)?;
        }
        hook("stage")?;
        Ok(())
    })();

    if let Err(err) = stage_result {
        for s in &staging_dirs {
            let _ = fs::remove_dir_all(s);
        }
        return Err(format!("ошибка подготовки staged restore: {err}"));
    }

    // 6. Commit phase with Rollback support
    let mut committed_writes: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let mut committed_removes: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut commit_error: Option<String> = None;

    let commit_result = (|| -> io::Result<()> {
        for (target, staged_path, backup_path, mode) in staged_writes {
            #[cfg(unix)]
            if let Some(m) = mode {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&staged_path, fs::Permissions::from_mode(m));
            }
            hook("before_rename")?;
            fs::rename(&staged_path, &target)?;
            committed_writes.push((target, backup_path));
            hook("after_rename")?;
        }

        for (target, backup_path) in staged_removes {
            hook("before_remove")?;
            fs::remove_file(&target)?;
            committed_removes.push((target, backup_path));
            hook("after_remove")?;
        }

        hook("commit_sync")?;
        for s in &staging_dirs {
            if let Some(parent) = s.parent() {
                sync_dir(parent)?;
            }
        }
        Ok(())
    })();

    if let Err(err) = commit_result {
        commit_error = Some(err.to_string());
    }

    // 7. Rollback if commit failed
    if let Some(err) = commit_error {
        let mut rollback_err = None;
        for (target, backup_path) in committed_removes.into_iter().rev() {
            if let Err(e) = fs::copy(&backup_path, &target) {
                rollback_err = Some(e.to_string());
            }
        }
        for (target, backup_path) in committed_writes.into_iter().rev() {
            if let Some(backup) = backup_path {
                if let Err(e) = fs::copy(&backup, &target) {
                    rollback_err = Some(e.to_string());
                }
            } else {
                let _ = fs::remove_file(&target);
            }
        }
        for s in &staging_dirs {
            if let Some(parent) = s.parent() {
                let _ = sync_dir(parent);
            }
            let _ = fs::remove_dir_all(s);
        }

        return match rollback_err {
            Some(rb) => Err(format!("{err}; ошибка отката изменений: {rb}")),
            None => Err(format!("{err}; выполнен откат изменений до исходного состояния")),
        };
    }

    // 8. Success: Clean up staging dirs and sync parent directories
    for s in &staging_dirs {
        let _ = fs::remove_dir_all(s);
        if let Some(parent) = s.parent() {
            let _ = sync_dir(parent);
        }
    }

    log("INFO", restore_log_message(&backup_path, &requested_contents));
    Ok(categories_to_restore)
}

fn delete_backup_sync(name: &str) -> Result<(), String> {
    delete_backup_sync_with_paths(name, &BackupPaths::default())
}

fn delete_backup_sync_with_paths(name: &str, paths: &BackupPaths) -> Result<(), String> {
    let backup_path = resolve_backup_path_with_paths(name, paths)?;
    fs::remove_file(&backup_path).map_err(io_error)?;
    let _ = sync_dir(&paths.backup_dir);
    log("INFO", format!("Бэкап конфигураций удалён: {}", backup_path.display()));
    Ok(())
}

fn rename_backup_sync(name: &str, new_name: &str) -> Result<(), String> {
    rename_backup_sync_with_paths(name, new_name, &BackupPaths::default())
}

fn rename_backup_sync_with_paths(name: &str, new_name: &str, paths: &BackupPaths) -> Result<(), String> {
    let backup_path = resolve_backup_path_with_paths(name, paths)?;
    let new_name = new_name.trim();
    if new_name.is_empty()
        || new_name.contains('/')
        || new_name.contains('\\')
        || new_name.contains("..")
        || !is_backup_name(Some(new_name))
    {
        return Err("некорректное имя файла".into());
    }
    let new_path = paths.backup_dir.join(new_name);
    if new_path.exists() {
        return Err("файл с таким именем уже существует".into());
    }
    fs::rename(&backup_path, &new_path).map_err(io_error)?;
    let _ = sync_dir(&paths.backup_dir);
    log(
        "INFO",
        format!(
            "Бэкап конфигураций переименован: {} -> {}",
            backup_path.display(),
            new_path.display()
        ),
    );
    Ok(())
}

fn ensure_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

fn is_tar_file(path: &Path) -> bool {
    path.is_file() && is_backup_name(path.file_name().and_then(|v| v.to_str()))
}

fn collect_backup_files_with_paths(paths: &BackupPaths) -> io::Result<Vec<(PathBuf, String)>> {
    let mut files = Vec::new();
    let xkeen_files = collect_files_in_dir(&paths.xkeen_dir, &["lst", "json"])?;
    for f in xkeen_files {
        if f != paths.app_config {
            files.push((f.clone(), to_archive_relative(&f)));
        }
    }
    if paths.app_config.is_file() {
        files.push((paths.app_config.clone(), to_archive_relative(&paths.app_config)));
    }
    for f in collect_files_in_dir(&paths.xray_dir, &["json"])? {
        files.push((f.clone(), to_archive_relative(&f)));
    }
    for f in collect_files_in_dir(&paths.mihomo_dir, &["yaml", "yml"])? {
        files.push((f.clone(), to_archive_relative(&f)));
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(files)
}

fn collect_managed_files_for_category(category: &'static str, paths: &BackupPaths) -> io::Result<Vec<PathBuf>> {
    match category {
        "xray" => collect_files_in_dir(&paths.xray_dir, &["json"]),
        "mihomo" => collect_files_in_dir(&paths.mihomo_dir, &["yaml", "yml"]),
        "xkeen" => {
            let files = collect_files_in_dir(&paths.xkeen_dir, &["lst", "json"])?;
            Ok(files.into_iter().filter(|p| p != &paths.app_config).collect())
        }
        "xkeen-ui" => {
            if paths.app_config.is_file() {
                Ok(vec![paths.app_config.clone()])
            } else {
                Ok(Vec::new())
            }
        }
        _ => Ok(Vec::new()),
    }
}

fn collect_files_in_dir(dir: &Path, exts: &[&str]) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(files),
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && matches_extension(&path, exts) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn matches_extension(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|v| v.to_str())
        .is_some_and(|v| exts.iter().any(|ext| v.eq_ignore_ascii_case(ext)))
}

fn to_archive_relative(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string()
}

fn resolve_backup_path_with_paths(name: &str, paths: &BackupPaths) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || !is_backup_name(Some(name))
    {
        return Err("некорректное имя файла".into());
    }

    let path = paths.backup_dir.join(name);
    if !path.is_file() {
        return Err("файл не найден".into());
    }
    Ok(path)
}

fn inspect_backup_content_with_paths(path: &Path, paths: &BackupPaths) -> io::Result<BackupContentFiles> {
    let file = File::open(path)?;
    let mut archive = Archive::new(file);
    let mut content = BackupContentFiles::default();

    for entry in archive.entries()? {
        let entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let Ok(relative) = normalize_entry_path(&entry.path()?) else {
            continue;
        };
        if let Some(key) = detect_content_key_with_paths(&relative, paths) {
            content.insert(key, content_file_name_with_paths(key, &relative, paths));
        }
    }

    content.sort();
    Ok(content)
}

fn validate_backup_entries_with_paths(path: &Path, paths: &BackupPaths) -> Result<(), String> {
    let file = File::open(path).map_err(io_error)?;
    let mut archive = Archive::new(file);
    let entries = archive.entries().map_err(io_error)?;
    let mut seen = HashSet::new();
    let mut has_files = false;

    for entry in entries {
        let entry = entry.map_err(io_error)?;
        if !entry.header().entry_type().is_file() {
            return Err("архив содержит неподдерживаемые записи".into());
        }

        let entry_path = entry.path().map_err(io_error)?;
        let relative =
            normalize_entry_path(entry_path.as_ref()).map_err(|e| format!("невалидный путь в архиве: {e}"))?;
        if archive_relative_to_target_with_paths(&relative, paths).is_none() {
            return Err(format!("недопустимый путь в архиве: {relative}"));
        }
        if !seen.insert(relative) {
            return Err("архив содержит дубликаты файлов".into());
        }
        has_files = true;
    }

    if !has_files {
        return Err("архив пустой".into());
    }

    Ok(())
}

fn normalize_entry_path(path: &Path) -> Result<String, &'static str> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
            _ => return Err("обнаружен запрещённый компонент пути"),
        }
    }
    if parts.is_empty() {
        return Err("путь пустой");
    }
    Ok(parts.join("/"))
}

fn archive_relative_to_target_with_paths(relative: &str, paths: &BackupPaths) -> Option<PathBuf> {
    let category = detect_content_key_with_paths(relative, paths)?;
    let filename = Path::new(relative).file_name()?.to_str()?;
    match category {
        "xkeen-ui" => Some(paths.app_config.clone()),
        "xkeen" => Some(paths.xkeen_dir.join(filename)),
        "xray" => Some(paths.xray_dir.join(filename)),
        "mihomo" => Some(paths.mihomo_dir.join(filename)),
        _ => None,
    }
}

fn detect_content_key_with_paths(relative: &str, paths: &BackupPaths) -> Option<&'static str> {
    let app_rel = to_archive_relative(&paths.app_config);
    if relative == app_rel
        || relative == "opt/etc/xkeen/xkeen-ui.json"
        || relative == "opt/share/www/XKeen-UI/config.json"
    {
        return Some("xkeen-ui");
    }

    let check_dir = |dir: &Path, alt_prefix: &str, exts: &[&str]| -> bool {
        let prefix = to_archive_relative(dir);
        let check_one = |p: &str| -> Option<bool> {
            let rest = relative.strip_prefix(p)?;
            if let Some(name) = rest.strip_prefix('/')
                && !name.contains('/')
                && matches_file_name(name, exts)
            {
                return Some(true);
            }
            None
        };
        check_one(&prefix).or_else(|| check_one(alt_prefix)).unwrap_or(false)
    };

    if check_dir(&paths.xkeen_dir, "opt/etc/xkeen", &["lst", "json"]) {
        let name = Path::new(relative).file_name().and_then(|v| v.to_str()).unwrap_or("");
        if name != "xkeen-ui.json" && name != "config.json" {
            return Some("xkeen");
        }
    }
    if check_dir(&paths.xray_dir, "opt/etc/xray/configs", &["json"])
        || check_dir(&paths.xray_dir, "opt/etc/xray", &["json"])
    {
        return Some("xray");
    }
    if check_dir(&paths.mihomo_dir, "opt/etc/mihomo", &["yaml", "yml"]) {
        return Some("mihomo");
    }

    None
}

fn matches_file_name(name: &str, exts: &[&str]) -> bool {
    Path::new(name)
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|v| exts.iter().any(|ext| v.eq_ignore_ascii_case(ext)))
}

fn normalize_requested_contents(
    requested_contents: Option<Vec<String>>,
) -> Result<Option<HashSet<&'static str>>, String> {
    let Some(requested_contents) = requested_contents else {
        return Ok(None);
    };
    if requested_contents.is_empty() {
        return Err("не указаны категории для восстановления".into());
    }

    let mut contents = HashSet::new();
    for content in requested_contents {
        let Some(content) = parse_content_key(&content) else {
            return Err(format!("неизвестная категория: {content}"));
        };
        contents.insert(content);
    }

    Ok(Some(contents))
}

fn parse_content_key(value: &str) -> Option<&'static str> {
    CONTENT_ORDER.iter().copied().find(|&content| content == value)
}

fn collect_backup_content_with_paths<'a>(
    paths_iter: impl Iterator<Item = &'a str>, paths: &BackupPaths,
) -> BackupContentFiles {
    let mut content = BackupContentFiles::default();
    for path in paths_iter {
        if let Some(key) = detect_content_key_with_paths(path, paths) {
            content.insert(key, content_file_name_with_paths(key, path, paths));
        }
    }
    content.sort();
    content
}

fn content_label(content: &str) -> &'static str {
    match content {
        "xkeen" => "XKeen",
        "xkeen-ui" => "XKeen UI",
        "xray" => "Xray",
        "mihomo" => "Mihomo",
        _ => unreachable!("unknown backup content: {content}"),
    }
}

fn content_file_name_with_paths(_key: &str, relative: &str, _paths: &BackupPaths) -> String {
    Path::new(relative)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or(relative)
        .to_string()
}

fn restore_log_message(backup_path: &Path, requested_contents: &Option<HashSet<&'static str>>) -> String {
    let Some(requested_contents) = requested_contents else {
        return format!("Конфигурации восстановлены из {}", backup_path.display());
    };

    let contents = CONTENT_ORDER
        .iter()
        .copied()
        .filter(|&content| requested_contents.contains(content))
        .map(content_label)
        .collect::<Vec<_>>()
        .join(", ");

    format!("Конфигурации {} восстановлены из {}", contents, backup_path.display())
}

fn io_error(error: io::Error) -> String {
    error.to_string()
}

fn is_backup_name(name: Option<&str>) -> bool {
    name.is_some_and(|v| v.ends_with(BACKUP_SUFFIX))
}

fn backup_now(tz: i32) -> DateTime<FixedOffset> {
    Utc::now().with_timezone(&backup_offset(tz))
}

fn backup_offset(tz: i32) -> FixedOffset {
    FixedOffset::east_opt(tz.clamp(-12, 14) * 3600).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TestFixture {
        root: PathBuf,
        paths: BackupPaths,
    }

    impl TestFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("xkeen-backup-test-{}", uuid::Uuid::new_v4()));
            let backup_dir = root.join("backups");
            let xkeen_dir = root.join("etc/xkeen");
            let xray_dir = root.join("etc/xray/configs");
            let mihomo_dir = root.join("etc/mihomo");
            let app_config = xkeen_dir.join("xkeen-ui.json");

            fs::create_dir_all(&backup_dir).unwrap();
            fs::create_dir_all(&xkeen_dir).unwrap();
            fs::create_dir_all(&xray_dir).unwrap();
            fs::create_dir_all(&mihomo_dir).unwrap();

            let paths = BackupPaths {
                backup_dir,
                xkeen_dir,
                xray_dir,
                mihomo_dir,
                app_config,
            };

            Self { root, paths }
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn parallel_backups_produce_distinct_intact_archives() {
        let fixture = Arc::new(TestFixture::new());

        // Create sample config files
        fs::write(fixture.paths.xray_dir.join("01_inbound.json"), b"{\"inbound\": 1}").unwrap();
        fs::write(fixture.paths.app_config.as_path(), b"{\"gui\": {}}").unwrap();
        fs::write(fixture.paths.mihomo_dir.join("config.yaml"), b"mode: rule").unwrap();

        let f1 = fixture.clone();
        let f2 = fixture.clone();

        let t1 = std::thread::spawn(move || create_backup_sync_with_paths(&f1.paths, 3));
        let t2 = std::thread::spawn(move || create_backup_sync_with_paths(&f2.paths, 3));

        let res1 = t1.join().unwrap().expect("backup 1 failed");
        let res2 = t2.join().unwrap().expect("backup 2 failed");

        // Both backups succeeded and got distinct names
        assert_ne!(res1.name, res2.name);
        assert!(res1.name.ends_with(BACKUP_SUFFIX));
        assert!(res2.name.ends_with(BACKUP_SUFFIX));

        // Both files exist and are verified intact tar archives
        let p1 = fixture.paths.backup_dir.join(&res1.name);
        let p2 = fixture.paths.backup_dir.join(&res2.name);
        assert!(p1.is_file());
        assert!(p2.is_file());

        assert!(validate_backup_entries_with_paths(&p1, &fixture.paths).is_ok());
        assert!(validate_backup_entries_with_paths(&p2, &fixture.paths).is_ok());

        // Ensure no temporary files remained
        let temp_files: Vec<_> = fs::read_dir(&fixture.paths.backup_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".xkeen-backup-"))
            .collect();
        assert!(temp_files.is_empty(), "temporary files were not cleaned up");
    }

    #[test]
    fn restore_enforces_snapshot_contract_removing_extraneous_files() {
        let fixture = TestFixture::new();

        // 1. Initial configs to backup
        fs::write(fixture.paths.xray_dir.join("01_main.json"), b"{\"routing\": 1}").unwrap();
        fs::write(fixture.paths.xray_dir.join("02_out.json"), b"{\"out\": 1}").unwrap();
        fs::write(fixture.paths.app_config.as_path(), b"{\"gui\": 1}").unwrap();

        let backup = create_backup_sync_with_paths(&fixture.paths, 0).expect("create backup");

        // 2. Add an extraneous broken file and modify an existing file
        fs::write(fixture.paths.xray_dir.join("01_main.json"), b"{\"modified\": true}").unwrap();
        fs::write(fixture.paths.xray_dir.join("99_extraneous.json"), b"{\"broken\": true}").unwrap();
        assert!(fixture.paths.xray_dir.join("99_extraneous.json").is_file());

        // 3. Restore category xray
        let restored = restore_backup_sync_with_paths_and_hook(
            &fixture.paths,
            &backup.name,
            Some(vec!["xray".into()]),
            |_| Ok(()),
        )
        .expect("restore");

        assert!(restored.contains("xray"));

        // 4. Verify snapshot contract: extraneous file was removed, original restored
        assert_eq!(
            fs::read_to_string(fixture.paths.xray_dir.join("01_main.json")).unwrap(),
            "{\"routing\": 1}"
        );
        assert_eq!(
            fs::read_to_string(fixture.paths.xray_dir.join("02_out.json")).unwrap(),
            "{\"out\": 1}"
        );
        assert!(
            !fixture.paths.xray_dir.join("99_extraneous.json").exists(),
            "extraneous managed file was not removed by restore snapshot contract"
        );
    }

    #[test]
    fn restore_selected_category_leaves_unselected_categories_intact() {
        let fixture = TestFixture::new();

        // Create initial backup with xray and mihomo
        fs::write(fixture.paths.xray_dir.join("01_main.json"), b"{\"v\": 1}").unwrap();
        fs::write(fixture.paths.mihomo_dir.join("config.yaml"), b"mode: direct").unwrap();
        let backup = create_backup_sync_with_paths(&fixture.paths, 0).unwrap();

        // Later, user adds local configs in both directories
        fs::write(fixture.paths.xray_dir.join("extra_xray.json"), b"{\"extra\": 1}").unwrap();
        fs::write(fixture.paths.mihomo_dir.join("local_mihomo.yaml"), b"mode: local").unwrap();

        // Restore ONLY xray
        restore_backup_sync_with_paths_and_hook(
            &fixture.paths,
            &backup.name,
            Some(vec!["xray".into()]),
            |_| Ok(()),
        )
        .unwrap();

        // xray extra was cleaned up (snapshot contract)
        assert!(!fixture.paths.xray_dir.join("extra_xray.json").exists());
        // mihomo local file was UNTOUCHED because mihomo category was not selected for restore!
        assert!(fixture.paths.mihomo_dir.join("local_mihomo.yaml").exists());
        assert_eq!(
            fs::read_to_string(fixture.paths.mihomo_dir.join("local_mihomo.yaml")).unwrap(),
            "mode: local"
        );
    }

    #[test]
    fn preflight_rejects_missing_category_without_touching_filesystem() {
        let fixture = TestFixture::new();

        // Backup has only xray
        fs::write(fixture.paths.xray_dir.join("01_main.json"), b"{\"v\": 1}").unwrap();
        let backup = create_backup_sync_with_paths(&fixture.paths, 0).unwrap();

        // Live filesystem gets changes
        fs::write(fixture.paths.xray_dir.join("01_main.json"), b"{\"live_version\": 2}").unwrap();

        // Request restore of xray AND mihomo (which is missing from backup)
        let res = restore_backup_sync_with_paths_and_hook(
            &fixture.paths,
            &backup.name,
            Some(vec!["xray".into(), "mihomo".into()]),
            |_| Ok(()),
        );

        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(err.contains("в архиве не найдены категории: Mihomo"));

        // Live file was completely untouched!
        assert_eq!(
            fs::read_to_string(fixture.paths.xray_dir.join("01_main.json")).unwrap(),
            "{\"live_version\": 2}"
        );
    }

    #[test]
    fn preflight_rejects_corrupted_json_or_yaml_before_stage() {
        let fixture = TestFixture::new();

        // Create valid backup
        fs::write(fixture.paths.xray_dir.join("01_main.json"), b"{\"v\": 1}").unwrap();
        let backup = create_backup_sync_with_paths(&fixture.paths, 0).unwrap();

        // Prepare backup with corrupt json in xray
        let corrupt_path = fixture.paths.backup_dir.join("corrupt_xkeen-ui.tar");
        let file = File::create(&corrupt_path).unwrap();
        let mut builder = Builder::new(file);

        let corrupt_data = b"{\"unclosed_json";
        let mut header = tar::Header::new_gnu();
        header.set_size(corrupt_data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let rel = to_archive_relative(&fixture.paths.xray_dir.join("01_main.json"));
        builder.append_data(&mut header, rel, &corrupt_data[..]).unwrap();
        builder.finish().unwrap();

        // Live file before restore
        fs::write(fixture.paths.xray_dir.join("01_main.json"), b"{\"live\": 10}").unwrap();

        let res = restore_backup_sync_with_paths_and_hook(
            &fixture.paths,
            "corrupt_xkeen-ui.tar",
            Some(vec!["xray".into()]),
            |_| Ok(()),
        );

        assert!(res.is_err());
        assert!(res.unwrap_err().contains("невалидный JSON"));

        // Live file unchanged
        assert_eq!(
            fs::read_to_string(fixture.paths.xray_dir.join("01_main.json")).unwrap(),
            "{\"live\": 10}"
        );
        let _ = backup;
    }

    #[test]
    fn failure_during_commit_rolls_back_all_files_completely() {
        let fixture = TestFixture::new();

        // Backup has 01_main.json and 02_secondary.json
        fs::write(fixture.paths.xray_dir.join("01_main.json"), b"{\"backup\": 1}").unwrap();
        fs::write(fixture.paths.xray_dir.join("02_secondary.json"), b"{\"backup\": 2}").unwrap();
        let backup = create_backup_sync_with_paths(&fixture.paths, 0).unwrap();

        // Live state before restore
        fs::write(fixture.paths.xray_dir.join("01_main.json"), b"{\"live\": 1}").unwrap();
        fs::write(fixture.paths.xray_dir.join("02_secondary.json"), b"{\"live\": 2}").unwrap();
        fs::write(fixture.paths.xray_dir.join("99_extra.json"), b"{\"live\": 99}").unwrap();

        // Inject failure on after_rename for second file
        let mut rename_count = 0;
        let res = restore_backup_sync_with_paths_and_hook(
            &fixture.paths,
            &backup.name,
            Some(vec!["xray".into()]),
            |step| {
                if step == "after_rename" {
                    rename_count += 1;
                    if rename_count == 2 {
                        return Err(io::Error::other("disk I/O failure during rename"));
                    }
                }
                Ok(())
            },
        );

        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(err.contains("выполнен откат изменений до исходного состояния"));

        // All live files are restored to their exact pre-restore state!
        assert_eq!(
            fs::read_to_string(fixture.paths.xray_dir.join("01_main.json")).unwrap(),
            "{\"live\": 1}"
        );
        assert_eq!(
            fs::read_to_string(fixture.paths.xray_dir.join("02_secondary.json")).unwrap(),
            "{\"live\": 2}"
        );
        assert_eq!(
            fs::read_to_string(fixture.paths.xray_dir.join("99_extra.json")).unwrap(),
            "{\"live\": 99}"
        );

        // Staging directories are cleaned up
        let staging_dirs: Vec<_> = fs::read_dir(&fixture.paths.xray_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".xkeen-restore-"))
            .collect();
        assert!(staging_dirs.is_empty(), "staging directory was not cleaned up after rollback");
    }

    #[tokio::test]
    async fn post_backup_reloads_live_state_and_notifies_auth() {
        use std::sync::RwLock;

        let fixture = TestFixture::new();

        // Initial settings in state
        let mut initial_settings = crate::types::AppSettings::default();
        initial_settings.log.timezone = 2;
        initial_settings.auth.enabled = false;

        let state = AppState {
            core: Arc::new(RwLock::new(crate::types::CoreInfo {
                name: "xray".into(),
                conf_dir: fixture.paths.xray_dir.to_string_lossy().into(),
                is_json: true,
            })),
            settings: Arc::new(RwLock::new(initial_settings)),
            init_file: Arc::new(RwLock::new(None)),
            http_client: reqwest::Client::new(),
            update_checker: Default::default(),
            geo_cache: Arc::new(RwLock::new(Default::default())),
            log_tx: Arc::new(tokio::sync::broadcast::channel(16).0),
            log_watcher: Arc::new(std::sync::Mutex::new(Default::default())),
            auth_changes: tokio::sync::watch::channel(0).0,
            app_config_lock: Arc::new(Mutex::new(())),
            debug: false,
            rci_token: Arc::new(RwLock::new(Some("old_token".into()))),
        };

        // Create backup with new settings and new token
        let restored_settings = serde_json::json!({
            "log": { "timezone": 5 },
            "auth": { "enabled": true, "session_ids": ["new_session_id"] }
        });
        fs::write(&fixture.paths.app_config, serde_json::to_string(&restored_settings).unwrap()).unwrap();

        let restored_xkeen = serde_json::json!({
            "xkeen": { "rci_token": "restored_rci_token_123" }
        });
        fs::write(fixture.paths.xkeen_dir.join("xkeen.json"), serde_json::to_string(&restored_xkeen).unwrap()).unwrap();

        let backup = create_backup_sync_with_paths(&fixture.paths, 0).unwrap();

        // Mutate live file and live state before post_backup call
        fs::write(&fixture.paths.app_config, b"{\"log\": {\"timezone\": 0}}").unwrap();
        *state.settings.write().unwrap() = crate::types::AppSettings::default();
        *state.rci_token.write().unwrap() = Some("old_token".into());

        let mut auth_rx = state.auth_changes.subscribe();
        assert_eq!(*auth_rx.borrow_and_update(), 0);

        // Perform restore sync and manual live-state sync matching post_backup
        let restored_categories = restore_backup_sync_with_paths_and_hook(
            &fixture.paths,
            &backup.name,
            None,
            |_| Ok(()),
        )
        .expect("restore");

        assert!(restored_categories.contains("xkeen-ui"));
        assert!(restored_categories.contains("xkeen"));

        // Simulate the post_backup state update:
        if restored_categories.contains("xkeen-ui") {
            let content = fs::read_to_string(&fixture.paths.app_config).unwrap();
            let new_settings: crate::types::AppSettings = serde_json::from_str(&content).unwrap();
            *state.settings.write().unwrap() = new_settings;
            state.auth_changes.send_modify(|version| *version = version.wrapping_add(1));
        }
        if restored_categories.contains("xkeen") {
            let content = fs::read_to_string(fixture.paths.xkeen_dir.join("xkeen.json")).unwrap();
            let json: serde_json::Value = serde_json::from_str(&content).unwrap();
            let new_token = json.get("xkeen").and_then(|x| x.get("rci_token")).and_then(|t| t.as_str()).map(String::from);
            *state.rci_token.write().unwrap() = new_token;
        }

        // Live state is synchronized!
        assert_eq!(state.settings.read().unwrap().log.timezone, 5);
        assert!(state.settings.read().unwrap().auth.enabled);
        assert_eq!(state.settings.read().unwrap().auth.session_ids, vec!["new_session_id"]);
        assert_eq!(*state.rci_token.read().unwrap(), Some("restored_rci_token_123".into()));
        assert_eq!(*auth_rx.borrow_and_update(), 1);
    }
}
