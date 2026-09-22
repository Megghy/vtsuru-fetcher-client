use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

#[path = "heartbeat_state.rs"]
mod state;
use state::{WatchdogEvent, WatchdogState, STARTUP_TIMEOUT};

fn write_heartbeat_log(message: &str) {
    let log_dir = if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(|appdata| {
                PathBuf::from(appdata)
                    .join("live.vtsuru.fetcher.client")
                    .join("logs")
            })
            .unwrap_or_else(|_| PathBuf::from("./logs"))
    } else {
        PathBuf::from("./logs")
    };
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let log_message = format!("[{}] {}\n", timestamp, message);
    let result = (|| -> std::io::Result<()> {
        fs::create_dir_all(&log_dir)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("heartbeat.log"))?
            .write_all(log_message.as_bytes())
    })();
    if let Err(error) = result {
        eprintln!("写入心跳日志失败: {error}");
    }
    eprintln!("{}", log_message.trim());
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatStatus {
    pub last_heartbeat: Option<String>,
    pub timeout_seconds: u64,
    pub is_monitoring: bool,
}

pub struct HeartbeatMonitor {
    state: Arc<Mutex<WatchdogState>>,
    timeout_duration: Duration,
}

impl HeartbeatMonitor {
    pub fn new(timeout_seconds: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(WatchdogState::new())),
            timeout_duration: Duration::from_secs(timeout_seconds),
        }
    }

    pub fn update_heartbeat(&self) {
        if self.state.lock().unwrap().heartbeat(Instant::now()) {
            write_heartbeat_log("前端心跳已恢复，继续监控");
        }
    }

    pub fn start_monitoring(&self, app_handle: AppHandle) {
        if !self.state.lock().unwrap().start(Instant::now()) {
            return;
        }
        write_heartbeat_log(&format!(
            "开始监控前端心跳：启动等待 {} 秒，运行失联阈值 {} 秒",
            STARTUP_TIMEOUT.as_secs(),
            self.timeout_duration.as_secs()
        ));
        let state = Arc::clone(&self.state);
        let timeout = self.timeout_duration;
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            let event = state.lock().unwrap().check(Instant::now(), timeout);
            let message = match event {
                Some(WatchdogEvent::StartupTimeout) => format!(
                    "前端启动超时：等待 {} 秒仍未收到首次心跳。请检查网络或重新打开窗口，客户端将继续等待恢复。",
                    STARTUP_TIMEOUT.as_secs()
                ),
                Some(WatchdogEvent::HeartbeatTimeout) => format!(
                    "前端心跳超时：已超过 {} 秒未响应，事件收集可能受到影响。请检查客户端窗口，客户端将继续等待恢复。",
                    timeout.as_secs()
                ),
                Some(WatchdogEvent::Resumed) => {
                    write_heartbeat_log("检测到监控线程长时间暂停，已重新给予心跳等待时间");
                    continue;
                }
                None => continue,
            };
            write_heartbeat_log(&message);
            if let Err(error) = app_handle
                .notification()
                .builder()
                .title("VTsuru 事件收集器")
                .body(&message)
                .show()
            {
                write_heartbeat_log(&format!("发送心跳异常通知失败: {error}"));
            }
        });
    }

    pub fn get_status(&self) -> HeartbeatStatus {
        let state = self.state.lock().unwrap();
        HeartbeatStatus {
            last_heartbeat: state
                .last_heartbeat
                .map(|instant| format!("{:?} ago", instant.elapsed())),
            timeout_seconds: self.timeout_duration.as_secs(),
            is_monitoring: state.started_at.is_some(),
        }
    }
}

lazy_static::lazy_static! {
    pub static ref HEARTBEAT_MONITOR: HeartbeatMonitor = HeartbeatMonitor::new(30);
}
