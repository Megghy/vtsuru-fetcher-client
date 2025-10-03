use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

// 心跳状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatStatus {
    pub last_heartbeat: Option<String>,
    pub timeout_seconds: u64,
    pub is_monitoring: bool,
}

// 心跳监控器
pub struct HeartbeatMonitor {
    last_heartbeat: Arc<Mutex<Option<Instant>>>,
    timeout_duration: Duration,
    is_monitoring: Arc<Mutex<bool>>,
}

impl HeartbeatMonitor {
    pub fn new(timeout_seconds: u64) -> Self {
        HeartbeatMonitor {
            last_heartbeat: Arc::new(Mutex::new(None)),
            timeout_duration: Duration::from_secs(timeout_seconds),
            is_monitoring: Arc::new(Mutex::new(false)),
        }
    }

    fn notify_and_exit(app_handle: &AppHandle, timeout_duration: Duration) -> ! {
        let _ = app_handle
            .notification()
            .builder()
            .title("VTsuru 事件收集器")
            .body(&format!(
                "前端加载失败，已超过 {} 秒未响应。应用即将退出。",
                timeout_duration.as_secs()
            ))
            .show();

        thread::sleep(Duration::from_secs(3));

        eprintln!("由于前端长时间未响应，应用退出");
        std::process::exit(1);
    }

    // 更新心跳时间
    pub fn update_heartbeat(&self) {
        let mut last = self.last_heartbeat.lock().unwrap();
        *last = Some(Instant::now());
    }

    // 启动监控
    pub fn start_monitoring(&self, app_handle: AppHandle) {
        let mut is_monitoring = self.is_monitoring.lock().unwrap();
        if *is_monitoring {
            return;
        }
        *is_monitoring = true;
        drop(is_monitoring);

        let last_heartbeat = self.last_heartbeat.clone();
        let timeout_duration = self.timeout_duration;
        let is_monitoring_arc = self.is_monitoring.clone();
        let app_handle = app_handle.clone();

        thread::spawn(move || {
            let start_time = Instant::now();
            // 等待首次心跳，给前端足够的启动时间（稍短一些，因为总超时时间只有15秒）
            let initial_wait = Duration::from_secs(5);
            thread::sleep(initial_wait);

            loop {
                thread::sleep(Duration::from_secs(2)); // 每2秒检查一次

                let monitoring = *is_monitoring_arc.lock().unwrap();
                if !monitoring {
                    break;
                }

                let last = last_heartbeat.lock().unwrap().clone();

                if let Some(last_time) = last {
                    let elapsed = last_time.elapsed();

                    if elapsed > timeout_duration {
                        eprintln!(
                            "心跳超时: 已 {:?} 未收到前端心跳（阈值: {:?}）",
                            elapsed, timeout_duration
                        );

                        Self::notify_and_exit(&app_handle, timeout_duration);
                    }
                } else {
                    // 还未收到首次心跳，检查是否超时
                    // 这里我们使用一个更长的超时时间，因为前端可能需要时间加载
                    eprintln!("警告: 尚未收到前端首次心跳");

                    if start_time.elapsed() > timeout_duration {
                        eprintln!(
                            "前端启动超时: 已 {:?} 未收到首次心跳（阈值: {:?}）",
                            start_time.elapsed(),
                            timeout_duration
                        );

                        Self::notify_and_exit(&app_handle, timeout_duration);
                    }
                }
            }
        });
    }
    pub fn stop_monitoring(&self) {
        let mut is_monitoring = self.is_monitoring.lock().unwrap();
        *is_monitoring = false;
    }

    // 获取状态
    pub fn get_status(&self) -> HeartbeatStatus {
        let last = self.last_heartbeat.lock().unwrap();
        let is_monitoring = *self.is_monitoring.lock().unwrap();

        HeartbeatStatus {
            last_heartbeat: last.map(|instant| {
                format!("{:?} ago", instant.elapsed())
            }),
            timeout_seconds: self.timeout_duration.as_secs(),
            is_monitoring,
        }
    }
}

// 创建心跳监控器的单例
lazy_static::lazy_static! {
    pub static ref HEARTBEAT_MONITOR: HeartbeatMonitor = HeartbeatMonitor::new(15); // 15秒超时
}
