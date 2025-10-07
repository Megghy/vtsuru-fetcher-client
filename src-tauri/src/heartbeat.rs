use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use chrono::Local;

// 写入心跳日志
fn write_heartbeat_log(message: &str) {
    let log_dir = if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("live.vtsuru.fetcher.client").join("logs"))
            .unwrap_or_else(|_| PathBuf::from("./logs"))
    } else {
        PathBuf::from("./logs")
    };

    // 确保日志目录存在
    let _ = fs::create_dir_all(&log_dir);

    let log_file = log_dir.join("heartbeat.log");
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let log_message = format!("[{}] {}\n", timestamp, message);

    // 写入日志文件
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
    {
        let _ = file.write_all(log_message.as_bytes());
        let _ = file.flush();
    }

    // 同时输出到stderr
    eprintln!("{}", log_message.trim());
}

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
        let error_msg = format!(
            "前端加载失败，已超过 {} 秒未响应。应用即将退出。",
            timeout_duration.as_secs()
        );
        
        // 写入错误日志
        write_heartbeat_log(&format!("致命错误: {}", error_msg));
        
        let _ = app_handle
            .notification()
            .builder()
            .title("VTsuru 事件收集器")
            .body(&error_msg)
            .show();

        thread::sleep(Duration::from_secs(3));

        eprintln!("由于前端长时间未响应，应用退出");
        std::process::exit(1);
    }

    // 更新心跳时间
    pub fn update_heartbeat(&self) {
        let mut last = self.last_heartbeat.lock().unwrap();
        *last = Some(Instant::now());
        // 正常心跳不记录日志
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
            // 等待首次心跳，给前端足够的启动时间
            let initial_wait = Duration::from_secs(10);
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
                        let error_msg = format!(
                            "心跳超时: 已 {:?} 未收到前端心跳（阈值: {:?}）",
                            elapsed, timeout_duration
                        );
                        write_heartbeat_log(&error_msg);
                        eprintln!("{}", error_msg);

                        Self::notify_and_exit(&app_handle, timeout_duration);
                    }
                } else {
                    // 还未收到首次心跳，检查是否超时
                    let elapsed = start_time.elapsed();

                    if elapsed > timeout_duration {
                        let error_msg = format!(
                            "前端启动超时: 已 {:?} 未收到首次心跳（阈值: {:?}）",
                            elapsed, timeout_duration
                        );
                        write_heartbeat_log(&error_msg);
                        eprintln!("{}", error_msg);

                        Self::notify_and_exit(&app_handle, timeout_duration);
                    }
                }
            }
        });
    }
    #[allow(dead_code)]
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
    pub static ref HEARTBEAT_MONITOR: HeartbeatMonitor = HeartbeatMonitor::new(30); // 30秒超时，给前端更多时间初始化
}
