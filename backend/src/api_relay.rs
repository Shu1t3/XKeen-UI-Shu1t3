use axum::body::{Body, to_bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, MatchedPath, Query, State};
use axum::http::{HeaderMap, HeaderName, Request, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::Deserialize;
use std::path::Path as FsPath;
use std::pin::Pin;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{Error as TError, Message as TMessage};
use tokio_tungstenite::{client_async, connect_async};

use crate::logger::log;
use crate::types::{ApiResponse, AppState, MIHOMO_CONF_DIR};

#[derive(Clone)]
pub(crate) enum ClashTarget {
    Tcp { port: u16, secret: Option<String> },
    Unix { path: String },
}

#[derive(Deserialize)]
pub struct ClashWsQuery {
    pub port: Option<String>,
    pub secret: Option<String>,
    pub unix: Option<String>,
}

pub async fn get_device_list(State(state): State<AppState>) -> impl IntoResponse {
    let mut req = state
        .http_client
        .get("http://127.0.0.1:79/rci/show/device-list")
        .timeout(Duration::from_secs(5));
    let token = state.rci_token.read().unwrap().clone();
    if let Some(ref token) = token {
        req = req.header("X-Ndma-Tkn", token);
    }
    let response = match req.send().await {
        Ok(response) => response,
        Err(e) => return Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    };

    if !response.status().is_success() {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("Ошибка RCI: {}", response.status()),
        }));
    }

    match response.json::<serde_json::Value>().await {
        Ok(data) => Json(serde_json::json!({
            "success": true,
            "host": data.get("host").cloned().unwrap_or_else(|| serde_json::json!([])),
        })),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
}

fn raw_relay_path(matched_path: &str, request_path: &str) -> String {
    let prefix_len = matched_path.split("{*").next().unwrap_or("").len();
    request_path[prefix_len.min(request_path.len())..]
        .trim_start_matches('/')
        .to_string()
}

pub async fn proxy_http(matched_path: MatchedPath, req: Request<Body>) -> Response {
    let path = raw_relay_path(matched_path.as_str(), req.uri().path());
    let (parts, body) = req.into_parts();
    let port_override = header_value(&parts.headers, "x-clash-port");
    let secret_override = header_value(&parts.headers, "x-clash-secret");
    let unix_override = header_value(&parts.headers, "x-clash-unix");

    let target = match resolve_clash_target(port_override, secret_override, unix_override).await {
        Ok(t) => t,
        Err(e) => return make_error(StatusCode::BAD_REQUEST, e),
    };

    const CLASH_BODY_LIMIT: usize = 16 * 1024 * 1024;
    let body_bytes = match to_bytes(body, CLASH_BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => return make_error(StatusCode::BAD_GATEWAY, e.to_string()),
    };

    match target {
        ClashTarget::Tcp { port, secret } => {
            let url = build_url("http", port, &path, parts.uri.query());
            let client = match relay_http_client(None) {
                Ok(client) => client,
                Err(e) => return make_error(StatusCode::BAD_GATEWAY, e),
            };
            do_proxy_http(client, parts, body_bytes, url, secret).await
        }
        ClashTarget::Unix { path: socket_path } => {
            let url = build_url("http", 80, &path, parts.uri.query());
            let client = match relay_http_client(Some(&socket_path)) {
                Ok(c) => c,
                Err(e) => return make_error(StatusCode::BAD_GATEWAY, e),
            };
            do_proxy_http(client, parts, body_bytes, url, None).await
        }
    }
}

pub async fn proxy_ws(
    matched_path: MatchedPath, Query(q): Query<ClashWsQuery>, Extension(session): Extension<crate::auth::WsSession>,
    ws: WebSocketUpgrade, req: Request<Body>,
) -> impl IntoResponse {
    if !session.authorized() {
        return make_error(StatusCode::UNAUTHORIZED, "Сессия завершена".into());
    }
    let path = raw_relay_path(matched_path.as_str(), req.uri().path());
    let target = match resolve_clash_target(q.port, q.secret, q.unix).await {
        Ok(t) => t,
        Err(e) => return make_error(StatusCode::BAD_REQUEST, e),
    };

    ws.on_upgrade(move |socket| async move {
        if let Err(e) = proxy_ws_authorized(socket, path, target, session).await {
            eprintln!("Clash WS proxy error: {}", e);
        }
    })
}

async fn proxy_ws_authorized(
    socket: WebSocket, path: String, target: ClashTarget, mut session: crate::auth::WsSession,
) -> Result<(), String> {
    if !session.authorized() {
        return Ok(());
    }
    let forwarding_session = session.clone();
    tokio::select! {
        biased;
        _ = session.revoked() => Ok(()),
        result = proxy_ws_inner(socket, path, target, forwarding_session) => result,
    }
}

async fn proxy_ws_inner(
    client_ws: WebSocket, path: String, target: ClashTarget, session: crate::auth::WsSession,
) -> Result<(), String> {
    type UpstreamSink = Pin<Box<dyn Sink<TMessage, Error = TError> + Send>>;
    type UpstreamStream = Pin<Box<dyn Stream<Item = Result<TMessage, TError>> + Send>>;

    let (mut upstream_tx, mut upstream_rx): (UpstreamSink, UpstreamStream) = match target {
        ClashTarget::Tcp { port, secret } => {
            let mut url = build_url("ws", port, &path, None);
            if let Some(secret) = secret {
                url.query_pairs_mut().append_pair("token", &secret);
            }
            let (ws, _) = timeout(Duration::from_secs(5), connect_async(url.as_str()))
                .await
                .map_err(|_| "Upstream connect timeout".to_string())?
                .map_err(|e| e.to_string())?;
            let (tx, rx) = ws.split();
            (Box::pin(tx), Box::pin(rx))
        }
        ClashTarget::Unix { path: socket_path } => {
            let url = build_url("ws", 80, &path, None);
            let (ws, _) = timeout(Duration::from_secs(5), async {
                let stream = UnixStream::connect(socket_path).await?;
                client_async(url.as_str(), stream)
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))
            })
            .await
            .map_err(|_| "Upstream connect timeout".to_string())?
            .map_err(|e| e.to_string())?;
            let (tx, rx) = ws.split();
            (Box::pin(tx), Box::pin(rx))
        }
    };

    let (mut client_tx, mut client_rx) = client_ws.split();

    let client_to_upstream = async {
        while let Some(Ok(msg)) = client_rx.next().await {
            if !session.authorized() {
                break;
            }
            let t_msg = match msg {
                Message::Text(t) => TMessage::Text(t.to_string().into()),
                Message::Binary(b) => TMessage::Binary(b),
                Message::Ping(p) => TMessage::Ping(p),
                Message::Pong(p) => TMessage::Pong(p),
                Message::Close(_) => {
                    let _ = upstream_tx.send(TMessage::Close(None)).await;
                    break;
                }
            };
            if upstream_tx.send(t_msg).await.is_err() {
                break;
            }
        }
    };

    let upstream_to_client = async {
        while let Some(Ok(msg)) = upstream_rx.next().await {
            if !session.authorized() {
                break;
            }
            let a_msg = match msg {
                TMessage::Text(t) => Message::Text(t.to_string().into()),
                TMessage::Binary(b) => Message::Binary(b),
                TMessage::Ping(p) => Message::Ping(p),
                TMessage::Pong(p) => Message::Pong(p),
                TMessage::Close(_) => {
                    let _ = client_tx.send(Message::Close(None)).await;
                    break;
                }
                _ => continue,
            };
            if client_tx.send(a_msg).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = client_to_upstream => {},
        _ = upstream_to_client => {},
    }

    Ok(())
}

fn should_forward_header(name: &HeaderName) -> bool {
    let n = name.as_str();
    !matches!(
        n,
        "host"
            | "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-protocol"
            | "content-length"
            | "x-clash-port"
            | "x-clash-secret"
            | "x-clash-unix"
            | "authorization"
            | "cookie"
            | "cookie2"
            | "proxy-authorization"
    )
}

fn should_forward_response_header(name: &HeaderName) -> bool {
    let n = name.as_str();
    !matches!(
        n,
        "connection"
            | "upgrade"
            | "sec-websocket-accept"
            | "sec-websocket-protocol"
            | "transfer-encoding"
            | "content-length"
            | "set-cookie"
            | "set-cookie2"
    )
}

pub(crate) fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn make_error(status: StatusCode, err: String) -> Response {
    (
        status,
        axum::Json(ApiResponse::<()> {
            success: false,
            error: Some(err),
            data: None,
        }),
    )
        .into_response()
}

async fn do_proxy_http(
    client: reqwest::Client, parts: http::request::Parts, body_bytes: axum::body::Bytes, url: reqwest::Url,
    secret: Option<String>,
) -> Response {
    let mut builder = client.request(parts.method.clone(), url);
    for (name, value) in parts.headers.iter() {
        if should_forward_header(name) {
            builder = builder.header(name, value);
        }
    }
    if let Some(secret) = secret {
        builder = builder.header("Authorization", format!("Bearer {}", secret));
    }

    match builder.body(body_bytes).send().await {
        Ok(upstream) => build_http_response(upstream).await,
        Err(e) => make_error(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

async fn build_http_response(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    if status.is_redirection() {
        return make_error(StatusCode::BAD_GATEWAY, "Редиректы Clash API запрещены".into());
    }
    let headers = upstream.headers().clone();
    let bytes = upstream.bytes().await.unwrap_or_default();

    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&bytes);
        let detail = serde_json::from_str::<serde_json::Value>(&body_text)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| body_text.trim().to_string());

        let skip = detail.contains("Timeout") || detail.contains("An error occurred in the delay test");
        if !skip {
            log("ERROR", format!("Ошибка Mihomo: {}", detail));
        }
    }

    let mut response = Response::builder().status(status);
    for (name, value) in headers.iter() {
        if should_forward_response_header(name) {
            response = response.header(name, value);
        }
    }
    response
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub(crate) async fn resolve_clash_target(
    port_override: Option<String>, secret_override: Option<String>, unix_override: Option<String>,
) -> Result<ClashTarget, String> {
    if let Some(u) = unix_override
        && let Some(path) = sanitize_unix_name(&u) {
        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(ClashTarget::Unix { path });
        }
        return Err("Unix сокет не найден на диске".into());
    }

    if let Some(port) = port_override {
        let port = parse_clash_port(&port)?;
        let docs = crate::ruleset_inspector::load_mihomo_yaml().await?;
        let configured = docs
            .first()
            .and_then(|doc| doc["external-controller"].as_str())
            .ok_or_else(|| "В сохранённом config.yaml не задан external-controller".to_string())?;
        authorize_clash_port(port, configured)?;
        return Ok(ClashTarget::Tcp {
            port,
            secret: secret_override,
        });
    }

    Err("Фронт не передал данные для подключения".into())
}

fn sanitize_unix_name(raw: &str) -> Option<String> {
    let name = FsPath::new(raw.trim()).file_name()?.to_string_lossy();
    if name.is_empty() {
        return None;
    }
    Some(format!("{}/{}", MIHOMO_CONF_DIR, name))
}

fn parse_clash_port(raw: &str) -> Result<u16, String> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Порт Clash API должен быть числом от 1 до 65535".into());
    }
    raw.parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| "Порт Clash API должен быть числом от 1 до 65535".into())
}

fn authorize_clash_port(requested: u16, configured: &str) -> Result<(), String> {
    let addr = configured
        .parse::<std::net::SocketAddrV4>()
        .map_err(|_| "Некорректный external-controller в сохранённом config.yaml".to_string())?;
    if !matches!(
        *addr.ip(),
        std::net::Ipv4Addr::LOCALHOST | std::net::Ipv4Addr::UNSPECIFIED
    ) || addr.port() == 0
    {
        return Err("external-controller должен слушать 127.0.0.1 или 0.0.0.0 на ненулевом порту".into());
    }
    if requested != addr.port() {
        return Err("Порт relay не совпадает с external-controller в сохранённом config.yaml".into());
    }
    Ok(())
}

pub(crate) fn relay_http_client(socket_path: Option<&str>) -> Result<reqwest::Client, String> {
    // Keep TCP connection pooling without sharing API clients that follow redirects.
    static TCP_CLIENT: std::sync::LazyLock<Result<reqwest::Client, String>> =
        std::sync::LazyLock::new(|| build_relay_http_client(None));
    match socket_path {
        None => TCP_CLIENT.clone(),
        Some(path) => build_relay_http_client(Some(path)),
    }
}

fn build_relay_http_client(socket_path: Option<&str>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("XKeen-UI")
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120));
    if let Some(path) = socket_path {
        builder = builder.unix_socket(path);
    }
    builder.build().map_err(|e| e.to_string())
}

pub(crate) fn build_url(scheme: &str, port: u16, path: &str, query: Option<&str>) -> reqwest::Url {
    let mut url = reqwest::Url::parse("http://127.0.0.1/").expect("fixed loopback URL");
    url.set_scheme(scheme).expect("HTTP or WebSocket scheme");
    url.set_port(Some(port)).expect("fixed loopback host supports a port");
    url.set_path(&format!("/{}", path.trim_start_matches('/')));
    url.set_query(query);
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        routing::{any, get},
    };
    use serde_json::json;

    fn fixture_state() -> AppState {
        use std::sync::{Arc, RwLock};
        let (log_tx, _) = tokio::sync::broadcast::channel(16);
        let mut settings = crate::types::AppSettings::default();
        settings.auth.enabled = false;
        AppState {
            core: Arc::new(RwLock::new(crate::types::CoreInfo {
                name: "mihomo".into(),
                conf_dir: String::new(),
                is_json: false,
            })),
            settings: Arc::new(RwLock::new(settings)),
            init_file: Arc::new(RwLock::new(None)),
            http_client: reqwest::Client::new(),
            update_checker: Default::default(),
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

    async fn serve(app: Router) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, task)
    }

    #[test]
    fn ports_are_strict_nonzero_u16() {
        for input in ["1", "9090", "65535", "09090"] {
            assert!(parse_clash_port(input).is_ok());
        }
        for input in [
            "",
            "0",
            "65536",
            "-1",
            "+80",
            " 80",
            "80 ",
            "80@example.invalid",
            "80/",
            "80?",
            "80#",
            "80\\n",
            "８０",
        ] {
            assert!(parse_clash_port(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn only_the_configured_controller_port_is_authorized() {
        assert!(authorize_clash_port(9090, "127.0.0.1:9090").is_ok());
        assert!(authorize_clash_port(12345, "0.0.0.0:12345").is_ok());
        for (port, controller) in [
            (79, "127.0.0.1:9090"),
            (80, "0.0.0.0:9090"),
            (0, "127.0.0.1:0"),
            (9090, "192.0.2.1:9090"),
            (9090, "example.invalid:9090"),
            (9090, "127.0.0.1:9090@evil.invalid"),
            (9090, ""),
            (9090, "127.0.0.1:65536"),
        ] {
            assert!(authorize_clash_port(port, controller).is_err(), "{port} / {controller}");
        }
    }

    #[test]
    fn urls_keep_loopback_and_preserve_encoded_path_and_query() {
        for scheme in ["http", "ws"] {
            let url = build_url(scheme, 9090, "proxies/A%2FB", Some("name=a%26b&type=A"));
            assert_eq!(url.host_str(), Some("127.0.0.1"));
            assert_eq!(url.port(), Some(9090));
            assert_eq!(url.username(), "");
            assert!(url.password().is_none());
            assert_eq!(url.path(), "/proxies/A%2FB");
            assert_eq!(url.query(), Some("name=a%26b&type=A"));
            let hostile_path = build_url(
                scheme,
                9090,
                "//example.invalid/@other",
                Some("@evil.invalid/#fragment"),
            );
            assert_eq!(hostile_path.host_str(), Some("127.0.0.1"));
            assert!(hostile_path.fragment().is_none());
        }
    }

    #[test]
    fn session_headers_are_blocked_in_both_directions() {
        for name in ["cookie", "cookie2", "authorization", "proxy-authorization"] {
            assert!(!should_forward_header(&HeaderName::from_static(name)));
        }
        for name in ["set-cookie", "set-cookie2"] {
            assert!(!should_forward_response_header(&HeaderName::from_static(name)));
        }
        assert!(should_forward_header(&HeaderName::from_static("content-type")));
        assert!(should_forward_response_header(&HeaderName::from_static("content-type")));
    }

    #[tokio::test]
    async fn http_and_websocket_reject_port_injection_without_connecting_to_target() {
        let decoy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let injected = format!("80@127.0.0.1:{}", decoy.local_addr().unwrap().port());
        let (port, server) = serve(
            Router::new()
                .route("/clash/{*path}", any(proxy_http))
                .route("/clash-ws/{*path}", get(proxy_ws))
                .layer(Extension(crate::auth::WsSession::from_state(
                    &fixture_state(),
                    &HeaderMap::new(),
                ))),
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for value in [injected.as_str(), "80@example.invalid", "0", "65536", "+80"] {
            let response = client
                .get(format!("http://127.0.0.1:{port}/clash/proxies"))
                .header("X-Clash-Port", value)
                .header("Cookie", "session=private")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let mut url = build_url("ws", port, "clash-ws/connections", None);
            url.query_pairs_mut().append_pair("port", value);
            let result = connect_async(url.as_str()).await;
            assert!(matches!(result, Err(TError::Http(ref response)) if response.status() == StatusCode::BAD_REQUEST));
        }
        assert!(timeout(Duration::from_millis(100), decoy.accept()).await.is_err());
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn revocation_cancels_pending_upstream_handshake() {
        use tokio::io::AsyncReadExt;
        let state = fixture_state();
        let session = crate::auth::WsSession::from_state(&state, &HeaderMap::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            connected_tx.send(()).unwrap();
            let mut bytes = Vec::new();
            // Deliberately never answer the upgrade request. Cancellation must close TCP
            // before the normal five-second upstream timeout expires.
            stream.read_to_end(&mut bytes).await.unwrap();
        });
        let target = ClashTarget::Tcp {
            port: upstream_port,
            secret: None,
        };
        let app = Router::new().route(
            "/relay",
            get(move |ws: WebSocketUpgrade| {
                let session = session.clone();
                let target = target.clone();
                async move {
                    ws.on_upgrade(move |socket| async move {
                        proxy_ws_authorized(socket, "events".into(), target, session)
                            .await
                            .unwrap();
                    })
                }
            }),
        );
        let (port, server) = serve(app).await;
        let (mut client, _) = connect_async(format!("ws://127.0.0.1:{port}/relay")).await.unwrap();
        timeout(Duration::from_secs(1), connected_rx).await.unwrap().unwrap();
        state.settings.write().unwrap().auth.enabled = true;
        state
            .auth_changes
            .send_modify(|version| *version = version.wrapping_add(1));
        timeout(Duration::from_secs(1), upstream).await.unwrap().unwrap();
        let end = timeout(Duration::from_secs(1), client.next()).await.unwrap();
        assert!(matches!(end, None | Some(Err(_)) | Some(Ok(TMessage::Close(_)))));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn relay_drops_both_connections_on_session_revocation_and_expiry() {
        for expire in [false, true] {
            let state = fixture_state();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            {
                let mut settings = state.settings.write().unwrap();
                settings.auth.enabled = true;
                settings.auth.session_ids = vec![format!("relay-fixture:{}", now + if expire { 2 } else { 60 })];
            }
            let mut headers = HeaderMap::new();
            headers.insert("cookie", "session_id=relay-fixture".parse().unwrap());
            let session = crate::auth::WsSession::from_state(&state, &headers);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream_port = listener.local_addr().unwrap().port();
            let upstream = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let first = ws.next().await.unwrap().unwrap();
                assert_eq!(first, TMessage::Text("before".into()));
                ws.send(first).await.unwrap();
                // No additional application frame may arrive after authorization ends.
                let end = ws.next().await;
                assert!(matches!(end, None | Some(Err(_)) | Some(Ok(TMessage::Close(_)))));
            });
            let target = ClashTarget::Tcp {
                port: upstream_port,
                secret: None,
            };
            let app = Router::new().route(
                "/relay",
                get(move |ws: WebSocketUpgrade| {
                    let session = session.clone();
                    let target = target.clone();
                    async move {
                        ws.on_upgrade(move |socket| async move {
                            proxy_ws_authorized(socket, "events".into(), target, session)
                                .await
                                .unwrap();
                        })
                    }
                }),
            );
            let (port, server) = serve(app).await;
            let (mut client, _) = connect_async(format!("ws://127.0.0.1:{port}/relay")).await.unwrap();
            client.send(TMessage::Text("before".into())).await.unwrap();
            assert_eq!(client.next().await.unwrap().unwrap(), TMessage::Text("before".into()));
            if !expire {
                state.settings.write().unwrap().auth.session_ids.clear();
                state
                    .auth_changes
                    .send_modify(|version| *version = version.wrapping_add(1));
            }
            let end = timeout(Duration::from_secs(4), client.next()).await.unwrap();
            assert!(matches!(end, None | Some(Err(_)) | Some(Ok(TMessage::Close(_)))));
            timeout(Duration::from_secs(1), upstream).await.unwrap().unwrap();
            server.abort();
            let _ = server.await;
        }
    }

    #[tokio::test]
    async fn http_relay_isolates_cookies_and_preserves_api_headers() {
        let (port, server) = serve(Router::new().route(
            "/proxies/{*name}",
            any(|request: Request<Body>| async move {
                let body = json!({
                    "cookie": header_value(request.headers(), "cookie"),
                    "authorization": header_value(request.headers(), "authorization"),
                    "target_header": header_value(request.headers(), "x-clash-port"),
                    "accept": header_value(request.headers(), "accept"),
                    "path": request.uri().path(), "query": request.uri().query(),
                });
                (
                    [("set-cookie", "session=upstream; Path=/"), ("x-upstream", "kept")],
                    Json(body),
                )
            }),
        ))
        .await;
        let (parts, _) = Request::builder()
            .method("GET")
            .header("cookie", "session=private")
            .header("authorization", "Bearer ui-private")
            .header("x-clash-port", "9090")
            .header("accept", "application/json")
            .body(Body::empty())
            .unwrap()
            .into_parts();
        let response = do_proxy_http(
            relay_http_client(None).unwrap(),
            parts,
            axum::body::Bytes::new(),
            build_url("http", port, "proxies/A%2FB", Some("name=a%26b")),
            Some("mihomo-secret".into()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("set-cookie"));
        assert_eq!(response.headers()["x-upstream"], "kept");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert!(body["cookie"].is_null());
        assert!(body["target_header"].is_null());
        assert_eq!(body["authorization"], "Bearer mihomo-secret");
        assert_eq!(body["accept"], "application/json");
        assert_eq!(body["path"], "/proxies/A%2FB");
        assert_eq!(body["query"], "name=a%26b");
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn upstream_redirect_is_neither_followed_nor_forwarded() {
        let decoy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let location = format!("http://{}/capture", decoy.local_addr().unwrap());
        let (port, server) = serve(Router::new().route(
            "/proxies",
            get(move || {
                let location = location.clone();
                async move { (StatusCode::FOUND, [("location", location)], "redirect") }
            }),
        ))
        .await;
        let (parts, _) = Request::builder().body(Body::empty()).unwrap().into_parts();
        let response = do_proxy_http(
            relay_http_client(None).unwrap(),
            parts,
            axum::body::Bytes::new(),
            build_url("http", port, "proxies", None),
            Some("private-secret".into()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(!response.headers().contains_key("location"));
        assert!(timeout(Duration::from_millis(100), decoy.accept()).await.is_err());
        server.abort();
        let _ = server.await;
    }
}
