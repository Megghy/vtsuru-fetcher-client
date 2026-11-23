use chrono::Local;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

pub const APP_IDENTIFIER: &str = "live.vtsuru.fetcher.client";

/// 统一获取应用数据目录（跨平台，避免回退到相对路径）
pub fn get_app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join(APP_IDENTIFIER);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(APP_IDENTIFIER);
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            return PathBuf::from(xdg).join(APP_IDENTIFIER);
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join(APP_IDENTIFIER);
        }
    }

    std::env::temp_dir().join(APP_IDENTIFIER)
}

/// 统一获取日志目录（优先使用 Tauri path API，与 tauri-plugin-log 对齐）
pub fn get_log_dir(app: Option<&AppHandle>) -> PathBuf {
    if let Some(handle) = app {
        if let Ok(dir) = handle.path().app_log_dir() {
            return dir;
        }
    }
    get_app_data_dir().join("logs")
}

/// 统一获取内置 FFmpeg 存放路径
pub fn get_ffmpeg_local_path() -> PathBuf {
    let bin_dir = get_app_data_dir().join("bin");
    #[cfg(target_os = "windows")]
    {
        bin_dir.join("ffmpeg.exe")
    }
    #[cfg(not(target_os = "windows"))]
    {
        bin_dir.join("ffmpeg")
    }
}

/// 追加写入指定日志文件（用于 crash.log / heartbeat.log）
pub fn append_log_file(app: Option<&AppHandle>, file_name: &str, message: &str) {
    let log_dir = get_log_dir(app);
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let log_message = format!("[{}] {}\n", timestamp, message);

    let result = (|| -> std::io::Result<()> {
        fs::create_dir_all(&log_dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join(file_name))?;
        file.write_all(log_message.as_bytes())?;
        file.flush()
    })();

    if let Err(err) = result {
        eprintln!("写入日志 {file_name} 失败: {err}");
    }
    eprintln!("{}", log_message.trim_end());
}
