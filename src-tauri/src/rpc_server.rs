// 开放 RPC 中继服务器
//
// Rust 端只做「哑管道」: 在一个端口上同时提供 GET /health 探测与 WS /rpc。
// 所有 RPC 方法逻辑都在 JS (Tauri webview) 端用 birpc 实现, 因此新增接口无需重新编译本客户端。
//
// 数据流:
//   外部网页 --WS--> [本模块] --emit rpc://message--> [webview birpc server]
//   [webview] --invoke rpc_send--> [本模块] --WS--> 外部网页
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

const RPC_PORT: u16 = 29304;

// 允许连接的来源白名单 (WS Origin 头 / HTTP CORS)。localhost 任意端口用于本地开发。
const ALLOWED_ORIGIN_HOSTS: &[&str] = &["vtsuru.suki.club", "vtsuru.live"];

fn is_origin_allowed(origin: &str) -> bool {
    // 允许空 Origin (部分非浏览器客户端), 白名单域名, 以及本地开发
    if origin.is_empty() {
        return true;
    }
    let host = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .split('/')
        .next()
        .unwrap_or("");
    let host_no_port = host.split(':').next().unwrap_or(host);
    ALLOWED_ORIGIN_HOSTS.contains(&host_no_port)
        || host_no_port == "localhost"
        || host_no_port == "127.0.0.1"
        || host_no_port == "tauri.localhost"
}

// ── 事件负载 (emit 给 webview) ─────────────────────────────
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
pub struct RpcServerStatus {
    pub running: bool,
    pub port: u16,
    pub connection_count: usize,
}

// 每个连接持有一个 mpsc sender, rpc_send 命令通过它把数据推回对应 WS
type ConnMap = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<Message>>>>;

pub struct RpcServerManager {
    running: Arc<Mutex<bool>>,
    app_handle: Arc<Mutex<Option<AppHandle>>>,
    connections: ConnMap,
    conn_seq: AtomicU64,
}

impl RpcServerManager {
    pub fn new() -> Self {
        RpcServerManager {
            running: Arc::new(Mutex::new(false)),
            app_handle: Arc::new(Mutex::new(None)),
            connections: Arc::new(Mutex::new(HashMap::new())),
            conn_seq: AtomicU64::new(1),
        }
    }

    // 启动服务器 (在 setup 时调用, 传入 AppHandle 用于 emit 事件)
    pub fn start(&self, app_handle: AppHandle) {
        let mut running = self.running.lock().unwrap();
        if *running {
            return;
        }
        *running = true;
        *self.app_handle.lock().unwrap() = Some(app_handle);
        drop(running);

        let connections = self.connections.clone();
        let app_handle_arc = self.app_handle.clone();
        let running_arc = self.running.clone();

        thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let state = AppState {
                    connections,
                    app_handle: app_handle_arc,
                };
                let app = Router::new()
                    .route("/health", get(health_handler).options(preflight_handler))
                    .route("/rpc", get(ws_handler))
                    .with_state(state);

                let addr = format!("127.0.0.1:{}", RPC_PORT);
                match tokio::net::TcpListener::bind(&addr).await {
                    Ok(listener) => {
                        log::info!("[RPC] 开放接口已启动 http://{}", addr);
                        if let Err(e) = axum::serve(listener, app).await {
                            log::error!("[RPC] 服务器异常退出: {}", e);
                        }
                    }
                    Err(e) => {
                        log::error!("[RPC] 端口 {} 绑定失败: {}", RPC_PORT, e);
                        *running_arc.lock().unwrap() = false;
                    }
                }
            });
        });
    }

    // 把数据推回指定连接
    pub fn send(&self, conn_id: &str, data: String) -> Result<(), String> {
        let conns = self.connections.lock().unwrap();
        let sender = conns
            .get(conn_id)
            .ok_or_else(|| format!("连接不存在: {}", conn_id))?;
        sender
            .send(Message::Text(data))
            .map_err(|e| format!("发送失败: {}", e))
    }

    // 主动关闭指定连接
    pub fn close(&self, conn_id: &str) -> Result<(), String> {
        let conns = self.connections.lock().unwrap();
        let sender = conns
            .get(conn_id)
            .ok_or_else(|| format!("连接不存在: {}", conn_id))?;
        let _ = sender.send(Message::Close(None));
        Ok(())
    }

    pub fn get_status(&self) -> RpcServerStatus {
        RpcServerStatus {
            running: *self.running.lock().unwrap(),
            port: RPC_PORT,
            connection_count: self.connections.lock().unwrap().len(),
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
        if let Some(handle) = self.app_handle.lock().unwrap().as_ref() {
            let _ = handle.emit(event, payload);
        }
    }
}

// 跨源探测所需的响应头。外部页面 (公网源) fetch 本机 loopback 属跨源 + Private Network Access,
// 浏览器要求 GET 回显 CORS 头, 且 OPTIONS 预检要带 Access-Control-Allow-Private-Network。
fn cors_headers(origin: &str) -> [(&'static str, HeaderValue); 4] {
    // 仅回显白名单内的源, 否则不放行 (返回请求源无意义, 用空串让浏览器拦截)
    let allow_origin = if is_origin_allowed(origin) { origin } else { "" };
    [
        (
            "access-control-allow-origin",
            HeaderValue::from_str(allow_origin).unwrap_or(HeaderValue::from_static("")),
        ),
        ("access-control-allow-methods", HeaderValue::from_static("GET, OPTIONS")),
        ("access-control-allow-private-network", HeaderValue::from_static("true")),
        ("vary", HeaderValue::from_static("Origin")),
    ]
}

// GET /health — 供外部页面探测本地是否安装了 eventfetcher
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

// OPTIONS /health — Private Network Access 预检
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

// WS /rpc — 中继升级
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let origin = origin_of(&headers);
    if !is_origin_allowed(&origin) {
        log::warn!("[RPC] 拒绝来源: {}", origin);
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state, origin))
}

async fn handle_socket(socket: WebSocket, state: AppState, origin: String) {
    let conn_id = RPC_SERVER.next_conn_id();
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

    state
        .connections
        .lock()
        .unwrap()
        .insert(conn_id.clone(), out_tx);
    state.emit(
        "rpc://open",
        OpenPayload {
            conn_id: conn_id.clone(),
            origin,
        },
    );

    // 出站: mpsc -> WS
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 入站: WS -> emit rpc://message
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(text) => state.emit(
                "rpc://message",
                MessagePayload {
                    conn_id: conn_id.clone(),
                    data: text,
                },
            ),
            Message::Close(_) => break,
            _ => {}
        }
    }

    // 清理
    send_task.abort();
    state.connections.lock().unwrap().remove(&conn_id);
    state.emit("rpc://close", ClosePayload { conn_id });
}

lazy_static::lazy_static! {
    pub static ref RPC_SERVER: RpcServerManager = RpcServerManager::new();
}
