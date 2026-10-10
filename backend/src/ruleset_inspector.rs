use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::SystemTime;
use tokio::process::Command;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

use crate::types::{ApiResponse, AppState, MIHOMO_CONF_DIR};

const MIHOMO_CONF_DIRIG_PATH: &str = opt_path!("/etc/mihomo/config.yaml");

type CachedMihomoYaml = Option<(SystemTime, Arc<Vec<Yaml>>)>;

static MIHOMO_YAML_CACHE: LazyLock<RwLock<CachedMihomoYaml>> = LazyLock::new(|| RwLock::new(None));

/// Общий кэш `config.yaml` mihomo по mtime — используется и этим модулем, и `route_test::mihomo`
/// (нужен движку тестера маршрутов, чтобы не парсить YAML на каждый запрос).
pub(crate) async fn load_mihomo_yaml() -> Result<Arc<Vec<Yaml>>, String> {
    let mtime = tokio::fs::metadata(MIHOMO_CONF_DIRIG_PATH)
        .await
        .map_err(|e| format!("Ошибка чтения конфига: {e}"))?
        .modified()
        .map_err(|e| format!("Ошибка чтения mtime: {e}"))?;

    if let Some(cached) = {
        let guard = MIHOMO_YAML_CACHE.read().unwrap();
        guard
            .as_ref()
            .filter(|(ts, _)| *ts == mtime)
            .map(|(_, docs)| docs.clone())
    } {
        return Ok(cached);
    }

    let content = tokio::fs::read_to_string(MIHOMO_CONF_DIRIG_PATH)
        .await
        .map_err(|e| format!("Ошибка чтения конфига: {e}"))?;
    let docs = YamlLoader::load_from_str(&content).map_err(|e| format!("Ошибка парсинга YAML: {e}"))?;
    let arc = Arc::new(docs);
    *MIHOMO_YAML_CACHE.write().unwrap() = Some((mtime, arc.clone()));
    crate::route_test::providers::clear_provider_cache();
    Ok(arc)
}

/// Принудительно инвалидирует кэш YAML конфига и кэш провайдеров.
pub(crate) fn invalidate_mihomo_yaml_cache() {
    *MIHOMO_YAML_CACHE.write().unwrap() = None;
    crate::route_test::providers::clear_provider_cache();
}

#[derive(Deserialize)]
pub struct RuleContentQuery {
    pub name: String,
    pub format: Option<String>,
    pub behavior: Option<String>,
    #[serde(rename = "vehicleType")]
    pub vehicle_type: Option<String>,
}

pub async fn get_ruleset_content(State(_state): State<AppState>, Query(params): Query<RuleContentQuery>) -> Response {
    let docs = match load_mihomo_yaml().await {
        Ok(d) => d,
        Err(e) => return error_response(e),
    };
    let Some(parsed) = docs.first() else {
        return error_response("YAML пуст".into());
    };

    let provider = &parsed["rule-providers"][params.name.as_str()];
    if provider.is_badvalue() {
        return error_response(format!("Провайдер '{}' не найден", params.name));
    }

    if params
        .vehicle_type
        .as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case("inline"))
    {
        let items: Vec<&str> = provider["payload"]
            .as_vec()
            .map(|seq| seq.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        if items.is_empty() {
            return error_response("Payload пуст или не найден".into());
        }
        return ok_response(items.join("\n"));
    }

    let url = provider["url"].as_str();
    let path = provider["path"].as_str();

    let final_path = match path {
        Some(p) => match resolve_provider_path(p) {
            Ok(p) => p,
            Err(e) => return error_response(e),
        },
        None => match url {
            Some(u) => format!("{}/rules/{:x}", MIHOMO_CONF_DIR, md5::compute(u)),
            None => return error_response("В провайдере нет ни path, ни url".into()),
        },
    };

    let is_mrs = params
        .format
        .as_deref()
        .is_some_and(|f| f.eq_ignore_ascii_case("mrs") || f.eq_ignore_ascii_case("mrsrule"));

    let content = if is_mrs {
        let behavior = params.behavior.as_deref().unwrap_or("domain");
        match convert_mrs(&final_path, behavior).await {
            Ok(c) => c,
            Err(e) => return error_response(e),
        }
    } else {
        match tokio::fs::read_to_string(&final_path).await {
            Ok(c) => c,
            Err(e) => {
                return error_response(format!("Не удалось прочитать файл {final_path}: {e}"));
            }
        }
    };

    ok_response(content)
}

#[derive(Deserialize)]
pub struct ProxyProviderContentQuery {
    pub name: String,
    #[serde(rename = "vehicleType")]
    pub vehicle_type: Option<String>,
}

pub async fn get_proxy_provider_content(
    State(_state): State<AppState>, Query(params): Query<ProxyProviderContentQuery>,
) -> Response {
    let docs = match load_mihomo_yaml().await {
        Ok(d) => d,
        Err(e) => return error_response(e),
    };
    let Some(parsed) = docs.first() else {
        return error_response("YAML пуст".into());
    };

    let provider = &parsed["proxy-providers"][params.name.as_str()];
    if provider.is_badvalue() {
        return error_response(format!("Провайдер '{}' не найден", params.name));
    }

    if params
        .vehicle_type
        .as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case("inline"))
    {
        let content = match proxies_payload_to_yaml(&provider["payload"]) {
            Some(c) => c,
            None => return error_response("Payload пуст или не найден".into()),
        };
        return ok_response(content);
    }

    let url = provider["url"].as_str();
    let path = provider["path"].as_str();

    let final_path = match path {
        Some(p) => match resolve_provider_path(p) {
            Ok(p) => p,
            Err(e) => return error_response(e),
        },
        None => match url {
            Some(u) => format!("{}/proxies/{:x}", MIHOMO_CONF_DIR, md5::compute(u)),
            None => return error_response("В провайдере нет ни path, ни url".into()),
        },
    };

    match tokio::fs::read_to_string(&final_path).await {
        Ok(content) => ok_response(content),
        Err(e) => error_response(format!("Не удалось прочитать файл {final_path}: {e}")),
    }
}

fn proxies_payload_to_yaml(payload: &Yaml) -> Option<String> {
    let items = payload.as_vec()?;
    if items.is_empty() {
        return None;
    }
    let mut root = yaml_rust2::yaml::Hash::new();
    root.insert(Yaml::String("proxies".into()), Yaml::Array(items.clone()));
    let mut content = String::new();
    YamlEmitter::new(&mut content).dump(&Yaml::Hash(root)).ok()?;
    Some(content.trim_start_matches("---\n").to_string())
}

/// Конвертирует `.mrs` в текстовый список правил через `mihomo convert-ruleset <behavior> mrs`.
/// Используется хендлером `/api/ruleset` и `route_test::providers` (там же логика, что и в
/// `ConvertMain`/`ConvertToMrs` mihomo: при исходном формате `mrs` результат — plain-текстовый
/// дамп, по одной записи на строку, вне зависимости от того, что было в исходном провайдере).
pub(crate) async fn convert_mrs(mrs_path: &str, behavior: &str) -> Result<String, String> {
    if tokio::fs::metadata(mrs_path).await.is_err() {
        return Err(format!("MRS файл не найден: {mrs_path}"));
    }

    let behavior = behavior.to_ascii_lowercase();
    match behavior.as_str() {
        "domain" | "ipcidr" | "classical" => {}
        _ => return Err(format!("Недопустимый behavior: '{behavior}'. Разрешены только: domain, ipcidr, classical")),
    }

    let tmp_path = format!("/tmp/convert-ruleset_{}", random_suffix());

    struct TmpGuard(PathBuf);
    impl Drop for TmpGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = TmpGuard(PathBuf::from(&tmp_path));

    let mut cmd = Command::new(opt_path!("/sbin/mihomo"));
    cmd.args(["convert-ruleset", behavior.as_str(), "mrs", mrs_path, &tmp_path])
        .kill_on_drop(true);

    let output = match tokio::time::timeout(std::time::Duration::from_secs(15), cmd.output()).await {
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

    let content = tokio::fs::read_to_string(&tmp_path)
        .await
        .map_err(|e| format!("Ошибка чтения результата конвертации: {e}"))?;

    Ok(content)
}

fn random_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08x}", nanos ^ std::process::id().wrapping_shl(8))
}

fn resolve_provider_path(path: &str) -> Result<String, String> {
    resolve_provider_path_in(path, MIHOMO_CONF_DIR)
}

/// Резолвит относительный `path` провайдера к каталогу mihomo. Параметризовано по `base_dir`,
/// чтобы `route_test::providers` могло переиспользовать ту же логику с временным каталогом
/// в тестах (боевой код всегда зовёт через `resolve_provider_path`, привязанный к `MIHOMO_CONF_DIR`).
pub(crate) fn resolve_provider_path_in(path: &str, base_dir: &str) -> Result<String, String> {
    if path.contains("..") {
        return Err("Обнаружена попытка выхода за пределы каталога (..)".into());
    }
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

fn ok_response(content: String) -> Response {
    (
        StatusCode::OK,
        axum::Json(ApiResponse {
            success: true,
            error: None,
            data: Some(serde_json::json!({ "content": content })),
        }),
    )
        .into_response()
}

fn error_response(msg: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(ApiResponse::<()> {
            success: false,
            error: Some(msg),
            data: None,
        }),
    )
        .into_response()
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
