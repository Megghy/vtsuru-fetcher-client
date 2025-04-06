#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]
use tauri::Manager;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/

// Import necessary items
use serde::Serialize;
use sysinfo::System;

// Define a struct to represent the data we want to send to the frontend.
// It needs `Serialize` to be convertible to JSON.
#[derive(Serialize, Clone)] // Clone is useful if you might pass this around
struct MemoryInfo {
    total: u64, // Use u64 for byte counts, which can be large
    free: u64,
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
    std::process::exit(0);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_log::Builder::new().build())
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
        .invoke_handler(tauri::generate_handler![get_memory_info, quit_app])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
