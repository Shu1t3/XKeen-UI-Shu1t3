use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::types::{APP_CONFIG, ApiResponse, AppState, XKEEN_CONF_DIR};

#[cfg(test)]
tokio::task_local! {
    pub static TEST_AUTH_CONFIG_OVERRIDE: (std::path::PathBuf, std::path::PathBuf);
}

fn current_config_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    #[cfg(test)]
    {
        if let Ok(paths) = TEST_AUTH_CONFIG_OVERRIDE.try_with(|p| p.clone()) {
            return paths;
        }
    }
    (
        std::path::PathBuf::from(APP_CONFIG),
        std::path::PathBuf::from(XKEEN_CONF_DIR),
    )
}

const SESSION_COOKIE: &str = "session_id";
const MAX_ATTEMPTS: u32 = 5;
const LOCKOUT_SECS: u64 = 60;

const MAX_TRACKED_CLIENTS: usize = 1024;
type AttemptCache = Mutex<HashMap<IpAddr, (u32, Instant)>>;

static BRUTE_CACHE: LazyLock<AttemptCache> = LazyLock::new(|| Mutex::new(HashMap::new()));
// One Argon2 operation at a time keeps memory use bounded on ARM/MIPS routers.
static PASSWORD_WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

fn reserve_attempt(cache: &AttemptCache, ip: IpAddr, now: Instant) -> Result<u32, ()> {
    let mut cache = cache.lock().unwrap();
    cache.retain(|_, (_, last)| now.duration_since(*last) < Duration::from_secs(LOCKOUT_SECS));
    if !cache.contains_key(&ip) && cache.len() >= MAX_TRACKED_CLIENTS {
        return Err(());
    }
    let entry = cache.entry(ip).or_insert((0, now));
    if entry.0 >= MAX_ATTEMPTS {
        return Err(());
    }
    // Reserve before spawning work, so parallel/cancelled requests cannot bypass the limit.
    entry.0 += 1;
    entry.1 = now;
    Ok(entry.0)
}

async fn password_operation<T: Send + 'static>(
    permit: OwnedSemaphorePermit, operation: impl FnOnce() -> T + Send + 'static,
) -> Result<(T, OwnedSemaphorePermit), tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        // Keep ownership inside the blocking task even if the request is cancelled.
        let value = operation();
        (value, permit)
    })
    .await
}

fn attempts_blocked() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(ApiResponse::<()> {
            success: false,
            error: Some(format!(
                "Слишком много попыток. Повторите через {} секунд",
                LOCKOUT_SECS
            )),
            data: None,
        }),
    )
        .into_response()
}

fn password_worker_busy() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(ApiResponse::<()> {
            success: false,
            error: Some("Проверка пароля занята. Повторите позже".into()),
            data: None,
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct PasswordReq {
    password: String,
    #[serde(default)]
    remember: bool,
}

fn now_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn get_session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|pair| pair.trim().strip_prefix("session_id="))
}

fn is_session_valid(session_ids: &[String], cookie: &str) -> bool {
    let ts = now_ts();
    session_ids.iter().any(|id| {
        if id == cookie {
            return true;
        }
        if let Some((uid, exp_str)) = id.split_once(':')
            && uid == cookie {
                return exp_str.parse::<u64>().unwrap_or(0) > ts;
            }
        false
    })
}

fn set_cookie_header(headers_in: &HeaderMap, value: String, max_age: u64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let secure = if headers_in.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https") {
        "Secure; "
    } else {
        ""
    };
    let cookie = if max_age > 0 {
        format!(
            "{}={}; HttpOnly; {}SameSite=Strict; Path=/; Max-Age={}",
            SESSION_COOKIE, value, secure, max_age
        )
    } else {
        format!(
            "{}={}; HttpOnly; {}SameSite=Strict; Path=/",
            SESSION_COOKIE, value, secure
        )
    };
    headers.insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    headers
}

fn clear_cookie_header() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_static("session_id=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
    );
    headers
}

pub async fn get_login_info(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let s = state.settings.read().unwrap();
    let authenticated =
        get_session_cookie(&headers).is_some_and(|cookie| is_session_valid(&s.auth.session_ids, cookie));
    Json(serde_json::json!({
        "enabled": s.auth.enabled,
        "has_password": s.auth.password_hash.is_some(),
        "authenticated": authenticated
    }))
}

pub async fn post_setup(
    State(state): State<AppState>, headers: HeaderMap, Json(req): Json<PasswordReq>,
) -> impl IntoResponse {
    if state.settings.read().unwrap().auth.password_hash.is_some() {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiResponse::<()> {
                success: false,
                error: Some("Password already set".into()),
                data: None,
            }),
        )
            .into_response();
    }

    let _enrollment_guard = state.enrollment_lock.lock().await;

    if state.settings.read().unwrap().auth.password_hash.is_some() {
        return (
            StatusCode::FORBIDDEN,
            Json(ApiResponse::<()> {
                success: false,
                error: Some("Password already set".into()),
                data: None,
            }),
        )
            .into_response();
    }

    let permit = match PASSWORD_WORKERS.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return password_worker_busy(),
    };
    let password = req.password;
    let (hash, _permit) = password_operation(permit, move || hash_password(&password))
        .await
        .unwrap();
    let session_id = Uuid::new_v4().to_string();
    let ttl = 86400;
    let session_val = format!("{}:{}", session_id, now_ts() + ttl);

    let commit_res = commit_setup(&state, hash, session_val).await;

    match commit_res {
        Ok(()) => {
            (
                set_cookie_header(&headers, session_id, 0),
                Json(ApiResponse::<()> {
                    success: true,
                    error: None,
                    data: None,
                }),
            )
                .into_response()
        }
        Err(AuthSetupError::AlreadySet) => {
            (
                StatusCode::FORBIDDEN,
                Json(ApiResponse::<()> {
                    success: false,
                    error: Some("Password already set".into()),
                    data: None,
                }),
            )
                .into_response()
        }
        Err(AuthSetupError::Io(e)) => {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiResponse::<()> {
                    success: false,
                    error: Some(format!("Ошибка сохранения: {}", e)),
                    data: None,
                }),
            )
                .into_response()
        }
    }
}

pub async fn post_login(
    State(state): State<AppState>, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap,
    Json(req): Json<PasswordReq>,
) -> Response {
    let ip = addr.ip();
    let hash = match state.settings.read().unwrap().auth.password_hash.clone() {
        Some(h) => h,
        None => {
            return (
                StatusCode::FORBIDDEN,
                Json(ApiResponse::<()> {
                    success: false,
                    error: Some("No password set".into()),
                    data: None,
                }),
            )
                .into_response();
        }
    };

    let permit = match PASSWORD_WORKERS.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return password_worker_busy(),
    };
    let attempt = match reserve_attempt(&BRUTE_CACHE, ip, Instant::now()) {
        Ok(attempt) => attempt,
        Err(()) => return attempts_blocked(),
    };
    let password = req.password;
    let (is_valid, _permit) = match password_operation(permit, move || verify_password(&password, &hash)).await {
        Ok(result) => result,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    if !is_valid {
        if let Some(entry) = BRUTE_CACHE.lock().unwrap().get_mut(&ip) {
            entry.1 = Instant::now();
        }
        println!(
            "{} [WARN] Incorrect password attempt [{}] ({}/{})",
            crate::logger::ts(),
            ip,
            attempt,
            MAX_ATTEMPTS
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(ApiResponse::<()> {
                success: false,
                error: Some("Неверный пароль".into()),
                data: None,
            }),
        )
            .into_response();
    }

    BRUTE_CACHE.lock().unwrap().remove(&ip);
    println!("{} [INFO] Successful auth {}", crate::logger::ts(), ip);

    let max_age = if req.remember { 2592000 } else { 0 };
    let backend_ttl = if req.remember { 2592000 } else { 86400 };

    let session_id = Uuid::new_v4().to_string();
    let session_val = format!("{}:{}", session_id, now_ts() + backend_ttl);

    let update_res = update_auth(&state, |auth| {
        auth.session_ids.push(session_val);
    })
    .await;

    if let Err(e) = update_res {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiResponse::<()> {
                success: false,
                error: Some(format!("Ошибка сохранения сессии: {}", e)),
                data: None,
            }),
        )
            .into_response();
    }

    (
        set_cookie_header(&headers, session_id, max_age),
        Json(ApiResponse::<()> {
            success: true,
            error: None,
            data: None,
        }),
    )
        .into_response()
}

pub async fn post_logout(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(cookie) = get_session_cookie(&headers) {
        let cookie = cookie.to_string();
        if let Err(e) = update_auth(&state, |auth| {
            auth.session_ids.retain(|id| id.split_once(':').map_or(id.as_str(), |(uid, _)| uid) != cookie)
        })
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                clear_cookie_header(),
                Json(ApiResponse::<()> {
                    success: false,
                    error: Some(format!("Ошибка сохранения: {}", e)),
                    data: None,
                }),
            )
                .into_response();
        }
    }
    (
        clear_cookie_header(),
        Json(ApiResponse::<()> {
            success: true,
            error: None,
            data: None,
        }),
    )
        .into_response()
}

pub async fn post_auth_reset(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(e) = update_auth(&state, |auth| {
        auth.password_hash = None;
        auth.session_ids.clear();
    })
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            clear_cookie_header(),
            Json(ApiResponse::<()> {
                success: false,
                error: Some(format!("Ошибка сброса авторизации: {}", e)),
                data: None,
            }),
        )
            .into_response();
    }
    (
        clear_cookie_header(),
        Json(ApiResponse::<()> {
            success: true,
            error: None,
            data: None,
        }),
    )
        .into_response()
}

/// Upgrade captures the admitted identity; disabling auth cannot resurrect a revoked session.
#[derive(Clone)]
pub struct WsSession {
    state: AppState,
    cookie: Option<String>,
    changes: tokio::sync::watch::Receiver<u64>,
}

impl WsSession {
    pub fn from_state(state: &AppState, headers: &HeaderMap) -> Self {
        let changes = state.auth_changes.subscribe();
        let auth = &state.settings.read().unwrap().auth;
        let cookie = get_session_cookie(headers)
            .filter(|cookie| auth.enabled || is_session_valid(&auth.session_ids, cookie))
            .map(str::to_string);
        Self { state: state.clone(), cookie, changes }
    }

    fn authorized_with(&self, auth: &crate::types::AuthSettings) -> bool {
        match &self.cookie {
            Some(cookie) => is_session_valid(&auth.session_ids, cookie),
            None => !auth.enabled,
        }
    }

    pub fn authorized(&self) -> bool {
        self.authorized_with(&self.state.settings.read().unwrap().auth)
    }

    /// Order destructive commands with logout/reset's settings write lock.
    pub fn if_authorized<T>(&self, work: impl FnOnce() -> T) -> Option<T> {
        let settings = self.state.settings.read().unwrap();
        self.authorized_with(&settings.auth).then(work)
    }

    pub async fn revoked(&mut self) {
        loop {
            // Mark current notifications seen before checking to avoid a check/subscribe race.
            self.changes.borrow_and_update();
            if !self.authorized() { return; }
            let expiry = {
                let auth = &self.state.settings.read().unwrap().auth;
                self.cookie.as_ref().and_then(|cookie| {
                    if auth.session_ids.iter().any(|id| id == cookie) { return None; }
                    auth.session_ids.iter().find_map(|id| {
                        let (uid, expiry) = id.split_once(':')?;
                        (uid == cookie).then(|| expiry.parse::<u64>().ok()).flatten()
                    })
                })
            };
            tokio::select! {
                result = self.changes.changed() => { if result.is_err() { return; } },
                _ = async {
                    match expiry {
                        Some(exp) => {
                            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                            let delay = Duration::from_secs(exp).saturating_sub(now).min(Duration::from_secs(86400));
                            tokio::time::sleep(delay).await;
                        },
                        None => std::future::pending::<()>().await,
                    }
                } => {},
            }
        }
    }
}

pub async fn auth_middleware(state: AppState, mut request: Request, next: Next) -> Response {
    let session = WsSession::from_state(&state, request.headers());
    if !session.authorized() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    request.extensions_mut().insert(session);
    next.run(request).await
}

fn hash_password(password: &str) -> String {
    Argon2::default()
        .hash_password(password.as_bytes())
        .unwrap()
        .to_string()
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

#[derive(Debug)]
pub enum AuthSetupError {
    AlreadySet,
    Io(std::io::Error),
}

impl std::fmt::Display for AuthSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadySet => write!(f, "Password already set"),
            Self::Io(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for AuthSetupError {}

#[derive(Debug)]
pub enum AuthModifyError<E> {
    Custom(E),
    Io(std::io::Error),
}

impl<E: std::fmt::Display> std::fmt::Display for AuthModifyError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Custom(e) => write!(f, "{}", e),
            Self::Io(e) => write!(f, "{}", e),
        }
    }
}

impl<E: std::fmt::Display + std::fmt::Debug> std::error::Error for AuthModifyError<E> {}

pub async fn update_auth_modify_at<E>(
    state: &AppState,
    config_path: &std::path::Path,
    conf_dir: &std::path::Path,
    modify: impl FnOnce(&mut crate::types::AuthSettings) -> Result<(), E>,
) -> Result<(), AuthModifyError<E>> {
    let _guard = state.app_config_lock.lock().await;

    let mut new_auth = state.settings.read().unwrap().auth.clone();
    modify(&mut new_auth).map_err(AuthModifyError::Custom)?;

    let ts = now_ts();
    new_auth.session_ids.retain(|id| {
        if let Some((_, exp)) = id.split_once(':') {
            exp.parse::<u64>().unwrap_or(0) > ts
        } else {
            true
        }
    });

    let mut file_json: serde_json::Value = tokio::fs::read_to_string(config_path)
        .await
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| {
            let current = state.settings.read().unwrap();
            serde_json::to_value(&*current).unwrap_or(serde_json::json!({}))
        });

    if !file_json.is_object() {
        let current = state.settings.read().unwrap();
        file_json = serde_json::to_value(&*current).unwrap_or(serde_json::json!({}));
    }

    file_json["auth"] = serde_json::to_value(&new_auth)
        .map_err(|e| AuthModifyError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))?;

    tokio::fs::create_dir_all(conf_dir)
        .await
        .map_err(AuthModifyError::Io)?;

    let serialized = serde_json::to_string_pretty(&file_json)
        .map_err(|e| AuthModifyError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))?;

    let tmp = format!("{}.tmp", config_path.display());
    tokio::fs::write(&tmp, &serialized)
        .await
        .map_err(AuthModifyError::Io)?;
    if let Err(e) = tokio::fs::rename(&tmp, config_path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(AuthModifyError::Io(e));
    }

    // Disk commit confirmed: publish to in-memory state and notify subscribers
    {
        let mut s = state.settings.write().unwrap();
        s.auth = new_auth;
    }
    state.auth_changes.send_modify(|version| *version = version.wrapping_add(1));

    Ok(())
}

async fn update_auth(
    state: &AppState,
    modify: impl FnOnce(&mut crate::types::AuthSettings),
) -> Result<(), std::io::Error> {
    let (config_path, conf_dir) = current_config_paths();
    update_auth_at(state, &config_path, &conf_dir, modify).await
}

pub async fn update_auth_at(
    state: &AppState,
    config_path: &std::path::Path,
    conf_dir: &std::path::Path,
    modify: impl FnOnce(&mut crate::types::AuthSettings),
) -> Result<(), std::io::Error> {
    match update_auth_modify_at(state, config_path, conf_dir, |auth| {
        modify(auth);
        Ok::<(), std::convert::Infallible>(())
    })
    .await
    {
        Ok(()) => Ok(()),
        Err(AuthModifyError::Io(e)) => Err(e),
        Err(AuthModifyError::Custom(infallible)) => match infallible {},
    }
}

async fn commit_setup(
    state: &AppState,
    hash: String,
    session_val: String,
) -> Result<(), AuthSetupError> {
    let (config_path, conf_dir) = current_config_paths();
    commit_setup_at(state, &config_path, &conf_dir, hash, session_val).await
}

pub async fn commit_setup_at(
    state: &AppState,
    config_path: &std::path::Path,
    conf_dir: &std::path::Path,
    hash: String,
    session_val: String,
) -> Result<(), AuthSetupError> {
    update_auth_modify_at(state, config_path, conf_dir, |auth| {
        if auth.password_hash.is_some() {
            return Err(AuthSetupError::AlreadySet);
        }
        auth.password_hash = Some(hash);
        auth.session_ids.push(session_val);
        Ok(())
    })
    .await
    .map_err(|e| match e {
        AuthModifyError::Custom(err) => err,
        AuthModifyError::Io(io_err) => AuthSetupError::Io(io_err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AppSettings, UpdateChecker};
    use axum::{Router, routing::post};
    use std::sync::{Barrier, RwLock};

    #[test]
    fn attempts_expire_without_blocked_requests_extending_lockout() {
        let cache = Mutex::new(HashMap::new());
        let ip = "192.0.2.1".parse().unwrap();
        let now = Instant::now();
        for count in 1..=MAX_ATTEMPTS {
            assert_eq!(reserve_attempt(&cache, ip, now), Ok(count));
        }
        assert_eq!(reserve_attempt(&cache, ip, now + Duration::from_secs(59)), Err(()));
        assert_eq!(reserve_attempt(&cache, ip, now + Duration::from_secs(60)), Ok(1));
    }

    #[test]
    fn parallel_requests_reserve_only_five_attempts() {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let barrier = Arc::new(Barrier::new(32));
        let threads: Vec<_> = (0..32)
            .map(|_| {
                let cache = cache.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve_attempt(&cache, "192.0.2.2".parse().unwrap(), Instant::now()).is_ok()
                })
            })
            .collect();
        let admitted = threads.into_iter().map(|t| u32::from(t.join().unwrap())).sum::<u32>();
        assert_eq!(admitted, MAX_ATTEMPTS);
    }

    #[test]
    fn attempt_cache_is_bounded_and_expires() {
        let cache = Mutex::new(HashMap::new());
        let now = Instant::now();
        for n in 0..MAX_TRACKED_CLIENTS as u32 {
            assert_eq!(reserve_attempt(&cache, IpAddr::V4((n + 1).into()), now), Ok(1));
        }
        let extra = "2001:db8::1".parse().unwrap();
        assert_eq!(reserve_attempt(&cache, extra, now), Err(()));
        assert_eq!(cache.lock().unwrap().len(), MAX_TRACKED_CLIENTS);
        assert_eq!(reserve_attempt(&cache, extra, now + Duration::from_secs(60)), Ok(1));
        assert_eq!(cache.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn password_worker_remains_occupied_when_request_is_cancelled() {
        let workers = Arc::new(Semaphore::new(1));
        let permit = workers.clone().try_acquire_owned().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(password_operation(permit, move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(workers.clone().try_acquire_owned().is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(workers.clone().try_acquire_owned().is_err());
        release_tx.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(5), workers.clone().acquire_owned())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
    }

    #[tokio::test]
    async fn password_worker_preserves_hashing_and_verification() {
        let workers = Arc::new(Semaphore::new(1));
        let permit = workers.clone().try_acquire_owned().unwrap();
        let (hash, permit) = password_operation(permit, || hash_password("correct-password"))
            .await
            .unwrap();
        assert!(workers.clone().try_acquire_owned().is_err());
        let ((correct, wrong), permit) = password_operation(permit, move || {
            (
                verify_password("correct-password", &hash),
                verify_password("wrong-password", &hash),
            )
        })
        .await
        .unwrap();
        assert!(correct);
        assert!(!wrong);
        drop(permit);
        assert_eq!(workers.available_permits(), 1);
    }

    static AUTH_HANDLER_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[tokio::test]
    async fn login_ignores_forged_forwarded_addresses() {
        let _test_lock = AUTH_HANDLER_TEST_LOCK.lock().await;
        BRUTE_CACHE.lock().unwrap().remove(&"127.0.0.1".parse().unwrap());
        let (log_tx, _) = tokio::sync::broadcast::channel(16);
        let mut settings = AppSettings::default();
        // Invalid hashes reject passwords without saving sessions or touching router files.
        settings.auth.password_hash = Some("invalid-hash".into());
        let state = AppState {
            core: Arc::new(RwLock::new(crate::types::CoreInfo {
                name: "xray".into(),
                conf_dir: String::new(),
                is_json: true,
            })),
            settings: Arc::new(RwLock::new(settings)),
            init_file: Arc::new(RwLock::new(None)),
            http_client: reqwest::Client::new(),
            update_checker: UpdateChecker::default(),
            geo_cache: Arc::new(RwLock::new(Default::default())),
            log_tx: Arc::new(log_tx),
            log_watcher: Arc::new(std::sync::Mutex::new(Default::default())),
            auth_changes: tokio::sync::watch::channel(0).0,
            app_config_lock: Arc::new(tokio::sync::Mutex::new(())),
            enrollment_lock: Arc::new(tokio::sync::Mutex::new(())),
            debug: false,
            rci_token: Arc::new(RwLock::new(None)),
        };
        let app = Router::new()
            .route("/api/auth/login", post(post_login))
            .route("/api/auth/setup", post(post_setup))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/auth/login", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .unwrap();
        });
        let client = reqwest::Client::new();
        for n in 0..MAX_ATTEMPTS + 2 {
            let response = client
                .post(&url)
                .header("x-real-ip", format!("192.0.2.{}", n + 1))
                .header("x-forwarded-for", format!("198.51.100.{}, 203.0.113.1", n + 1))
                .json(&serde_json::json!({"password": "wrong"}))
                .send()
                .await
                .unwrap();
            let expected = if n < MAX_ATTEMPTS {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            assert_eq!(response.status(), expected);
            assert!(!response.headers().contains_key(header::SET_COOKIE));
        }
        assert_eq!(
            BRUTE_CACHE
                .lock()
                .unwrap()
                .get(&"127.0.0.1".parse().unwrap())
                .unwrap()
                .0,
            MAX_ATTEMPTS
        );
        let ip = "127.0.0.1".parse().unwrap();
        BRUTE_CACHE.lock().unwrap().remove(&ip);
        let mut requests = tokio::task::JoinSet::new();
        for n in 0..32 {
            let client = client.clone();
            let url = url.clone();
            requests.spawn(async move {
                client
                    .post(url)
                    .header("x-real-ip", format!("203.0.113.{}", n + 1))
                    .json(&serde_json::json!({"password": "wrong"}))
                    .send()
                    .await
                    .unwrap()
                    .status()
            });
        }
        let mut checked = 0;
        while let Some(result) = requests.join_next().await {
            match result.unwrap() {
                StatusCode::UNAUTHORIZED => checked += 1,
                StatusCode::TOO_MANY_REQUESTS => (),
                status => panic!("unexpected login status: {status}"),
            }
        }
        assert!(checked > 0 && checked <= MAX_ATTEMPTS);
        assert_eq!(BRUTE_CACHE.lock().unwrap().get(&ip).unwrap().0, checked);
        BRUTE_CACHE.lock().unwrap().remove(&ip);

        // Setup and login share the same worker; busy requests start no hashing or attempt.
        let permit = PASSWORD_WORKERS.clone().try_acquire_owned().unwrap();
        let response = client
            .post(&url)
            .json(&serde_json::json!({"password": "wrong"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!BRUTE_CACHE.lock().unwrap().contains_key(&ip));
        state.settings.write().unwrap().auth.password_hash = None;
        let setup_url = url.replace("/login", "/setup");
        let response = client
            .post(setup_url)
            .json(&serde_json::json!({"password": "new"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(state.settings.read().unwrap().auth.password_hash.is_none());
        assert!(state.settings.read().unwrap().auth.session_ids.is_empty());
        drop(permit);
        server.abort();
        let _ = server.await;
    }

    fn fixture_state() -> AppState {
        let (log_tx, _) = tokio::sync::broadcast::channel(16);
        AppState {
            core: Arc::new(RwLock::new(crate::types::CoreInfo {
                name: "xray".into(),
                conf_dir: String::new(),
                is_json: true,
            })),
            settings: Arc::new(RwLock::new(AppSettings::default())),
            init_file: Arc::new(RwLock::new(None)),
            http_client: reqwest::Client::new(),
            update_checker: UpdateChecker::default(),
            geo_cache: Arc::new(RwLock::new(Default::default())),
            log_tx: Arc::new(log_tx),
            log_watcher: Arc::new(std::sync::Mutex::new(Default::default())),
            auth_changes: tokio::sync::watch::channel(0).0,
            app_config_lock: Arc::new(tokio::sync::Mutex::new(())),
            enrollment_lock: Arc::new(tokio::sync::Mutex::new(())),
            debug: false,
            rci_token: Arc::new(RwLock::new(None)),
        }
    }

    #[tokio::test]
    async fn setup_persists_before_publishing_and_fails_safely_on_io_error() {
        let _test_lock = AUTH_HANDLER_TEST_LOCK.lock().await;
        let state = fixture_state();
        let mut auth_rx = state.auth_changes.subscribe();

        // 1. Failure branch: unwritable path
        let fail_dir = std::env::temp_dir().join(format!("xkeen-auth-setup-fail-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&fail_dir).await.unwrap();
        let blocker = fail_dir.join("blocker");
        tokio::fs::write(&blocker, b"blocker").await.unwrap();
        let bad_config = blocker.join("xkeen-ui.json");
        let bad_conf_dir = blocker.clone();

        TEST_AUTH_CONFIG_OVERRIDE
            .scope((bad_config, bad_conf_dir), async {
                let req = PasswordReq {
                    password: "my-secure-password".into(),
                    remember: false,
                };
                let resp = post_setup(State(state.clone()), HeaderMap::new(), Json(req))
                    .await
                    .into_response();
                assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert!(!resp.headers().contains_key(header::SET_COOKIE));
                assert!(state.settings.read().unwrap().auth.password_hash.is_none());
                assert!(state.settings.read().unwrap().auth.session_ids.is_empty());
                assert_eq!(*auth_rx.borrow_and_update(), 0);
            })
            .await;
        _ = tokio::fs::remove_dir_all(&fail_dir).await;

        // 2. Success branch: writable path
        let ok_dir = std::env::temp_dir().join(format!("xkeen-auth-setup-ok-{}", uuid::Uuid::new_v4()));
        let ok_config = ok_dir.join("xkeen-ui.json");
        let ok_conf_dir = ok_dir.clone();

        TEST_AUTH_CONFIG_OVERRIDE
            .scope((ok_config.clone(), ok_conf_dir), async {
                let req = PasswordReq {
                    password: "my-secure-password".into(),
                    remember: false,
                };
                let resp = post_setup(State(state.clone()), HeaderMap::new(), Json(req))
                    .await
                    .into_response();
                assert_eq!(resp.status(), StatusCode::OK);
                assert!(resp.headers().contains_key(header::SET_COOKIE));
                assert!(state.settings.read().unwrap().auth.password_hash.is_some());
                assert_eq!(state.settings.read().unwrap().auth.session_ids.len(), 1);
                assert_eq!(*auth_rx.borrow_and_update(), 1);

                // Disk must contain the committed auth
                let disk_str = tokio::fs::read_to_string(&ok_config).await.unwrap();
                let disk_json: serde_json::Value = serde_json::from_str(&disk_str).unwrap();
                assert_eq!(
                    disk_json["auth"]["password_hash"],
                    serde_json::to_value(state.settings.read().unwrap().auth.password_hash.clone()).unwrap()
                );
                assert_eq!(disk_json["auth"]["session_ids"].as_array().unwrap().len(), 1);
            })
            .await;
        _ = tokio::fs::remove_dir_all(&ok_dir).await;
    }

    #[tokio::test]
    async fn login_persists_session_and_fails_safely_on_io_error() {
        let _test_lock = AUTH_HANDLER_TEST_LOCK.lock().await;
        let addr: SocketAddr = "192.0.2.200:12345".parse().unwrap();
        BRUTE_CACHE.lock().unwrap().remove(&addr.ip());
        let state = fixture_state();
        let hash = hash_password("login-password");
        state.settings.write().unwrap().auth.password_hash = Some(hash);
        let mut auth_rx = state.auth_changes.subscribe();

        // 1. Failure branch: unwritable path
        let fail_dir = std::env::temp_dir().join(format!("xkeen-auth-login-fail-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&fail_dir).await.unwrap();
        let blocker = fail_dir.join("blocker");
        tokio::fs::write(&blocker, b"blocker").await.unwrap();
        let bad_config = blocker.join("xkeen-ui.json");
        let bad_conf_dir = blocker.clone();

        TEST_AUTH_CONFIG_OVERRIDE
            .scope((bad_config, bad_conf_dir), async {
                let req = PasswordReq {
                    password: "login-password".into(),
                    remember: false,
                };
                let resp = post_login(State(state.clone()), ConnectInfo(addr), HeaderMap::new(), Json(req)).await;
                assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert!(!resp.headers().contains_key(header::SET_COOKIE));
                assert!(state.settings.read().unwrap().auth.session_ids.is_empty());
                assert_eq!(*auth_rx.borrow_and_update(), 0);
            })
            .await;
        _ = tokio::fs::remove_dir_all(&fail_dir).await;

        // 2. Success branch: writable path
        let ok_dir = std::env::temp_dir().join(format!("xkeen-auth-login-ok-{}", uuid::Uuid::new_v4()));
        let ok_config = ok_dir.join("xkeen-ui.json");
        let ok_conf_dir = ok_dir.clone();

        TEST_AUTH_CONFIG_OVERRIDE
            .scope((ok_config.clone(), ok_conf_dir), async {
                let req = PasswordReq {
                    password: "login-password".into(),
                    remember: false,
                };
                let resp = post_login(State(state.clone()), ConnectInfo(addr), HeaderMap::new(), Json(req)).await;
                assert_eq!(resp.status(), StatusCode::OK);
                assert!(resp.headers().contains_key(header::SET_COOKIE));
                assert_eq!(state.settings.read().unwrap().auth.session_ids.len(), 1);
                assert_eq!(*auth_rx.borrow_and_update(), 1);

                let disk_str = tokio::fs::read_to_string(&ok_config).await.unwrap();
                let disk_json: serde_json::Value = serde_json::from_str(&disk_str).unwrap();
                assert_eq!(disk_json["auth"]["session_ids"].as_array().unwrap().len(), 1);
            })
            .await;
        _ = tokio::fs::remove_dir_all(&ok_dir).await;
    }

    #[tokio::test]
    async fn reset_persists_and_preserves_hash_on_io_error() {
        let state = fixture_state();
        let hash = "existing-hash".to_string();
        state.settings.write().unwrap().auth.password_hash = Some(hash.clone());
        state.settings.write().unwrap().auth.session_ids = vec!["sess-1".into(), "sess-2".into()];
        let mut auth_rx = state.auth_changes.subscribe();

        // 1. Failure branch: unwritable path
        let fail_dir = std::env::temp_dir().join(format!("xkeen-auth-reset-fail-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&fail_dir).await.unwrap();
        let blocker = fail_dir.join("blocker");
        tokio::fs::write(&blocker, b"blocker").await.unwrap();
        let bad_config = blocker.join("xkeen-ui.json");
        let bad_conf_dir = blocker.clone();

        TEST_AUTH_CONFIG_OVERRIDE
            .scope((bad_config, bad_conf_dir), async {
                let resp = post_auth_reset(State(state.clone())).await.into_response();
                assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(state.settings.read().unwrap().auth.password_hash, Some(hash.clone()));
                assert_eq!(state.settings.read().unwrap().auth.session_ids.len(), 2);
                assert_eq!(*auth_rx.borrow_and_update(), 0);
            })
            .await;
        _ = tokio::fs::remove_dir_all(&fail_dir).await;

        // 2. Success branch: writable path
        let ok_dir = std::env::temp_dir().join(format!("xkeen-auth-reset-ok-{}", uuid::Uuid::new_v4()));
        let ok_config = ok_dir.join("xkeen-ui.json");
        let ok_conf_dir = ok_dir.clone();

        TEST_AUTH_CONFIG_OVERRIDE
            .scope((ok_config.clone(), ok_conf_dir), async {
                let resp = post_auth_reset(State(state.clone())).await.into_response();
                assert_eq!(resp.status(), StatusCode::OK);
                assert!(state.settings.read().unwrap().auth.password_hash.is_none());
                assert!(state.settings.read().unwrap().auth.session_ids.is_empty());
                assert_eq!(*auth_rx.borrow_and_update(), 1);

                let disk_str = tokio::fs::read_to_string(&ok_config).await.unwrap();
                let disk_json: serde_json::Value = serde_json::from_str(&disk_str).unwrap();
                assert!(disk_json["auth"]["password_hash"].is_null());
                assert!(disk_json["auth"]["session_ids"].as_array().unwrap().is_empty());
            })
            .await;
        _ = tokio::fs::remove_dir_all(&ok_dir).await;
    }

    #[tokio::test]
    async fn logout_persists_and_preserves_session_on_io_error() {
        let state = fixture_state();
        let active_sess = format!("active-uid:{}", now_ts() + 3600);
        let other_sess = format!("other-uid:{}", now_ts() + 3600);
        state.settings.write().unwrap().auth.session_ids = vec![active_sess.clone(), other_sess.clone()];
        let mut auth_rx = state.auth_changes.subscribe();

        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("session_id=active-uid"));

        // 1. Failure branch: unwritable path
        let fail_dir = std::env::temp_dir().join(format!("xkeen-auth-logout-fail-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&fail_dir).await.unwrap();
        let blocker = fail_dir.join("blocker");
        tokio::fs::write(&blocker, b"blocker").await.unwrap();
        let bad_config = blocker.join("xkeen-ui.json");
        let bad_conf_dir = blocker.clone();

        TEST_AUTH_CONFIG_OVERRIDE
            .scope((bad_config, bad_conf_dir), async {
                let resp = post_logout(State(state.clone()), headers.clone()).await.into_response();
                assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(state.settings.read().unwrap().auth.session_ids.len(), 2);
                assert_eq!(*auth_rx.borrow_and_update(), 0);
            })
            .await;
        _ = tokio::fs::remove_dir_all(&fail_dir).await;

        // 2. Success branch: writable path
        let ok_dir = std::env::temp_dir().join(format!("xkeen-auth-logout-ok-{}", uuid::Uuid::new_v4()));
        let ok_config = ok_dir.join("xkeen-ui.json");
        let ok_conf_dir = ok_dir.clone();

        TEST_AUTH_CONFIG_OVERRIDE
            .scope((ok_config.clone(), ok_conf_dir), async {
                let resp = post_logout(State(state.clone()), headers.clone()).await.into_response();
                assert_eq!(resp.status(), StatusCode::OK);
                assert_eq!(state.settings.read().unwrap().auth.session_ids, vec![other_sess.clone()]);
                assert_eq!(*auth_rx.borrow_and_update(), 1);

                let disk_str = tokio::fs::read_to_string(&ok_config).await.unwrap();
                let disk_json: serde_json::Value = serde_json::from_str(&disk_str).unwrap();
                assert_eq!(
                    disk_json["auth"]["session_ids"].as_array().unwrap(),
                    &[serde_json::Value::String(other_sess.clone())]
                );
            })
            .await;
        _ = tokio::fs::remove_dir_all(&ok_dir).await;
    }

    #[tokio::test]
    async fn concurrent_setup_requests_elect_single_winner_and_prevent_duplicate_sessions() {
        let _test_lock = AUTH_HANDLER_TEST_LOCK.lock().await;
        let state = fixture_state();
        let mut auth_rx = state.auth_changes.subscribe();

        let dir = std::env::temp_dir().join(format!("xkeen-auth-setup-race-{}", uuid::Uuid::new_v4()));
        let config_path = dir.join("xkeen-ui.json");
        let conf_dir = dir.clone();

        TEST_AUTH_CONFIG_OVERRIDE
            .scope((config_path.clone(), conf_dir), async {
                let req1 = PasswordReq {
                    password: "password-one".into(),
                    remember: false,
                };
                let req2 = PasswordReq {
                    password: "password-two".into(),
                    remember: false,
                };

                let state1 = state.clone();
                let state2 = state.clone();

                // Fire two setup requests concurrently
                let (resp1, resp2) = tokio::join!(
                    post_setup(State(state1), HeaderMap::new(), Json(req1)),
                    post_setup(State(state2), HeaderMap::new(), Json(req2))
                );

                let r1 = resp1.into_response();
                let r2 = resp2.into_response();

                // Exactly one request must succeed with 200 OK and cookie, the other must fail with 403 FORBIDDEN and no cookie
                let (winner_resp, loser_resp) = if r1.status() == StatusCode::OK {
                    (r1, r2)
                } else {
                    (r2, r1)
                };

                assert_eq!(winner_resp.status(), StatusCode::OK);
                assert!(winner_resp.headers().contains_key(header::SET_COOKIE));

                assert_eq!(loser_resp.status(), StatusCode::FORBIDDEN);
                assert!(!loser_resp.headers().contains_key(header::SET_COOKIE));

                // Settings must have password_hash set and exactly ONE session ID
                let auth = state.settings.read().unwrap().auth.clone();
                assert!(auth.password_hash.is_some());
                assert_eq!(auth.session_ids.len(), 1);

                // Check winner cookie matches the single session ID
                let cookie_header = winner_resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
                let session_id = cookie_header
                    .split(';')
                    .find_map(|s| s.trim().strip_prefix("session_id="))
                    .unwrap();
                assert!(auth.session_ids[0].starts_with(session_id));

                // Exactly 1 update notification emitted
                assert_eq!(*auth_rx.borrow_and_update(), 1);

                // Disk configuration must match in-memory state and have exactly 1 session ID
                let disk_str = tokio::fs::read_to_string(&config_path).await.unwrap();
                let disk_json: serde_json::Value = serde_json::from_str(&disk_str).unwrap();
                assert_eq!(disk_json["auth"]["session_ids"].as_array().unwrap().len(), 1);
                assert_eq!(
                    disk_json["auth"]["password_hash"],
                    serde_json::to_value(auth.password_hash.clone()).unwrap()
                );
            })
            .await;
        _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn setup_commit_level_race_rejection() {
        let state = fixture_state();
        let dir = std::env::temp_dir().join(format!("xkeen-auth-commit-race-{}", uuid::Uuid::new_v4()));
        let config_path = dir.join("xkeen-ui.json");
        let conf_dir = dir.clone();

        let hash1 = hash_password("first-pass");
        let hash2 = hash_password("second-pass");
        let sess1 = format!("{}:{}", Uuid::new_v4(), now_ts() + 86400);
        let sess2 = format!("{}:{}", Uuid::new_v4(), now_ts() + 86400);

        // First commit succeeds
        let res1 = commit_setup_at(&state, &config_path, &conf_dir, hash1.clone(), sess1.clone()).await;
        assert!(res1.is_ok());
        assert_eq!(state.settings.read().unwrap().auth.password_hash, Some(hash1.clone()));
        assert_eq!(state.settings.read().unwrap().auth.session_ids, vec![sess1.clone()]);

        // Second commit MUST fail with AlreadySet and NOT modify password or append session
        let res2 = commit_setup_at(&state, &config_path, &conf_dir, hash2.clone(), sess2.clone()).await;
        match res2 {
            Err(AuthSetupError::AlreadySet) => {}
            other => panic!("Expected AlreadySet, got {:?}", other),
        }

        // Verify state is untouched by second commit
        assert_eq!(state.settings.read().unwrap().auth.password_hash, Some(hash1.clone()));
        assert_eq!(state.settings.read().unwrap().auth.session_ids, vec![sess1.clone()]);

        // Disk is untouched by second commit
        let disk_str = tokio::fs::read_to_string(&config_path).await.unwrap();
        let disk_json: serde_json::Value = serde_json::from_str(&disk_str).unwrap();
        assert_eq!(disk_json["auth"]["password_hash"].as_str().unwrap(), hash1);
        assert_eq!(disk_json["auth"]["session_ids"].as_array().unwrap().len(), 1);

        _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn setup_serialized_and_recovers_after_transient_failure() {
        let _test_lock = AUTH_HANDLER_TEST_LOCK.lock().await;
        let state = fixture_state();

        let fail_dir = std::env::temp_dir().join(format!("xkeen-auth-setup-recover-fail-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&fail_dir).await.unwrap();
        let blocker = fail_dir.join("blocker");
        tokio::fs::write(&blocker, b"blocker").await.unwrap();
        let bad_config = blocker.join("xkeen-ui.json");
        let bad_conf_dir = blocker.clone();

        let ok_dir = std::env::temp_dir().join(format!("xkeen-auth-setup-recover-ok-{}", uuid::Uuid::new_v4()));
        let ok_config = ok_dir.join("xkeen-ui.json");
        let ok_conf_dir = ok_dir.clone();

        // 1. First setup fails due to I/O error
        TEST_AUTH_CONFIG_OVERRIDE
            .scope((bad_config, bad_conf_dir), async {
                let req = PasswordReq {
                    password: "attempt-one".into(),
                    remember: false,
                };
                let resp = post_setup(State(state.clone()), HeaderMap::new(), Json(req))
                    .await
                    .into_response();
                assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert!(state.settings.read().unwrap().auth.password_hash.is_none());
            })
            .await;
        _ = tokio::fs::remove_dir_all(&fail_dir).await;

        // 2. Second setup can now proceed under enrollment lock and succeeds
        TEST_AUTH_CONFIG_OVERRIDE
            .scope((ok_config.clone(), ok_conf_dir), async {
                let req = PasswordReq {
                    password: "attempt-two".into(),
                    remember: false,
                };
                let resp = post_setup(State(state.clone()), HeaderMap::new(), Json(req))
                    .await
                    .into_response();
                assert_eq!(resp.status(), StatusCode::OK);
                assert!(resp.headers().contains_key(header::SET_COOKIE));
                assert!(state.settings.read().unwrap().auth.password_hash.is_some());
                assert_eq!(state.settings.read().unwrap().auth.session_ids.len(), 1);
            })
            .await;
        _ = tokio::fs::remove_dir_all(&ok_dir).await;
    }

    #[tokio::test]
    async fn setup_fast_rejection_when_password_already_set() {
        let state = fixture_state();
        state.settings.write().unwrap().auth.password_hash = Some(hash_password("existing"));

        let req = PasswordReq {
            password: "new-password".into(),
            remember: false,
        };
        let resp = post_setup(State(state.clone()), HeaderMap::new(), Json(req))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(!resp.headers().contains_key(header::SET_COOKIE));
    }
}
