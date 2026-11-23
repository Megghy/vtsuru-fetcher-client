use crate::paths::append_log_file;
use crate::tray::reload_main_window_internal;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

#[path = "heartbeat_state.rs"]
mod state;
use state::{WatchdogEvent, WatchdogState, STARTUP_TIMEOUT};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatStatus {
    pub last_heartbeat: Option<String>,
    pub elapsed_seconds: Option<u64>,
    pub timeout_seconds: u64,
    pub is_monitoring: bool,
    pub timeout_count: u64,
    pub auto_reload_enabled: bool,
}

pub struct HeartbeatMonitor {
    state: Arc<Mutex<WatchdogState>>,
    timeout_duration: Duration,
    timeout_count: Arc<AtomicU64>,
    auto_reload_enabled: Arc<AtomicBool>,
}

impl HeartbeatMonitor {
    pub fn new(timeout_seconds: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(WatchdogState::new())),
            timeout_duration: Duration::from_secs(timeout_seconds),
            timeout_count: Arc::new(AtomicU64::new(0)),
            auto_reload_enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_auto_reload(&self, enabled: bool) {
        self.auto_reload_enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn update_heartbeat(&self) {
        if self.state.lock().heartbeat(Instant::now()) {
            append_log_file(None, "heartbeat.log", "前端心跳已恢复，继续监控");
        }
    }

    pub fn start_monitoring(&self, app_handle: AppHandle) {
        if !self.state.lock().start(Instant::now()) {
            return;
        }
        append_log_file(
            Some(&app_handle),
            "heartbeat.log",
            &format!(
                "开始监控前端心跳：启动等待 {} 秒，运行失联阈值 {} 秒",
                STARTUP_TIMEOUT.as_secs(),
                self.timeout_duration.as_secs()
            ),
        );
        let state = Arc::clone(&self.state);
        let timeout = self.timeout_duration;
        let timeout_count = Arc::clone(&self.timeout_count);
        let auto_reload = Arc::clone(&self.auto_reload_enabled);

        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            let event = state.lock().check(Instant::now(), timeout);
            let (is_heartbeat_timeout, message) = match event {
                Some(WatchdogEvent::StartupTimeout) => (
                    false,
                    format!(
                        "前端启动超时：等待 {} 秒仍未收到首次心跳。请检查网络或从托盘重新加载界面，客户端将继续等待恢复。",
                        STARTUP_TIMEOUT.as_secs()
                    ),
                ),
                Some(WatchdogEvent::HeartbeatTimeout) => (
                    true,
                    format!(
                        "前端心跳超时：已超过 {} 秒未响应，事件收集可能受到影响。{}",
                        timeout.as_secs(),
                        if auto_reload.load(Ordering::SeqCst) {
                            "已自动尝试重新加载主界面。"
                        } else {
                            "可在托盘菜单点击「重新加载界面」恢复。"
                        }
                    ),
                ),
                Some(WatchdogEvent::Resumed) => {
                    append_log_file(
                        Some(&app_handle),
                        "heartbeat.log",
                        "检测到监控线程长时间暂停，已重新给予心跳等待时间",
                    );
                    continue;
                }
                None => continue,
            };

            timeout_count.fetch_add(1, Ordering::Relaxed);
            append_log_file(Some(&app_handle), "heartbeat.log", &message);

            if is_heartbeat_timeout && auto_reload.load(Ordering::SeqCst) {
                reload_main_window_internal(&app_handle);
            }

            if let Err(error) = app_handle
                .notification()
                .builder()
                .title("VTsuru 事件收集器")
                .body(&message)
                .show()
            {
                append_log_file(
                    Some(&app_handle),
                    "heartbeat.log",
                    &format!("发送心跳异常通知失败: {error}"),
                );
            }
        });
    }

    pub fn get_status(&self) -> HeartbeatStatus {
        let state = self.state.lock();
        let elapsed_seconds = state.last_heartbeat.map(|instant| instant.elapsed().as_secs());
        HeartbeatStatus {
            last_heartbeat: elapsed_seconds.map(|s| format!("{s}s ago")),
            elapsed_seconds,
            timeout_seconds: self.timeout_duration.as_secs(),
            is_monitoring: state.started_at.is_some(),
            timeout_count: self.timeout_count.load(Ordering::Relaxed),
            auto_reload_enabled: self.auto_reload_enabled.load(Ordering::SeqCst),
        }
    }
}

lazy_static::lazy_static! {
    pub static ref HEARTBEAT_MONITOR: HeartbeatMonitor = HeartbeatMonitor::new(30);
}
