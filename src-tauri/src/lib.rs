#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use serde::Serialize;
use sysinfo::System;

mod diagnostics;
mod ffmpeg_install;
mod ffmpeg_jobs;
mod heartbeat;
mod paths;
mod rpc_server;
mod tray;

use diagnostics::{
    get_diagnostics_info, is_autostart_arg_present, open_log_dir, read_diagnostic_logs,
};
use ffmpeg_install::{download_ffmpeg, get_ffmpeg_status, set_custom_ffmpeg_path};
use ffmpeg_jobs::{
    get_ffmpeg_job, list_ffmpeg_jobs, spawn_ffmpeg_job, stop_ffmpeg_job, FFMPEG_JOBS,
};
use heartbeat::{HeartbeatStatus, HEARTBEAT_MONITOR};
use paths::append_log_file;
use rpc_server::{RpcServerStatus, RPC_SERVER};
use tray::{reload_main_window_internal, setup_native_tray, show_main_window};

#[derive(Serialize, Clone)]
struct MemoryInfo {
    total: u64,
    free: u64,
}

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

        append_log_file(None, "crash.log", &error_msg);
    }));
}

#[tauri::command]
fn get_memory_info() -> MemoryInfo {
    let mut sys = System::new();
    sys.refresh_memory();
    MemoryInfo {
        total: sys.total_memory(),
        free: sys.free_memory(),
    }
}

#[tauri::command]
async fn quit_app(app: tauri::AppHandle) {
    FFMPEG_JOBS.stop_all().await;
    app.exit(0);
}

#[tauri::command]
fn open_dev_tools(webview_window: tauri::WebviewWindow) {
    webview_window.open_devtools();
}

#[tauri::command]
fn reload_main_window(app: tauri::AppHandle) {
    reload_main_window_internal(&app);
}

#[tauri::command]
fn is_autostart_launch() -> bool {
    is_autostart_arg_present()
}

/// 返回主窗口配置的 additionalBrowserArgs，确保动态创建子 WebView2 时与主窗口环境参数完全一致（避免 Windows 0x8007139F）
#[tauri::command]
fn get_webview_browser_args(app: tauri::AppHandle) -> Option<String> {
    app.config()
        .app
        .windows
        .first()
        .and_then(|w| w.additional_browser_args.clone())
}

#[tauri::command]
fn heartbeat() {
    HEARTBEAT_MONITOR.update_heartbeat();
}

#[tauri::command]
fn get_heartbeat_status() -> HeartbeatStatus {
    HEARTBEAT_MONITOR.get_status()
}

#[tauri::command]
fn set_heartbeat_auto_reload(enabled: bool) {
    HEARTBEAT_MONITOR.set_auto_reload(enabled);
}

#[tauri::command]
fn rpc_send(conn_id: String, data: String) -> Result<(), String> {
    RPC_SERVER.send(&conn_id, data)
}

#[tauri::command]
fn rpc_close(conn_id: String) -> Result<(), String> {
    RPC_SERVER.close(&conn_id)
}

#[tauri::command]
fn get_rpc_server_status() -> RpcServerStatus {
    RPC_SERVER.get_status()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    setup_panic_hook();

    let result = std::panic::catch_unwind(run_app);
    if let Err(e) = result {
        let error_msg = if let Some(s) = e.downcast_ref::<&str>() {
            format!("应用程序异常退出: {}", s)
        } else if let Some(s) = e.downcast_ref::<String>() {
            format!("应用程序异常退出: {}", s)
        } else {
            "应用程序异常退出: 未知错误".to_string()
        };
        append_log_file(None, "crash.log", &error_msg);
        std::process::exit(1);
    }
}

fn run_app() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_denylist(&["danmaku-window", "gift-window"])
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .build(),
        )
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
                .max_file_size(5 * 1024 * 1024)
                .level(log::LevelFilter::Info)
                .level_for("hyper", log::LevelFilter::Warn)
                .level_for("hyper_util", log::LevelFilter::Warn)
                .level_for("tungstenite", log::LevelFilter::Warn)
                .level_for("tower_http", log::LevelFilter::Warn)
                .level_for("reqwest", log::LevelFilter::Warn)
                .build(),
        )
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            get_memory_info,
            quit_app,
            open_dev_tools,
            reload_main_window,
            is_autostart_launch,
            get_webview_browser_args,
            heartbeat,
            get_heartbeat_status,
            set_heartbeat_auto_reload,
            get_ffmpeg_status,
            set_custom_ffmpeg_path,
            download_ffmpeg,
            rpc_send,
            rpc_close,
            get_rpc_server_status,
            spawn_ffmpeg_job,
            stop_ffmpeg_job,
            get_ffmpeg_job,
            list_ffmpeg_jobs,
            get_diagnostics_info,
            open_log_dir,
            read_diagnostic_logs,
        ])
        .setup(|app| {
            if let Err(err) = setup_native_tray(app.handle()) {
                log::error!("[Tray] 创建系统托盘失败: {err}");
            }
            HEARTBEAT_MONITOR.start_monitoring(app.handle().clone());
            RPC_SERVER.start(app.handle().clone());
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(|e| {
            let error_msg = format!("Tauri应用构建失败: {}", e);
            append_log_file(None, "crash.log", &error_msg);
            e
        })
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if let Some(code_val) = code {
                    if code_val != 0 {
                        append_log_file(
                            None,
                            "crash.log",
                            &format!("应用异常退出请求: code={}", code_val),
                        );
                    }
                    return;
                }
                api.prevent_exit();
            }
        });
}
