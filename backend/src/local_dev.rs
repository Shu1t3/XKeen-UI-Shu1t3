use crate::types::*;
use axum::Json;
use axum::extract::Request;
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub fn prepare() -> std::io::Result<()> {
    for dir in [
        XKEEN_CONF_DIR,
        XRAY_CONF_DIR,
        XRAY_ASSET_DIR,
        MIHOMO_CONF_DIR,
        opt_path!("/var/log/xray"),
        opt_path!("/backups"),
        opt_path!("/tmp"),
        opt_path!("/sbin"),
    ] {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn requires_router(path: &str, method: &Method) -> bool {
    matches!(path, "/api/update" | "/api/system" | "/api/device-list")
        || (path == "/api/control" && method != Method::GET)
}

pub async fn router_operations(req: Request, next: Next) -> Response {
    if cfg!(feature = "local-dev") && requires_router(req.uri().path(), req.method()) {
        return Json(serde_json::json!({
            "success": false,
            "error": "Эта операция требует окружения роутера и недоступна в local-dev"
        }))
        .into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_operations_leave_local_editing_available() {
        assert!(requires_router("/api/control", &Method::POST));
        assert!(requires_router("/api/update", &Method::POST));
        assert!(requires_router("/api/system", &Method::GET));
        assert!(requires_router("/api/device-list", &Method::GET));
        assert!(!requires_router("/api/control", &Method::GET));
        assert!(!requires_router("/api/settings", &Method::PATCH));
        assert!(!requires_router("/api/configs", &Method::POST));
        assert!(!requires_router("/api/route-test", &Method::POST));
    }

    #[test]
    fn paths_match_build_mode() {
        if cfg!(feature = "local-dev") {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join(".local/opt");
            assert!(std::path::Path::new(APP_CONFIG).starts_with(root));
            assert!(!APP_CONFIG.contains(".."));
            assert!(!XRAY_CONF_DIR.starts_with("/opt/"));
        } else {
            assert_eq!(APP_CONFIG, "/opt/etc/xkeen/xkeen-ui.json");
            assert_eq!(XRAY_CONF_DIR, "/opt/etc/xray/configs");
        }
    }
}
