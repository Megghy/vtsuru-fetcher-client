#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
use chrono::Local;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use tauri::Manager;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

// Import necessary items
use serde::Serialize;
use sysinfo::System;

// 引入文件服务器模块
mod file_server;
use file_server::{FileServerConfig, FileServerStatus, FILE_SERVER};

// 引入心跳监控模块
mod heartbeat;
use heartbeat::{HeartbeatStatus, HEARTBEAT_MONITOR};

// 引入RTMP转发模块
mod rtmp_relay;
use rtmp_relay::{FfmpegStatus, RtmpRelayConfig, RtmpRelayStatus, RTMP_RELAY};

// Define a struct to represent the data we want to send to the frontend.
// It needs `Serialize` to be convertible to JSON.
#[derive(Serialize, Clone)] // Clone is useful if you might pass this around
struct MemoryInfo {
    total: u64, // Use u64 for byte counts, which can be large
    free: u64,
}

// 写入错误日志到文件
fn write_error_log(message: &str) {
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

    // 确保日志目录存在
    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!("无法创建日志目录: {}", e);
        return;
    }

    let log_file = log_dir.join("crash.log");
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let log_message = format!("[{}] {}\n", timestamp, message);

    // 写入日志文件
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_file) {
        let _ = file.write_all(log_message.as_bytes());
        let _ = file.flush();
    }

    // 同时输出到stderr
    eprintln!("{}", log_message);
}

// 设置panic hook
fn setup_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "未知位置".to_string());

        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "未知panic原因".to_string()
        };

        let error_msg = format!(
            "应用程序发生严重错误 (panic):\n位置: {}\n错误: {}\n堆栈: {:?}",
            location,
            message,
            std::backtrace::Backtrace::capture()
        );

        write_error_log(&error_msg);
    }));
}

// Define the Tauri command function.
#[tauri::command] // This macro exposes the function to the frontend
fn get_memory_info() -> MemoryInfo {
    // Create a new System instance.
    // `new_all` initializes everything, including CPU list, network list, etc.
    // Use `System::new()` if you only need memory/process/disk info initially.
    let mut sys = System::new_all();

    // Refresh the memory information. It's important to refresh before reading!
    sys.refresh_memory();

    // Get the total and free memory (in bytes).
    // Note: `free_memory` might not include reclaimable memory like caches/buffers on some OSes (like Linux).
    // `available_memory()` often gives a more practical "how much can be used" value on those systems.
    // Stick with `free_memory` to exactly match the frontend example's `free` field.
    let total_memory = sys.total_memory();
    let free_memory = sys.free_memory();

    // Create and return the MemoryInfo struct.
    MemoryInfo {
        total: total_memory,
        free: free_memory,
    }
}

#[tauri::command]
fn quit_app() {
	let _ = RTMP_RELAY.stop_relay();
	std::process::exit(0);
}

#[tauri::command]
fn open_dev_tools(webview_window: tauri::WebviewWindow) {
    webview_window.open_devtools();
}

// 文件服务器相关命令
#[tauri::command]
fn start_file_server() -> Result<FileServerStatus, String> {
    FILE_SERVER.start_server()
}

#[tauri::command]
fn stop_file_server() -> Result<FileServerStatus, String> {
    FILE_SERVER.stop_server()
}

#[tauri::command]
fn update_file_server_config(
    folder_path: Option<String>,
    port: Option<u16>,
) -> Result<FileServerConfig, String> {
    FILE_SERVER.update_config(folder_path, port)
}

#[tauri::command]
fn get_file_server_status() -> FileServerStatus {
    FILE_SERVER.get_status()
}

// 心跳相关命令
#[tauri::command]
fn heartbeat() {
    HEARTBEAT_MONITOR.update_heartbeat();
}

#[tauri::command]
fn get_heartbeat_status() -> HeartbeatStatus {
    HEARTBEAT_MONITOR.get_status()
}

// RTMP转发相关命令
#[tauri::command]
fn start_rtmp_relay(target_url: String) -> Result<RtmpRelayStatus, String> {
    RTMP_RELAY.start_relay(target_url)
}

#[tauri::command]
fn stop_rtmp_relay() -> Result<RtmpRelayStatus, String> {
    RTMP_RELAY.stop_relay()
}

#[tauri::command]
fn get_rtmp_relay_status() -> RtmpRelayStatus {
    RTMP_RELAY.get_status()
}

#[tauri::command]
fn get_rtmp_local_url() -> String {
    RTMP_RELAY.get_local_url()
}

#[tauri::command]
fn update_rtmp_relay_config(
    local_port: Option<u16>,
    target_url: Option<String>,
) -> Result<RtmpRelayConfig, String> {
    RTMP_RELAY.update_config(local_port, target_url)
}

// FFmpeg 状态与下载
#[tauri::command]
fn get_ffmpeg_status() -> FfmpegStatus {
    RTMP_RELAY.get_ffmpeg_status()
}

#[tauri::command]
fn download_ffmpeg() -> Result<FfmpegStatus, String> {
    RTMP_RELAY.download_ffmpeg()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 设置panic hook
    setup_panic_hook();

    // 使用Result来捕获可能的错误
    let result = std::panic::catch_unwind(|| run_app());

    match result {
        Ok(_) => {
            // 正常退出，不记录日志
        }
        Err(e) => {
            let error_msg = if let Some(s) = e.downcast_ref::<&str>() {
                format!("应用程序异常退出: {}", s)
            } else if let Some(s) = e.downcast_ref::<String>() {
                format!("应用程序异常退出: {}", s)
            } else {
                "应用程序异常退出: 未知错误".to_string()
            };
            write_error_log(&error_msg);
            std::process::exit(1);
        }
    }
}

fn run_app() {
    tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_log::Builder::new()
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("logs".to_string()),
                    },
                ))
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Webview,
                ))
                .max_file_size(50_000 /* bytes */)
                .build(),
        )
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = app
                .get_webview_window("main")
                .expect("no main window")
                .set_focus();
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--flag1", "--flag2"]),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            get_memory_info,
            quit_app,
            open_dev_tools,
            start_file_server,
            stop_file_server,
            update_file_server_config,
            get_file_server_status,
            heartbeat,
            get_heartbeat_status,
            start_rtmp_relay,
            stop_rtmp_relay,
            get_rtmp_relay_status,
            get_rtmp_local_url,
            update_rtmp_relay_config,
            get_ffmpeg_status,
            download_ffmpeg,
        ])
        .setup(|app| {
            // 启动心跳监控
            HEARTBEAT_MONITOR.start_monitoring(app.handle().clone());
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(|e| {
            let error_msg = format!("Tauri应用构建失败: {}", e);
            write_error_log(&error_msg);
            e
        })
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            match event {
                tauri::RunEvent::ExitRequested { api, code, .. } => {
                    // 只在异常退出码时记录
                    if let Some(code_val) = code {
                        if code_val != 0 {
                            write_error_log(&format!("应用异常退出请求: code={}", code_val));
                        }
                    }
                    api.prevent_exit();
                }
                _ => {}
            }
        });
}
