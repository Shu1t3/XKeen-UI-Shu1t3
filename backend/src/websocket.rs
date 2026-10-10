use crate::logger::process_log_line;
use crate::types::*;
use axum::extract::State;
use axum::Extension;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use notify::{RecursiveMode, Watcher};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Count and watcher ownership change under one lock on every exit, including cancellation.
struct LogClient {
    lifecycle: Arc<Mutex<LogWatcherState>>,
}

impl LogClient {
    fn acquire(state: &AppState) -> Self {
        let mut lifecycle = state.log_watcher.lock().unwrap();
        lifecycle.clients += 1;
        if state.debug { println!("{} [INFO] WS Connected (Total: {})", crate::logger::ts(), lifecycle.clients); }
        if lifecycle.handle.as_ref().is_none_or(|h| h.is_finished()) {
            lifecycle.handle = Some(spawn_log_watcher(state));
        }
        Self { lifecycle: state.log_watcher.clone() }
    }
}

impl Drop for LogClient {
    fn drop(&mut self) {
        let mut lifecycle = self.lifecycle.lock().unwrap();
        lifecycle.clients -= 1;
        if lifecycle.clients == 0
            && let Some(handle) = lifecycle.handle.take() { handle.abort(); }
    }
}

fn spawn_log_watcher(state: &AppState) -> tokio::task::AbortHandle {
    let tx = state.log_tx.clone();
    tokio::spawn(async move {
        let (mpsc_tx, mut mpsc_rx) = tokio::sync::mpsc::channel::<String>(32);
        let mut watcher = match notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            if let Ok(e) = res
                && e.kind.is_modify() {
                for path in e.paths {
                    let _ = mpsc_tx.try_send(path.to_string_lossy().to_string());
                }
            }
        }) {
            Ok(watcher) => watcher,
            Err(e) => { crate::logger::log("ERROR", format!("Log watcher: {e}")); return; }
        };
        let _ = watcher.watch(Path::new(&error_log_path()), RecursiveMode::NonRecursive);
        let _ = watcher.watch(Path::new(&access_log_path()), RecursiveMode::NonRecursive);
        while let Some(path) = mpsc_rx.recv().await { let _ = tx.send(path); }
    }).abort_handle()
}

pub async fn ws_handler(
    ws: WebSocketUpgrade, State(state): State<AppState>, Extension(mut session): Extension<crate::auth::WsSession>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let work_session = session.clone();
        tokio::select! {
            biased;
            _ = session.revoked() => {},
            _ = handle_socket(socket, state, work_session) => {},
        }
    })
}

fn read_log_file(p: String, offset: u64, query: String, full: bool, tz: i32) -> (String, Vec<String>, u64) {
    let mut f = match File::open(&p) {
        Ok(f) => f,
        _ => return ("clear".into(), vec![], 0),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut current_pos = offset;

    if full && query.is_empty() && len > 128000 {
        let seek_pos = len - 128000;
        _ = f.seek(SeekFrom::Start(seek_pos));
        let mut reader = BufReader::new(&mut f);
        let mut discard = String::new();
        let skipped = reader.read_line(&mut discard).unwrap_or(0);
        current_pos = seek_pos + skipped as u64;
    } else if !full && len < offset {
        return ("clear".into(), vec![], 0);
    } else if full {
        current_pos = 0;
    }

    _ = f.seek(SeekFrom::Start(current_pos));
    let mut lines = Vec::new();
    let mut total_bytes = 0usize;
    let mut bytes_read = current_pos;
    let keywords: Vec<String> = query
        .split('|')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let reader = BufReader::new(f);

    for line in reader.lines().map_while(Result::ok) {
        let line_len = line.len() + 1;
        bytes_read += line_len as u64;

        let matched = if query.is_empty() {
            true
        } else {
            let normalized = line
                .replace("[Debug]", "[DEBUG]")
                .replace("level=debug", "level=DEBUG")
                .replace("[Info]", "[INFO]")
                .replace("level=info", "level=INFO")
                .replace("[Warning]", "[WARN]")
                .replace("level=warning", "level=WARN")
                .replace("[Error]", "[ERROR]")
                .replace("level=error", "level=ERROR")
                .replace("[Fatal]", "[FATAL]")
                .replace("level=fatal", "level=FATAL");
            keywords.iter().any(|k| normalized.contains(k))
        };

        if matched {
            let proc = process_log_line(line, tz);
            if !proc.is_empty() {
                if full && !query.is_empty() {
                    total_bytes += line_len;
                    if total_bytes >= 128000 {
                        break;
                    }
                }
                lines.push(proc);
            }
        }
    }
    (if full { "initial".into() } else { "append".into() }, lines, bytes_read)
}

async fn handle_socket(socket: WebSocket, state: AppState, session: crate::auth::WsSession) {
    if !session.authorized() { return; }
    let _client = LogClient::acquire(&state);
    let (mut tx, mut rx) = socket.split();
    let mut log_rx = state.log_tx.subscribe();
    let mut path = error_log_path();
    let mut query = String::new();
    let tz = state.settings.read().unwrap().log.timezone;

    let p_clone = path.clone();
    let q_clone = query.clone();
    let (t, l, mut offset) = tokio::task::spawn_blocking(move || read_log_file(p_clone, 0, q_clone, true, tz))
        .await
        .unwrap();

    let init_msg = if l.is_empty() {
        serde_json::json!({"type": "clear"})
    } else {
        serde_json::json!({"type": t, "lines": l})
    };
    if !session.authorized() || tx.send(Message::Text(init_msg.to_string().into())).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            msg = rx.next() => {
                if !session.authorized() { break; }
                let Some(msg) = msg else { break; };
                match msg {
                    Ok(Message::Text(txt)) => {
                        let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or_default();
                        let tz = state.settings.read().unwrap().log.timezone;
                        match v["type"].as_str() {
                            Some("switchFile") => {
                                path = if v["file"] == "access.log" { access_log_path() } else { error_log_path() };

                                let p = path.clone();
                                let q = query.clone();
                                let (t, l, off) = tokio::task::spawn_blocking(move || read_log_file(p, 0, q, true, tz)).await.unwrap();
                                offset = off;

                                let msg = if l.is_empty() { serde_json::json!({"type": "clear"}) } else { serde_json::json!({"type": t, "lines": l}) };
                                if tx.send(Message::Text(msg.to_string().into())).await.is_err() { break; }
                            },
                            Some("filter") | Some("reload") => {
                                query = v["query"].as_str().unwrap_or("").to_string();
                                let q = query.clone();
                                let p = path.clone();
                                let (t, l, off) = tokio::task::spawn_blocking(move || read_log_file(p, 0, q, true, tz)).await.unwrap();
                                offset = off;

                                let msg = if l.is_empty() { serde_json::json!({"type": "clear"}) } else { serde_json::json!({"type": if v["type"] == "filter" { "filtered" } else { t.as_str() }, "lines": l}) };
                                if tx.send(Message::Text(msg.to_string().into())).await.is_err() { break; }
                            },
                            Some("clear") => {
                                let p = path.clone();
                                offset = 0;
                                let clear_session = session.clone();
                                tokio::task::spawn_blocking(move || {
                                    clear_session.if_authorized(|| { File::create(p).ok(); });
                                }).await.ok();
                                if tx.send(Message::Text(serde_json::json!({"type": "clear"}).to_string().into())).await.is_err() { break; }
                            },
                            _ => {}
                        }
                    },
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            result = log_rx.recv() => {
                if !session.authorized() { break; }
                let changed_path = match result {
                    Ok(p) => p,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => path.clone(),
                    Err(_) => break,
                };
                if changed_path == path {
                    let tz = state.settings.read().unwrap().log.timezone;
                    let p = path.clone();
                    let q = query.clone();
                    let off_curr = offset;
                    let (t, l, new_off) = tokio::task::spawn_blocking(move || read_log_file(p, off_curr, q, false, tz)).await.unwrap();
                    offset = new_off;

                    if !l.is_empty() {
                        let content = l.join("\n");
                        if tx.send(Message::Text(serde_json::json!({"type": t, "content": content}).to_string().into())).await.is_err() { break; }
                    } else if t == "clear"
                         && tx.send(Message::Text(serde_json::json!({"type": "clear"}).to_string().into())).await.is_err() { break; }
                }
            }
        }
    }


}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture() -> AppState {
        let (log_tx, _) = tokio::sync::broadcast::channel(16);
        let mut settings = AppSettings::default();
        settings.auth.enabled = false;
        AppState {
            core: Arc::new(std::sync::RwLock::new(CoreInfo { name: "mihomo".into(), conf_dir: String::new(), is_json: false })),
            settings: Arc::new(std::sync::RwLock::new(settings)),
            init_file: Arc::new(std::sync::RwLock::new(None)),
            http_client: reqwest::Client::new(), update_checker: UpdateChecker::default(),
            geo_cache: Arc::new(std::sync::RwLock::new(Default::default())),
            log_tx: Arc::new(log_tx), log_watcher: Arc::new(Mutex::new(Default::default())),
            auth_changes: tokio::sync::watch::channel(0).0,
            app_config_lock: Arc::new(tokio::sync::Mutex::new(())),
            enrollment_lock: Arc::new(tokio::sync::Mutex::new(())),
            debug: false,
            rci_token: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    #[tokio::test]
    async fn session_guard_rejects_commands_after_revoke_even_when_auth_is_disabled() {
        let state = fixture();
        state.settings.write().unwrap().auth.session_ids = vec!["legacy-session".into()];
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("cookie", "session_id=legacy-session".parse().unwrap());
        let mut session = crate::auth::WsSession::from_state(&state, &headers);
        assert!(session.authorized());
        let path = std::env::temp_dir().join(format!("xkeen-revoked-clear-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, "keep logs").unwrap();
        state.settings.write().unwrap().auth.session_ids.clear();
        state.auth_changes.send_modify(|v| *v += 1);
        tokio::time::timeout(std::time::Duration::from_secs(1), session.revoked()).await.unwrap();
        assert!(session.if_authorized(|| File::create(&path).unwrap()).is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep logs");
        std::fs::remove_file(path).unwrap();
        // Anonymous access while auth is off is closed when authentication is enabled.
        let mut anonymous = crate::auth::WsSession::from_state(&state, &axum::http::HeaderMap::new());
        assert!(anonymous.authorized());
        state.settings.write().unwrap().auth.enabled = true;
        state.auth_changes.send_modify(|v| *v += 1);
        tokio::time::timeout(std::time::Duration::from_secs(1), anonymous.revoked()).await.unwrap();
        assert!(!anonymous.authorized());
    }

    #[tokio::test]
    async fn watcher_is_released_on_early_return_and_cancelled_task() {
        let state = fixture();
        let early = |state: &AppState| -> Result<(), ()> {
            let _client = LogClient::acquire(state);
            Err(()) // Same drop path as failed initial send, with no awaited cleanup.
        };
        assert!(early(&state).is_err());
        assert_eq!(state.log_watcher.lock().unwrap().clients, 0);
        assert!(state.log_watcher.lock().unwrap().handle.is_none());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let _client = LogClient::acquire(&task_state);
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        assert_eq!(state.log_watcher.lock().unwrap().clients, 1);
        task.abort();
        let _ = task.await;
        assert_eq!(state.log_watcher.lock().unwrap().clients, 0);
        assert!(state.log_watcher.lock().unwrap().handle.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overlapping_connect_disconnect_keeps_new_clients_watcher() {
        let state = fixture();
        for _ in 0..100 {
            let old = LogClient::acquire(&state);
            let task_state = state.clone();
            let new = tokio::spawn(async move { LogClient::acquire(&task_state) });
            drop(old);
            let new = new.await.unwrap();
            let lifecycle = state.log_watcher.lock().unwrap();
            assert_eq!(lifecycle.clients, 1);
            assert!(!lifecycle.handle.as_ref().unwrap().is_finished());
            drop(lifecycle);
            drop(new);
            assert!(state.log_watcher.lock().unwrap().handle.is_none());
        }
    }

    #[tokio::test]
    async fn log_sockets_close_on_logout_reset_and_expiration() {
        use axum::{Router, middleware, routing::{get, post}};
        use tokio_tungstenite::{connect_async, tungstenite::{client::IntoClientRequest, Message as WireMessage}};
        for mode in ["logout", "reset", "expiration"] {
            let state = fixture();
            {
                let mut settings = state.settings.write().unwrap();
                settings.auth.enabled = true;
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                settings.auth.session_ids = vec![format!("victim:{}", now + if mode == "expiration" { 2 } else { 60 }), format!("other:{}", now + 60)];
            }
            let auth_state = state.clone();
            let app = Router::new().route("/ws", get(ws_handler))
                .route("/logout", post(crate::auth::post_logout)).route("/reset", post(crate::auth::post_auth_reset))
                .route_layer(middleware::from_fn(move |req, next| {
                    let state = auth_state.clone();
                    async move { crate::auth::auth_middleware(state, req, next).await }
                })).with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
            request.headers_mut().insert("cookie", "session_id=victim".parse().unwrap());
            let (mut victim, _) = connect_async(request).await.unwrap();
            victim.next().await.unwrap().unwrap(); // Initial data proves the admitted socket is live.
            let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
            request.headers_mut().insert("cookie", "session_id=other".parse().unwrap());
            let (mut other, _) = connect_async(request).await.unwrap();
            other.next().await.unwrap().unwrap();
            // Use isolated temp config so the real endpoint persists and revokes without touching router files.
            let temp_dir = std::env::temp_dir().join(format!("xkeen-ws-test-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&temp_dir).await.unwrap();
            let config_path = temp_dir.join("xkeen-ui.json");
            let conf_dir = temp_dir.clone();
            let revoke = if mode != "expiration" {
                let mut changed = state.auth_changes.subscribe();
                let task_state = state.clone();
                let task_config = config_path.clone();
                let task_dir = conf_dir.clone();
                let task = tokio::spawn(async move {
                    crate::auth::TEST_AUTH_CONFIG_OVERRIDE
                        .scope((task_config, task_dir), async move {
                            if mode == "logout" {
                                let mut headers = axum::http::HeaderMap::new();
                                headers.insert("cookie", "session_id=victim".parse().unwrap());
                                let _ = crate::auth::post_logout(State(task_state), headers).await;
                            } else {
                                let _ = crate::auth::post_auth_reset(State(task_state)).await;
                            }
                        })
                        .await;
                });
                tokio::time::timeout(std::time::Duration::from_secs(2), changed.changed()).await.unwrap().unwrap();
                Some(task)
            } else { None };
            let closed = tokio::time::timeout(std::time::Duration::from_secs(4), victim.next()).await.unwrap();
            assert!(!matches!(closed, Some(Ok(WireMessage::Text(_)))), "{mode}");
            if mode == "reset" {
                let closed = tokio::time::timeout(std::time::Duration::from_secs(2), other.next()).await.unwrap();
                assert!(!matches!(closed, Some(Ok(WireMessage::Text(_)))));
            } else {
                other.send(WireMessage::Text(r#"{"type":"reload"}"#.into())).await.unwrap();
                assert!(matches!(tokio::time::timeout(std::time::Duration::from_secs(2), other.next()).await.unwrap(), Some(Ok(WireMessage::Text(_)))));
            }
            if let Some(task) = revoke { let _ = task.await; }
            server.abort(); let _ = server.await;
            _ = tokio::fs::remove_dir_all(&temp_dir).await;
            drop(victim); drop(other);
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if state.log_watcher.lock().unwrap().clients == 0 { break; }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            assert!(state.log_watcher.lock().unwrap().handle.is_none());
        }
    }

    #[test]
    fn initial_filtered_and_appended_logs_escape_stored_payloads() {
        let path = std::env::temp_dir().join(format!("xkeen-log-xss-{}.log", uuid::Uuid::new_v4()));
        let name = path.to_string_lossy().into_owned();
        let payload = "[ERROR] upstream: <img src=x onerror=alert(1)>\n";
        std::fs::write(&path, payload).unwrap();
        let (kind, initial, offset) = read_log_file(name.clone(), 0, String::new(), true, 0);
        assert_eq!(kind, "initial");
        assert_eq!(initial.len(), 1);
        assert!(initial[0].contains("&lt;img src=x onerror=alert(1)&gt;"));
        let (_, filtered, _) = read_log_file(name.clone(), 0, "ERROR".into(), true, 0);
        assert_eq!(filtered, initial);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "[WARN] validator: </div><svg onload=alert(1)>").unwrap();
        let (kind, appended, _) = read_log_file(name, offset, String::new(), false, 0);
        assert_eq!(kind, "append");
        assert_eq!(appended.len(), 1);
        assert!(appended[0].contains("&lt;/div&gt;&lt;svg onload=alert(1)&gt;"));
        let wire = serde_json::json!({"type": kind, "content": appended.join("\n")});
        assert!(!wire["content"].as_str().unwrap().contains("<svg"));
        std::fs::remove_file(path).unwrap();
    }
}
