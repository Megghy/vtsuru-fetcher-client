use crate::ffmpeg_install::{FfmpegStatus, FFMPEG_INSTALL};
use crate::ffmpeg_jobs::FFMPEG_JOBS;
use crate::heartbeat::{HeartbeatStatus, HEARTBEAT_MONITOR};
use crate::paths::get_log_dir;
use crate::rpc_server::{RpcServerStatus, RPC_SERVER};
use serde::Serialize;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::time::UNIX_EPOCH;
use sysinfo::System;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogFileMeta {
    pub name: String,
    pub size_bytes: u64,
    pub modified_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticLogFile {
    pub name: String,
    pub truncated: bool,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsInfo {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub autostart_launch: bool,
    pub log_dir: String,
    pub log_files: Vec<LogFileMeta>,
    pub rpc: RpcServerStatus,
    pub heartbeat: HeartbeatStatus,
    pub ffmpeg: FfmpegStatus,
    pub active_ffmpeg_jobs: usize,
    pub memory_total: u64,
    pub memory_free: u64,
}

pub fn is_autostart_arg_present() -> bool {
    std::env::args().any(|arg| arg == "--autostart")
}

/// 对日志中的常见敏感字段做基础脱敏（Cookie、Token、Authorization）
pub fn redact_sensitive_text(input: &str) -> String {
    const SENSITIVE_KEYS: &[&str] = &[
        "SESSDATA=",
        "bili_jct=",
        "DedeUserID__ckMd5=",
        "ac_time_value=",
        "refresh_token=",
        "Authorization: Bearer ",
        "authorization: bearer ",
        "\"token\":\"",
        "\"cookie\":\"",
        "\"password\":\"",
        "token=",
    ];

    let mut result = String::with_capacity(input.len());
    for line in input.lines() {
        let mut sanitized = line.to_string();
        for key in SENSITIVE_KEYS {
            let mut search_from = 0;
            while let Some(pos) = sanitized[search_from..].find(key) {
                let start = search_from + pos + key.len();
                let end = sanitized[start..]
                    .find(|c: char| matches!(c, ';' | '&' | '"' | '\'' | ' ' | '\r' | '\n' | ',' | '}'))
                    .map(|offset| start + offset)
                    .unwrap_or(sanitized.len());
                sanitized.replace_range(start..end, "[REDACTED]");
                search_from = start + "[REDACTED]".len();
            }
        }
        result.push_str(&sanitized);
        result.push('\n');
    }
    result
}

#[tauri::command]
pub fn get_diagnostics_info(app: AppHandle) -> DiagnosticsInfo {
    let log_dir = get_log_dir(Some(&app));
    let mut log_files = Vec::new();

    if let Ok(entries) = fs::read_dir(&log_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(meta) = entry.metadata() {
                    let modified_at = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    log_files.push(LogFileMeta {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        size_bytes: meta.len(),
                        modified_at,
                    });
                }
            }
        }
    }
    log_files.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));

    let mut sys = System::new();
    sys.refresh_memory();

    DiagnosticsInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        autostart_launch: is_autostart_arg_present(),
        log_dir: log_dir.to_string_lossy().into_owned(),
        log_files,
        rpc: RPC_SERVER.get_status(),
        heartbeat: HEARTBEAT_MONITOR.get_status(),
        ffmpeg: FFMPEG_INSTALL.get_status(),
        active_ffmpeg_jobs: FFMPEG_JOBS.list().len(),
        memory_total: sys.total_memory(),
        memory_free: sys.free_memory(),
    }
}

#[tauri::command]
pub fn open_log_dir(app: AppHandle) -> Result<(), String> {
    let log_dir = get_log_dir(Some(&app));
    fs::create_dir_all(&log_dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
    app.opener()
        .open_path(log_dir.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| format!("打开日志目录失败: {e}"))
}

#[tauri::command]
pub fn read_diagnostic_logs(
    app: AppHandle,
    max_bytes_per_file: Option<usize>,
) -> Result<Vec<DiagnosticLogFile>, String> {
    let limit = max_bytes_per_file.unwrap_or(512 * 1024).min(2 * 1024 * 1024);
    let log_dir = get_log_dir(Some(&app));
    if !log_dir.exists() {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();
    let entries = fs::read_dir(&log_dir).map_err(|e| format!("读取日志目录失败: {e}"))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".log") {
            continue;
        }

        let Ok(mut file) = fs::File::open(&path) else {
            continue;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0) as usize;
        let truncated = len > limit;
        if truncated {
            let _ = file.seek(SeekFrom::End(-(limit as i64)));
        }
        let mut buf = Vec::with_capacity(len.min(limit));
        if file.read_to_end(&mut buf).is_ok() {
            let raw = String::from_utf8_lossy(&buf);
            results.push(DiagnosticLogFile {
                name,
                truncated,
                content: redact_sensitive_text(&raw),
            });
        }
    }

    results.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::redact_sensitive_text;

    #[test]
    fn redacts_common_credentials() {
        let sample = "Cookie: SESSDATA=abc123; bili_jct=xyz789; foo=bar\nAuthorization: Bearer secret.jwt.token\n{\"token\":\"t_12345\"}";
        let out = redact_sensitive_text(sample);
        assert!(!out.contains("abc123"));
        assert!(!out.contains("xyz789"));
        assert!(!out.contains("secret.jwt.token"));
        assert!(!out.contains("t_12345"));
        assert!(out.contains("foo=bar"));
        assert!(out.contains("SESSDATA=[REDACTED]"));
    }
}
