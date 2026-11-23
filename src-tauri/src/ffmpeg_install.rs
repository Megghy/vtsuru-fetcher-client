use crate::paths::get_ffmpeg_local_path;
use futures::StreamExt;
use lazy_static::lazy_static;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;

const FFMPEG_DOWNLOAD_URL: &str = "https://files.vtsuru.suki.club/things/ffmpeg.exe";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegStatus {
    pub available: bool,
    pub path: Option<String>,
    pub source: String, // "custom" | "local" | "system" | "none"
    pub version: Option<String>,
    pub custom_path: Option<String>,
    pub can_auto_download: bool,
    pub downloading: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegDownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub percent: Option<f64>,
}

pub struct FfmpegInstallManager {
    custom_path: RwLock<Option<PathBuf>>,
    downloading: AtomicBool,
}

lazy_static! {
    pub static ref FFMPEG_INSTALL: FfmpegInstallManager = FfmpegInstallManager::new();
}

impl FfmpegInstallManager {
    fn new() -> Self {
        Self {
            custom_path: RwLock::new(None),
            downloading: AtomicBool::new(false),
        }
    }

    pub fn set_custom_path(&self, path: Option<String>) -> Result<FfmpegStatus, String> {
        let trimmed = path
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());

        if let Some(ref p) = trimmed {
            let pb = PathBuf::from(p);
            if !pb.exists() {
                return Err(format!("指定的 FFmpeg 文件不存在: {p}"));
            }
            probe_ffmpeg_version(p)
                .map_err(|e| format!("指定的路径不是有效的 FFmpeg 可执行文件: {e}"))?;
            *self.custom_path.write() = Some(pb);
        } else {
            *self.custom_path.write() = None;
        }

        Ok(self.get_status())
    }

    /// 按优先级解析可用 FFmpeg 可执行路径: 自定义路径 > 内置路径 > 系统 PATH
    pub fn resolve_executable(&self) -> Result<(String, &'static str), String> {
        if let Some(custom) = self.custom_path.read().clone() {
            if custom.exists() {
                return Ok((custom.to_string_lossy().into_owned(), "custom"));
            }
        }

        let local = get_ffmpeg_local_path();
        if local.exists() {
            return Ok((local.to_string_lossy().into_owned(), "local"));
        }

        if probe_ffmpeg_version("ffmpeg").is_ok() {
            return Ok(("ffmpeg".to_string(), "system"));
        }

        Err("未找到 FFmpeg，请在客户端内下载或指定本地 FFmpeg 路径".to_string())
    }

    pub fn get_status(&self) -> FfmpegStatus {
        let custom_path = self
            .custom_path
            .read()
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let downloading = self.downloading.load(Ordering::SeqCst);
        let can_auto_download = cfg!(target_os = "windows");

        match self.resolve_executable() {
            Ok((exe, source)) => {
                let version = probe_ffmpeg_version(&exe).ok();
                FfmpegStatus {
                    available: version.is_some(),
                    path: Some(exe),
                    source: source.to_string(),
                    version,
                    custom_path,
                    can_auto_download,
                    downloading,
                }
            }
            Err(_) => FfmpegStatus {
                available: false,
                path: None,
                source: "none".to_string(),
                version: None,
                custom_path,
                can_auto_download,
                downloading,
            },
        }
    }

    pub async fn download(&self, app: AppHandle) -> Result<FfmpegStatus, String> {
        if !cfg!(target_os = "windows") {
            return Err("当前平台暂不支持自动下载 FFmpeg，请通过系统包管理器安装（如 brew/apt install ffmpeg）或手动指定路径".to_string());
        }

        if self
            .downloading
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("FFmpeg 正在下载中，请勿重复操作".to_string());
        }

        struct ResetGuard<'a>(&'a AtomicBool);
        impl Drop for ResetGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _guard = ResetGuard(&self.downloading);

        let target_path = get_ffmpeg_local_path();
        if let Some(parent) = target_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("创建 FFmpeg 目录失败: {e}"))?;
        }

        let tmp_path = target_path.with_extension("exe.download");
        let _ = tokio::fs::remove_file(&tmp_path).await;

        log::info!(
            "[FFmpeg] 开始从 {} 下载至 {}",
            FFMPEG_DOWNLOAD_URL,
            target_path.to_string_lossy()
        );

        let response = reqwest::get(FFMPEG_DOWNLOAD_URL)
            .await
            .map_err(|e| format!("请求 FFmpeg 下载失败: {e}"))?;

        if !response.status().is_success() {
            return Err(format!("下载 FFmpeg 失败: HTTP {}", response.status()));
        }

        let total = response.content_length();
        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| format!("创建临时文件失败: {e}"))?;

        let mut downloaded: u64 = 0;
        let mut last_emitted_pct: i64 = -1;
        let mut stream = response.bytes_stream();

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(format!("下载 FFmpeg 数据流中断: {e}"));
                }
            };
            if let Err(e) = file.write_all(&chunk).await {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(format!("写入临时文件失败: {e}"));
            }
            downloaded += chunk.len() as u64;

            let percent = total.filter(|&t| t > 0).map(|t| (downloaded as f64 / t as f64) * 100.0);
            let bucket = percent.map(|p| p.floor() as i64).unwrap_or(-1);
            if bucket != last_emitted_pct || total.is_none() {
                last_emitted_pct = bucket;
                let _ = app.emit(
                    "ffmpeg://download-progress",
                    FfmpegDownloadProgress {
                        downloaded,
                        total,
                        percent,
                    },
                );
            }
        }

        if let Err(e) = file.flush().await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(format!("刷新文件缓存失败: {e}"));
        }
        drop(file);

        // 验证下载的二进制可执行
        if let Err(e) = verify_executable(&tmp_path) {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(format!("下载的 FFmpeg 校验失败: {e}"));
        }

        if target_path.exists() {
            let _ = tokio::fs::remove_file(&target_path).await;
        }
        tokio::fs::rename(&tmp_path, &target_path)
            .await
            .map_err(|e| format!("替换 FFmpeg 文件失败: {e}"))?;

        log::info!("[FFmpeg] 下载并校验完成: {}", target_path.to_string_lossy());
        Ok(self.get_status())
    }
}

fn verify_executable(path: &Path) -> Result<String, String> {
    probe_ffmpeg_version(&path.to_string_lossy())
}

fn probe_ffmpeg_version(exe: &str) -> Result<String, String> {
    let mut cmd = Command::new(exe);
    cmd.arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("无法执行 {exe}: {e}"))?;

    if !output.status.success() {
        return Err(format!("退出码异常: {:?}", output.status.code()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or("").trim().to_string();
    if first_line.is_empty() {
        Err("输出为空".to_string())
    } else {
        Ok(first_line)
    }
}

#[tauri::command]
pub fn get_ffmpeg_status() -> FfmpegStatus {
    FFMPEG_INSTALL.get_status()
}

#[tauri::command]
pub fn set_custom_ffmpeg_path(path: Option<String>) -> Result<FfmpegStatus, String> {
    FFMPEG_INSTALL.set_custom_path(path)
}

#[tauri::command]
pub async fn download_ffmpeg(app: AppHandle) -> Result<FfmpegStatus, String> {
    FFMPEG_INSTALL.download(app).await
}
