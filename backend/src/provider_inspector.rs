use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

use crate::types::{ApiResponse, MIHOMO_CONF, MIHOMO_CONF_DIR};

const RULE_PROVIDERS: &str = "rule-providers";
const PROXY_PROVIDERS: &str = "proxy-providers";
const MAX_PROVIDER_FILE_BYTES: usize = 10 * 1024 * 1024;
const CONVERT_TIMEOUT: Duration = Duration::from_secs(15);

type CachedMihomoYaml = Option<(SystemTime, Arc<Vec<Yaml>>)>;

static MIHOMO_YAML_CACHE: LazyLock<RwLock<CachedMihomoYaml>> = LazyLock::new(|| RwLock::new(None));

pub(crate) async fn load_mihomo_yaml() -> Result<Arc<Vec<Yaml>>, String> {
    let mtime = tokio::fs::metadata(MIHOMO_CONF)
        .await
        .and_then(|metadata| metadata.modified())
        .map_err(|e| format!("Ошибка чтения конфига: {e}"))?;

    let cached = MIHOMO_YAML_CACHE
        .read()
        .unwrap()
        .as_ref()
        .filter(|(cached_mtime, _)| *cached_mtime == mtime)
        .map(|(_, docs)| docs.clone());
    if let Some(docs) = cached {
        return Ok(docs);
    }

    let content = tokio::fs::read_to_string(MIHOMO_CONF)
        .await
        .map_err(|e| format!("Ошибка чтения конфига: {e}"))?;
    let docs = Arc::new(YamlLoader::load_from_str(&content).map_err(|e| format!("Ошибка парсинга YAML: {e}"))?);
    *MIHOMO_YAML_CACHE.write().unwrap() = Some((mtime, docs.clone()));
    crate::route_test::providers::clear_provider_cache();
    Ok(docs)
}

/// Принудительно инвалидирует кэш YAML конфига и кэш провайдеров.
pub(crate) fn invalidate_mihomo_yaml_cache() {
    *MIHOMO_YAML_CACHE.write().unwrap() = None;
    crate::route_test::providers::clear_provider_cache();
    crate::api_relay::invalidate_clash_target_cache();
}

#[derive(Deserialize)]
pub struct ProviderQuery {
    pub name: String,
    #[serde(rename = "vehicleType")]
    pub vehicle_type: Option<String>,
    pub format: Option<String>,
    pub behavior: Option<String>,
}

impl ProviderQuery {
    fn is_vehicle(&self, expected: &str) -> bool {
        self.vehicle_type
            .as_deref()
            .is_some_and(|vehicle| vehicle.eq_ignore_ascii_case(expected))
    }

    fn is_mrs(&self) -> bool {
        self.format
            .as_deref()
            .is_some_and(|format| format.eq_ignore_ascii_case("mrs") || format.eq_ignore_ascii_case("mrsrule"))
    }
}

#[derive(Deserialize)]
pub struct SaveProviderRequest {
    pub content: String,
}

fn find_provider<'a>(docs: &'a [Yaml], section: &str, name: &str) -> Result<&'a Yaml, String> {
    let provider = &docs.first().ok_or("YAML пуст")?[section][name];
    if provider.is_badvalue() {
        return Err(format!("Провайдер '{name}' не найден"));
    }
    Ok(provider)
}

fn provider_file_path(provider: &Yaml, section: &str) -> Result<(String, bool), String> {
    if let Some(path) = provider["path"].as_str() {
        return Ok((resolve_provider_path(path)?, true));
    }
    let url = provider["url"].as_str().ok_or("В провайдере нет ни path, ни url")?;
    let subdirectory = if section == RULE_PROVIDERS { "rules" } else { "proxies" };
    Ok((format!("{MIHOMO_CONF_DIR}/{subdirectory}/{:x}", md5::compute(url)), false))
}

fn inline_payload(provider: &Yaml, section: &str) -> Option<String> {
    if section == PROXY_PROVIDERS {
        return proxies_payload_to_yaml(&provider["payload"]);
    }
    let items: Vec<&str> = provider["payload"].as_vec()?.iter().filter_map(Yaml::as_str).collect();
    (!items.is_empty()).then(|| items.join("\n"))
}

async fn read_provider(section: &str, query: &ProviderQuery) -> Result<String, String> {
    let docs = load_mihomo_yaml().await?;
    let provider = find_provider(&docs, section, &query.name)?;

    if query.is_vehicle("inline") {
        return inline_payload(provider, section).ok_or_else(|| "Payload пуст или не найден".into());
    }

    let (path, _) = provider_file_path(provider, section)?;
    if section == RULE_PROVIDERS && query.is_mrs() {
        return convert_mrs(&path, query.behavior.as_deref().unwrap_or("domain")).await;
    }
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("Не удалось прочитать файл {path}: {e}"))
}

async fn write_provider(section: &str, query: &ProviderQuery, content: &str) -> Result<(), String> {
    if !query.is_vehicle("file") {
        return Err("Только file-провайдеры доступны для редактирования".into());
    }
    if section == RULE_PROVIDERS && query.is_mrs() {
        return Err("MRS-провайдер нельзя редактировать".into());
    }

    let docs = load_mihomo_yaml().await?;
    let (path, has_explicit_path) = provider_file_path(find_provider(&docs, section, &query.name)?, section)?;
    if !has_explicit_path {
        return Err("У провайдера нет path — редактируется только file-провайдер".into());
    }
    write_provider_file(&path, content).await?;
    invalidate_mihomo_yaml_cache();
    Ok(())
}

async fn write_provider_file(path: &str, content: &str) -> Result<(), String> {
    if content.len() > MAX_PROVIDER_FILE_BYTES {
        return Err("Файл слишком большой (лимит 10 МБ)".into());
    }
    if let Some(parent) = Path::new(path).parent().filter(|parent| !parent.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Не удалось создать каталог {parent:?}: {e}"))?;
    }
    crate::config_transaction::write_atomic(Path::new(path), content.as_bytes(), false)
        .map_err(|e| format!("Не удалось записать файл {path}: {e}"))
}

pub async fn get_rule_providers_content(Query(query): Query<ProviderQuery>) -> Response {
    respond(read_provider(RULE_PROVIDERS, &query).await)
}

pub async fn get_proxy_provider_content(Query(query): Query<ProviderQuery>) -> Response {
    respond(read_provider(PROXY_PROVIDERS, &query).await)
}

pub async fn put_rule_providers_content(
    Query(query): Query<ProviderQuery>,
    Json(request): Json<SaveProviderRequest>,
) -> Response {
    respond(
        write_provider(RULE_PROVIDERS, &query, &request.content)
            .await
            .map(|()| String::new()),
    )
}

pub async fn put_proxy_provider_content(
    Query(query): Query<ProviderQuery>,
    Json(request): Json<SaveProviderRequest>,
) -> Response {
    respond(
        write_provider(PROXY_PROVIDERS, &query, &request.content)
            .await
            .map(|()| String::new()),
    )
}

fn proxies_payload_to_yaml(payload: &Yaml) -> Option<String> {
    let items = payload.as_vec().filter(|items| !items.is_empty())?;
    let mut root = yaml_rust2::yaml::Hash::new();
    root.insert(Yaml::String("proxies".into()), Yaml::Array(items.clone()));
    let mut content = String::new();
    YamlEmitter::new(&mut content).dump(&Yaml::Hash(root)).ok()?;
    Some(content.trim_start_matches("---\n").to_string())
}

struct TmpGuard(PathBuf);
impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(crate) async fn convert_mrs(mrs_path: &str, behavior: &str) -> Result<String, String> {
    let behavior_clean = behavior.trim().to_ascii_lowercase();
    if !matches!(behavior_clean.as_str(), "domain" | "ipcidr" | "classical") {
        return Err(format!("Недопустимый тип behavior для конвертации MRS: {behavior}"));
    }

    if tokio::fs::metadata(mrs_path).await.is_err() {
        return Err(format!("MRS файл не найден: {mrs_path}"));
    }

    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let tmp_path = format!("/tmp/convert-ruleset_{}_{nanos:x}", std::process::id());
    let _guard = TmpGuard(PathBuf::from(&tmp_path));

    let mut cmd = Command::new("/opt/sbin/mihomo");
    cmd.args(["convert-ruleset", behavior_clean.as_str(), "mrs", mrs_path, &tmp_path]);
    cmd.kill_on_drop(true);

    let output = match tokio::time::timeout(CONVERT_TIMEOUT, cmd.output()).await {
        Ok(res) => res.map_err(|e| format!("Ошибка запуска mihomo: {e}"))?,
        Err(_) => return Err("Превышен таймаут выполнения mihomo convert-ruleset (15 с)".into()),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "mihomo convert-ruleset упал с кодом {}: {}",
            output.status, stderr
        ));
    }

    tokio::fs::read_to_string(&tmp_path)
        .await
        .map_err(|e| format!("Ошибка чтения результата конвертации: {e}"))
}

fn resolve_provider_path(path: &str) -> Result<String, String> {
    resolve_provider_path_in(path, MIHOMO_CONF_DIR)
}

pub(crate) fn resolve_provider_path_in(path: &str, base_dir: &str) -> Result<String, String> {
    let base = Path::new(base_dir);
    let candidate = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        base.join(path.trim_start_matches("./"))
    };

    let mut normalized = PathBuf::new();
    for comp in candidate.components() {
        match comp {
            std::path::Component::Prefix(p) => normalized.push(p.as_os_str()),
            std::path::Component::RootDir => normalized.push("/"),
            std::path::Component::CurDir => {},
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err("Обнаружена попытка выхода за пределы каталога".into());
                }
            }
            std::path::Component::Normal(c) => normalized.push(c),
        }
    }

    if !normalized.starts_with(base) {
        return Err(format!("Путь '{}' выходит за пределы базового каталога {}", path, base_dir));
    }

    if let Ok(canon_base) = std::fs::canonicalize(base)
        && let Ok(canon_target) = std::fs::canonicalize(&normalized)
    {
        if !canon_target.starts_with(&canon_base) {
            return Err(format!("Канонический путь '{}' выходит за пределы базового каталога {}", path, base_dir));
        }
        if canon_target.file_name().and_then(|n| n.to_str()) == Some("xkeen-ui.json") {
            return Err("Доступ к файлу настроек запрещен".into());
        }
    }

    Ok(normalized.to_string_lossy().to_string())
}

fn respond(result: Result<String, String>) -> Response {
    match result {
        Ok(content) => (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                error: None,
                data: Some(serde_json::json!({ "content": content })),
            }),
        )
            .into_response(),
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse::<()> {
                success: false,
                error: Some(message),
                data: None,
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalidate_mihomo_yaml_cache_clears_provider_cache() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-inv-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let file_path = temp_dir.join("rules.yaml");
        tokio::fs::write(&file_path, "payload:\n  - domain.com\n").await.unwrap();

        let path_str = file_path.to_str().unwrap();
        let p1 = crate::route_test::providers::load_from_path(
            path_str,
            crate::route_test::providers::Behavior::Domain,
            crate::route_test::providers::ProviderFormat::Yaml,
        )
        .await
        .unwrap();

        invalidate_mihomo_yaml_cache();

        let p2 = crate::route_test::providers::load_from_path(
            path_str,
            crate::route_test::providers::Behavior::Domain,
            crate::route_test::providers::ProviderFormat::Yaml,
        )
        .await
        .unwrap();

        assert!(!Arc::ptr_eq(&p1, &p2), "invalidate_mihomo_yaml_cache должен был очистить provider cache");
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[test]
    fn r37_resolve_provider_path_in_boundary_isolation() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-r37-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let base_dir_str = temp_dir.to_str().unwrap();

        // 1. Valid relative paths
        let valid_rel = resolve_provider_path_in("rules/direct.yaml", base_dir_str);
        assert!(valid_rel.is_ok());
        let valid_dot_slash = resolve_provider_path_in("./rules/direct.yaml", base_dir_str);
        assert!(valid_dot_slash.is_ok());

        // 2. Traversal rejection (..)
        assert!(resolve_provider_path_in("../../etc/shadow", base_dir_str).is_err());
        assert!(resolve_provider_path_in("rules/../../etc/passwd", base_dir_str).is_err());

        // 3. Absolute path outside base_dir
        assert!(resolve_provider_path_in("/etc/shadow", base_dir_str).is_err());

        // 4. Symlink escaping base_dir
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside_dir = std::env::temp_dir().join(format!("xkeen-outside-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&outside_dir).unwrap();
            let outside_file = outside_dir.join("secret.yaml");
            std::fs::write(&outside_file, "payload: []").unwrap();

            let link_file = temp_dir.join("symlink_escape.yaml");
            symlink(&outside_file, &link_file).unwrap();

            let res = resolve_provider_path_in("symlink_escape.yaml", base_dir_str);
            assert!(res.is_err(), "Symlink вне базовой директории должен быть отклонен");

            let _ = std::fs::remove_dir_all(&outside_dir);
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn r38_convert_mrs_validates_behavior() {
        let temp_dir = std::env::temp_dir().join(format!("xkeen-r38-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let mrs_file = temp_dir.join("test.mrs");
        tokio::fs::write(&mrs_file, b"dummy").await.unwrap();
        let mrs_path = mrs_file.to_str().unwrap();

        // Invalid behaviors must be rejected immediately without running mihomo
        assert!(convert_mrs(mrs_path, "invalid_behavior").await.is_err());
        assert!(convert_mrs(mrs_path, "--help").await.is_err());
        assert!(convert_mrs(mrs_path, "domain; rm -rf /").await.is_err());

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}
