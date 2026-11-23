// 开放 RPC 中继服务 (HTTP /health + WebSocket /rpc)
//
// 仅做哑管道中继: 绑定 127.0.0.1:29304，把 WS 连接生命周期和消息转发给主 webview，
// 真正的 birpc 方法注册和权限控制都在前端 useFetcherRpcServer.ts 中完成。

use crate::ffmpeg_jobs::FFMPEG_JOBS;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

pub const RPC_PORT: u16 = 29304;

// 生产环境仅放行 https 协议的白名单来源
const HTTPS_ONLY_ORIGIN_HOSTS: &[&str] = &["vtsuru.suki.club", "vtsuru.live"];
// 本地开发与 Tauri 内部源允许 http / https
const LOCAL_ORIGIN_HOSTS: &[&str] = &["localhost", "127.0.0.1", "tauri.localhost"];

fn is_origin_allowed(origin: &str, allow_empty: bool) -> bool {
    if origin.is_empty() {
        return allow_empty;
    }
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or("");

    if LOCAL_ORIGIN_HOSTS.contains(&host) {
        return true;
    }
    scheme == "https" && HTTPS_ONLY_ORIGIN_HOSTS.contains(&host)
}

#[derive(Clone, Serialize)]
struct OpenPayload {
    conn_id: String,
    origin: String,
}

#[derive(Clone, Serialize)]
struct MessagePayload {
    conn_id: String,
    data: String,
}

#[derive(Clone, Serialize)]
struct ClosePayload {
    conn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcServerStatus {
    pub running: bool,
    pub port: u16,
    pub connection_count: usize,
    pub last_error: Option<String>,
}

type ConnMap = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Message>>>>;

pub struct RpcServerManager {
    running: Arc<Mutex<bool>>,
    last_error: Arc<Mutex<Option<String>>>,
    app_handle: Arc<Mutex<Option<AppHandle>>>,
    connections: ConnMap,
    conn_seq: AtomicU64,
}

impl RpcServerManager {
    pub fn new() -> Self {
        RpcServerManager {
            running: Arc::new(Mutex::new(false)),
            last_error: Arc::new(Mutex::new(None)),
            app_handle: Arc::new(Mutex::new(None)),
            connections: Arc::new(Mutex::new(HashMap::new())),
            conn_seq: AtomicU64::new(1),
        }
    }

    pub fn start(&self, app_handle: AppHandle) {
        {
            let mut running = self.running.lock();
            if *running {
                return;
            }
            *running = true;
            *self.last_error.lock() = None;
            *self.app_handle.lock() = Some(app_handle);
        }

        let connections = self.connections.clone();
        let app_handle_arc = self.app_handle.clone();
        let running_arc = self.running.clone();
        let last_error_arc = self.last_error.clone();

        thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    let msg = format!("创建 RPC Tokio 运行时失败: {e}");
                    log::error!("[RPC] {msg}");
                    *last_error_arc.lock() = Some(msg);
                    *running_arc.lock() = false;
                    return;
                }
            };
            rt.block_on(async move {
                let state = AppState {
                    connections,
                    app_handle: app_handle_arc,
                };
                let app = Router::new()
                    .route("/health", get(health_handler).options(preflight_handler))
                    .route("/rpc", get(ws_handler))
                    .route("/media/jobs/:job_id/stdin", get(ffmpeg_stdin_handler))
                    .route("/media/jobs/:job_id/stdout", get(ffmpeg_stdout_handler))
                    .route("/media/jobs/:job_id/stderr", get(ffmpeg_stderr_handler))
                    .with_state(state);

                let addr = format!("127.0.0.1:{}", RPC_PORT);
                match tokio::net::TcpListener::bind(&addr).await {
                    Ok(listener) => {
                        log::info!("[RPC] 开放接口已启动 http://{}", addr);
                        if let Err(e) = axum::serve(listener, app).await {
                            let msg = format!("服务器异常退出: {e}");
                            log::error!("[RPC] {msg}");
                            *last_error_arc.lock() = Some(msg);
                            *running_arc.lock() = false;
                        }
                    }
                    Err(e) => {
                        let msg = format!("端口 {} 绑定失败（可能被其他程序占用）: {}", RPC_PORT, e);
                        log::error!("[RPC] {msg}");
                        *last_error_arc.lock() = Some(msg);
                        *running_arc.lock() = false;
                    }
                }
            });
        });
    }

    pub fn send(&self, conn_id: &str, data: String) -> Result<(), String> {
        let conns = self.connections.lock();
        let sender = conns
            .get(conn_id)
            .ok_or_else(|| format!("连接不存在: {}", conn_id))?;
        sender
            .send(Message::Text(data.into()))
            .map_err(|e| format!("发送失败: {}", e))
    }

    pub fn close(&self, conn_id: &str) -> Result<(), String> {
        let conns = self.connections.lock();
        let sender = conns
            .get(conn_id)
            .ok_or_else(|| format!("连接不存在: {}", conn_id))?;
        let _ = sender.send(Message::Close(None));
        Ok(())
    }

    pub fn get_status(&self) -> RpcServerStatus {
        RpcServerStatus {
            running: *self.running.lock(),
            port: RPC_PORT,
            connection_count: self.connections.lock().len(),
            last_error: self.last_error.lock().clone(),
        }
    }

    fn next_conn_id(&self) -> String {
        format!("c{}", self.conn_seq.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone)]
struct AppState {
    connections: ConnMap,
    app_handle: Arc<Mutex<Option<AppHandle>>>,
}

impl AppState {
    fn emit<S: Serialize + Clone>(&self, event: &str, payload: S) {
        if let Some(handle) = self.app_handle.lock().as_ref() {
            let _ = handle.emit(event, payload);
        }
    }
}

fn cors_headers(origin: &str) -> [(&'static str, HeaderValue); 4] {
    let allow_origin = if is_origin_allowed(origin, false) {
        origin
    } else {
        ""
    };
    [
        (
            "access-control-allow-origin",
            HeaderValue::from_str(allow_origin).unwrap_or(HeaderValue::from_static("")),
        ),
        (
            "access-control-allow-methods",
            HeaderValue::from_static("GET, OPTIONS"),
        ),
        (
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        ),
        ("vary", HeaderValue::from_static("Origin")),
    ]
}

async fn health_handler(headers: HeaderMap) -> impl IntoResponse {
    let origin = origin_of(&headers);
    (
        cors_headers(&origin),
        axum::Json(serde_json::json!({
            "name": "vtsuru-fetcher-client",
            "version": env!("CARGO_PKG_VERSION"),
            "rpc": true,
        })),
    )
}

async fn preflight_handler(headers: HeaderMap) -> impl IntoResponse {
    let origin = origin_of(&headers);
    (StatusCode::NO_CONTENT, cors_headers(&origin))
}

fn origin_of(headers: &HeaderMap) -> String {
    headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let origin = origin_of(&headers);
    if !is_origin_allowed(&origin, false) {
        log::warn!("[RPC] 拒绝未授权来源: '{}'", origin);
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state, origin))
}

#[derive(Deserialize)]
struct FfmpegPipeQuery {
    token: String,
}

async fn ffmpeg_stdin_handler(
    ws: WebSocketUpgrade,
    Path(job_id): Path<String>,
    Query(query): Query<FfmpegPipeQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !is_origin_allowed(&origin_of(&headers), false) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(stdin) = FFMPEG_JOBS.stdin(&job_id, &query.token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ws.on_upgrade(move |socket| async move {
        let (_, mut receiver) = socket.split();
        while let Some(Ok(message)) = receiver.next().await {
            match message {
                Message::Binary(chunk) => {
                    if stdin.send(chunk.to_vec()).await.is_err() {
                        return;
                    }
                }
                Message::Close(_) => return,
                _ => {}
            }
        }
    })
}

async fn ffmpeg_stdout_handler(
    ws: WebSocketUpgrade,
    Path(job_id): Path<String>,
    Query(query): Query<FfmpegPipeQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !is_origin_allowed(&origin_of(&headers), false) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(mut stdout) = FFMPEG_JOBS.take_stdout(&job_id, &query.token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ws.on_upgrade(move |socket| async move {
        let (mut sender, _) = socket.split();
        while let Some(chunk) = stdout.recv().await {
            if sender.send(Message::Binary(chunk.into())).await.is_err() {
                return;
            }
        }
        let _ = sender.send(Message::Close(None)).await;
    })
}

async fn ffmpeg_stderr_handler(
    ws: WebSocketUpgrade,
    Path(job_id): Path<String>,
    Query(query): Query<FfmpegPipeQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !is_origin_allowed(&origin_of(&headers), false) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(mut stderr) = FFMPEG_JOBS.take_stderr(&job_id, &query.token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ws.on_upgrade(move |socket| async move {
        let (mut sender, _) = socket.split();
        while let Some(line) = stderr.recv().await {
            if sender.send(Message::Text(line.into())).await.is_err() {
                return;
            }
        }
        let _ = sender.send(Message::Close(None)).await;
    })
}

async fn handle_socket(socket: WebSocket, state: AppState, origin: String) {
    let conn_id = RPC_SERVER.next_conn_id();
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

    state.connections.lock().insert(conn_id.clone(), out_tx);
    state.emit(
        "rpc://open",
        OpenPayload {
            conn_id: conn_id.clone(),
            origin,
        },
    );

    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(text) => state.emit(
                "rpc://message",
                MessagePayload {
                    conn_id: conn_id.clone(),
                    data: text.to_string(),
                },
            ),
            Message::Close(_) => break,
            _ => {}
        }
    }

    send_task.abort();
    state.connections.lock().remove(&conn_id);
    state.emit("rpc://close", ClosePayload { conn_id });
}

lazy_static::lazy_static! {
    pub static ref RPC_SERVER: RpcServerManager = RpcServerManager::new();
}

#[cfg(test)]
mod tests {
    use super::is_origin_allowed;

    #[test]
    fn enforces_origin_policy() {
        assert!(is_origin_allowed("https://vtsuru.suki.club", false));
        assert!(is_origin_allowed("https://vtsuru.live", false));
        assert!(!is_origin_allowed("http://vtsuru.live", false));
        assert!(!is_origin_allowed("https://vtsuru.live.evil.com", false));
        assert!(!is_origin_allowed("https://evil.com@vtsuru.live", false));
        assert!(is_origin_allowed("http://localhost:5100", false));
        assert!(is_origin_allowed("http://127.0.0.1:5100", false));
        assert!(is_origin_allowed("https://tauri.localhost", false));
        assert!(!is_origin_allowed("", false));
        assert!(is_origin_allowed("", true));
    }
}
