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

use crate::types::{APP_CONFIG, ApiResponse, AppState};

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

    update_auth(&state, |auth| {
        auth.password_hash = Some(hash);
        auth.session_ids.push(session_val);
    })
    .await;

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

    update_auth(&state, |auth| {
        auth.session_ids.push(session_val);
    })
    .await;

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
        update_auth(&state, |auth| {
            auth.session_ids.retain(|id| id.split_once(':').map_or(id.as_str(), |(uid, _)| uid) != cookie)
        })
        .await;
    }
    (
        clear_cookie_header(),
        Json(ApiResponse::<()> {
            success: true,
            error: None,
            data: None,
        }),
    )
}

pub async fn post_auth_reset(State(state): State<AppState>) -> impl IntoResponse {
    update_auth(&state, |auth| {
        auth.password_hash = None;
        auth.session_ids.clear();
    })
    .await;
    (
        clear_cookie_header(),
        Json(ApiResponse::<()> {
            success: true,
            error: None,
            data: None,
        }),
    )
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

async fn update_auth(state: &AppState, modify: impl FnOnce(&mut crate::types::AuthSettings)) {
    {
        let mut s = state.settings.write().unwrap();
        modify(&mut s.auth);

        let ts = now_ts();
        s.auth.session_ids.retain(|id| {
            if let Some((_, exp)) = id.split_once(':') {
                exp.parse::<u64>().unwrap_or(0) > ts
            } else {
                true
            }
        });
    }
    state.auth_changes.send_modify(|version| *version = version.wrapping_add(1));
    save_auth_to_config(state).await;
}

async fn save_auth_to_config(state: &AppState) {
    let _guard = state.app_config_lock.lock().await;
    let auth = state.settings.read().unwrap().auth.clone();
    let mut file_json: serde_json::Value = tokio::fs::read_to_string(APP_CONFIG)
        .await
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or(serde_json::json!({}));
    file_json["auth"] = serde_json::to_value(auth).unwrap();
    let serialized = serde_json::to_string_pretty(&file_json).unwrap();
    let tmp = format!("{}.tmp", APP_CONFIG);
    if tokio::fs::write(&tmp, &serialized).await.is_ok() {
        let _ = tokio::fs::rename(&tmp, APP_CONFIG).await;
    }
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

    #[tokio::test]
    async fn login_ignores_forged_forwarded_addresses() {
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
}
