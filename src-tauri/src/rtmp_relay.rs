use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::process::{Child, Command, Stdio};
use std::path::PathBuf;
use std::fs;
use std::io::{BufRead, BufReader};
use std::thread;
use lazy_static::lazy_static;
use log::{error, info, warn};

lazy_static! {
    pub static ref RTMP_RELAY: RtmpRelay = RtmpRelay::new();
}

/// 获取本地 ffmpeg.exe 的预期路径
/// Windows 下使用 %APPDATA%/live.vtsuru.fetcher.client/bin/ffmpeg.exe
/// 其他平台暂时返回当前工作目录下的 ffmpeg，可根据需要扩展
pub fn get_ffmpeg_local_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base_dir = std::env::var("APPDATA")
            .map(|appdata| {
                PathBuf::from(appdata)
                    .join("live.vtsuru.fetcher.client")
                    .join("bin")
            })
            .unwrap_or_else(|_| PathBuf::from("./bin"));

        base_dir.join("ffmpeg.exe")
    }

    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("ffmpeg")
    }
}

const FFMPEG_DOWNLOAD_URL: &str = "https://files.vtsuru.suki.club/things/ffmpeg.exe";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtmpRelayConfig {
    pub local_port: u16,
    pub target_url: Option<String>,
}

impl Default for RtmpRelayConfig {
    fn default() -> Self {
        Self {
            local_port: 1935,
            target_url: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtmpRelayStatus {
    pub is_running: bool,
    pub local_port: u16,
    pub target_url: Option<String>,
    pub is_relaying: bool,
    pub bitrate_kbps: Option<f64>,
    pub speed: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FfmpegStatus {
    pub available: bool,
    pub source: Option<String>,
    pub path: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
}

struct RtmpRelayState {
    config: RtmpRelayConfig,
    ffmpeg_process: Option<Child>,
    is_relaying: bool,
    bitrate_kbps: Option<f64>,
    speed: Option<f64>,
}

pub struct RtmpRelay {
    state: Arc<RwLock<RtmpRelayState>>,
}

impl RtmpRelay {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(RtmpRelayState {
                config: RtmpRelayConfig::default(),
                ffmpeg_process: None,
                is_relaying: false,
                bitrate_kbps: None,
                speed: None,
            })),
        }
    }

    /// 更新配置
    pub fn update_config(&self, local_port: Option<u16>, target_url: Option<String>) -> Result<RtmpRelayConfig, String> {
        let mut state = self.state.write();
        
        if let Some(port) = local_port {
            state.config.local_port = port;
        }
        
        if let Some(url) = target_url {
            state.config.target_url = Some(url);
        }
        
        info!("[RTMP Relay] 配置已更新: {:?}", state.config);
        Ok(state.config.clone())
    }

    /// 开始转发
    pub fn start_relay(&self, target_url: String) -> Result<RtmpRelayStatus, String> {
        let mut state = self.state.write();
        
        // 如果已经在转发，先停止
        if state.is_relaying {
            warn!("[RTMP Relay] 正在转发中，先停止当前转发");
            self.stop_relay_internal(&mut state)?;
        }

        // 更新目标URL
        state.config.target_url = Some(target_url.clone());

        let local_url = format!("rtmp://127.0.0.1:{}/live/stream", state.config.local_port);
        
        info!("[RTMP Relay] 开始RTMP转发");
        info!("[RTMP Relay] 本地地址: {}", local_url);
        info!("[RTMP Relay] 目标地址: {}", target_url);

        // 使用 FFmpeg 进行 RTMP 转发
        // 以本地 RTMP 服务器模式监听来自 OBS 的推流，再转发到目标地址
        // ffmpeg -listen 1 -i rtmp://localhost:1935/live/stream -c copy -f flv rtmp://target_url

        // 优先尝试系统 PATH 中的 ffmpeg，不存在时回退到本地下载的 ffmpeg.exe
        let local_ffmpeg = get_ffmpeg_local_path();
        let mut candidates: Vec<String> = vec!["ffmpeg".to_string()];
        if local_ffmpeg.exists() {
            candidates.push(local_ffmpeg.to_string_lossy().to_string());
        }

        let mut last_error: Option<String> = None;

        for exe in candidates {
            info!("[RTMP Relay] 尝试使用 FFmpeg: {}", exe);
            match Command::new(&exe)
                .args(&[
                    "-listen",
                    "1",
                    "-i",
                    &local_url,
                    "-c:v",
                    "copy",
                    "-c:a",
                    "copy",
                    "-f",
                    "flv",
                    &target_url,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    let stderr = child.stderr.take();
                    state.ffmpeg_process = Some(child);
                    state.is_relaying = true;
                    state.bitrate_kbps = None;
                    state.speed = None;

                    if let Some(stderr) = stderr {
                        let state_arc = self.state.clone();
                        thread::spawn(move || {
                            let reader = BufReader::new(stderr);
                            for line_result in reader.lines() {
                                let line = match line_result {
                                    Ok(l) => l,
                                    Err(e) => {
                                        warn!("[RTMP Relay] 读取 FFmpeg 日志失败: {}", e);
                                        break;
                                    }
                                };

                                let mut bitrate: Option<f64> = None;
                                let mut speed: Option<f64> = None;

                                if let Some(idx) = line.find("bitrate=") {
                                    let rest = &line[idx + "bitrate=".len()..];
                                    if let Some(token) = rest.split_whitespace().next() {
                                        let value_str = token.trim_end_matches("kbits/s");
                                        if let Ok(v) = value_str.trim().parse::<f64>() {
                                            bitrate = Some(v);
                                        }
                                    }
                                }

                                if let Some(idx) = line.find("speed=") {
                                    let rest = &line[idx + "speed=".len()..];
                                    if let Some(token) = rest.split_whitespace().next() {
                                        let value_str = token.trim_end_matches('x');
                                        if let Ok(v) = value_str.trim().parse::<f64>() {
                                            speed = Some(v);
                                        }
                                    }
                                }

                                if bitrate.is_some() || speed.is_some() {
                                    let mut state = state_arc.write();
                                    if let Some(b) = bitrate {
                                        state.bitrate_kbps = Some(b);
                                    }
                                    if let Some(s) = speed {
                                        state.speed = Some(s);
                                    }
                                }
                            }
                        });
                    }
                    info!("[RTMP Relay] FFmpeg 进程已启动");
                    return Ok(self.get_status_internal(&state));
                }
                Err(e) => {
                    warn!("[RTMP Relay] 使用 {} 启动 FFmpeg 失败: {}", exe, e);
                    last_error = Some(format!("{}: {}", exe, e));
                }
            }
        }

        error!("[RTMP Relay] 启动 FFmpeg 失败: {:?}", last_error);
        Err(format!(
            "启动 FFmpeg 失败: {}. 请确保已安装 FFmpeg 或在客户端中自动下载",
            last_error.unwrap_or_else(|| "未知错误".to_string())
        ))
    }

    /// 停止转发
    pub fn stop_relay(&self) -> Result<RtmpRelayStatus, String> {
        let mut state = self.state.write();
        self.stop_relay_internal(&mut state)?;
        Ok(self.get_status_internal(&state))
    }

    /// 内部停止转发方法
    fn stop_relay_internal(&self, state: &mut RtmpRelayState) -> Result<(), String> {
        if let Some(mut child) = state.ffmpeg_process.take() {
            info!("[RTMP Relay] 正在停止 FFmpeg 进程...");
            match child.kill() {
                Ok(_) => {
                    let _ = child.wait();
                    info!("[RTMP Relay] FFmpeg 进程已停止");
                }
                Err(e) => {
                    error!("[RTMP Relay] 停止 FFmpeg 进程失败: {}", e);
                    return Err(format!("停止 FFmpeg 进程失败: {}", e));
                }
            }
        }
        
        state.is_relaying = false;
        state.bitrate_kbps = None;
        state.speed = None;
        Ok(())
    }

    /// 获取状态
    pub fn get_status(&self) -> RtmpRelayStatus {
        let state = self.state.read();
        self.get_status_internal(&state)
    }

    /// 内部获取状态方法
    fn get_status_internal(&self, state: &RtmpRelayState) -> RtmpRelayStatus {
        RtmpRelayStatus {
            is_running: true, // 服务本身一直在运行
            local_port: state.config.local_port,
            target_url: state.config.target_url.clone(),
            is_relaying: state.is_relaying,
            bitrate_kbps: state.bitrate_kbps,
            speed: state.speed,
        }
    }

    /// 获取本地推流地址
    pub fn get_local_url(&self) -> String {
        let state = self.state.read();
        format!("rtmp://127.0.0.1:{}/live/stream", state.config.local_port)
    }

    /// 获取 FFmpeg 状态（路径、版本等）
    pub fn get_ffmpeg_status(&self) -> FfmpegStatus {
        let local_ffmpeg = get_ffmpeg_local_path();
        let mut last_error: Option<String> = None;

        let mut candidates: Vec<(String, &str)> = vec![("ffmpeg".to_string(), "system")];
        if local_ffmpeg.exists() {
            candidates.push((local_ffmpeg.to_string_lossy().to_string(), "local"));
        }

        for (exe, source) in candidates {
            match get_ffmpeg_version(&exe) {
                Ok(version) => {
                    return FfmpegStatus {
                        available: true,
                        source: Some(source.to_string()),
                        path: Some(exe),
                        version: Some(version),
                        error: None,
                    };
                }
                Err(e) => {
                    warn!("[RTMP Relay] 检测 FFmpeg 失败 ({}): {}", source, e);
                    last_error = Some(e);
                }
            }
        }

        FfmpegStatus {
            available: false,
            source: None,
            path: if local_ffmpeg.exists() {
                Some(local_ffmpeg.to_string_lossy().to_string())
            } else {
                None
            },
            version: None,
            error: last_error.or_else(|| Some("未检测到可用的 FFmpeg".to_string())),
        }
    }

    /// 下载内置 FFmpeg 并返回最新状态
    pub fn download_ffmpeg(&self) -> Result<FfmpegStatus, String> {
        let local_path = get_ffmpeg_local_path();
        if !local_path.exists() {
            download_ffmpeg_binary()?;
        }
        Ok(self.get_ffmpeg_status())
    }
}

impl Drop for RtmpRelay {
    fn drop(&mut self) {
        let mut state = self.state.write();
        let _ = self.stop_relay_internal(&mut state);
    }
}

fn get_ffmpeg_version(exe: &str) -> Result<String, String> {
    match Command::new(exe)
        .arg("-version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => {
            if !output.status.success() {
                return Err(format!("ffmpeg -version 退出失败: {:?}", output.status));
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let first_line = stdout.lines().next().unwrap_or("").trim().to_string();
            if first_line.is_empty() {
                Err("无法解析 FFmpeg 版本信息".to_string())
            } else {
                Ok(first_line)
            }
        }
        Err(e) => Err(format!("执行 ffmpeg -version 失败: {}", e)),
    }
}

fn download_ffmpeg_binary() -> Result<PathBuf, String> {
    let path = get_ffmpeg_local_path();

    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return Err(format!("创建 FFmpeg 目录失败: {}", e));
        }
    }

    info!(
        "[RTMP Relay] 正在从 {} 下载 FFmpeg 到 {}",
        FFMPEG_DOWNLOAD_URL,
        path.to_string_lossy()
    );

    let resp = reqwest::blocking::get(FFMPEG_DOWNLOAD_URL)
        .map_err(|e| format!("下载 FFmpeg 失败: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("下载 FFmpeg 失败: HTTP {}", resp.status()));
    }

    let bytes = resp
        .bytes()
        .map_err(|e| format!("读取 FFmpeg 数据失败: {}", e))?;

    if let Err(e) = fs::write(&path, &bytes) {
        return Err(format!("保存 FFmpeg 到本地失败: {}", e));
    }

    info!(
        "[RTMP Relay] FFmpeg 已下载到 {}",
        path.to_string_lossy()
    );

    Ok(path)
}
