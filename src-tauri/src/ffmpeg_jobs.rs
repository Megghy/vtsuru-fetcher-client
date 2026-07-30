use crate::rtmp_relay::get_ffmpeg_local_path;
use lazy_static::lazy_static;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MEDIA_WS_BASE: &str = "ws://127.0.0.1:29304/media/jobs";

lazy_static! {
    pub static ref FFMPEG_JOBS: FfmpegJobManager = FfmpegJobManager::new();
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipeMode {
    Null,
    Pipe,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnFfmpegJobRequest {
    pub label: Option<String>,
    pub args: Vec<String>,
    pub stdin: PipeMode,
    pub stdout: PipeMode,
    pub stderr: PipeMode,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FfmpegJobState {
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegJobStatus {
    pub id: String,
    pub label: Option<String>,
    pub state: FfmpegJobState,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnFfmpegJobResponse {
    pub job: FfmpegJobStatus,
    pub token: String,
    pub ws_base_url: String,
    pub has_stdin: bool,
    pub has_stdout: bool,
    pub has_stderr: bool,
}

struct FfmpegJob {
    token: String,
    status: Arc<RwLock<FfmpegJobStatus>>,
    cancel: CancellationToken,
    stdin: Option<mpsc::Sender<Vec<u8>>>,
    stdout: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    stderr: Mutex<Option<mpsc::Receiver<String>>>,
}

pub struct FfmpegJobManager {
    jobs: RwLock<HashMap<String, Arc<FfmpegJob>>>,
}

impl FfmpegJobManager {
    fn new() -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
        }
    }

    pub async fn spawn(
        &self,
        request: SpawnFfmpegJobRequest,
    ) -> Result<SpawnFfmpegJobResponse, String> {
        if request.args.is_empty() {
            return Err("FFmpeg 参数不能为空".to_string());
        }
        if request.args.iter().any(|arg| arg.contains('\0')) {
            return Err("FFmpeg 参数包含空字符".to_string());
        }

        self.jobs
            .write()
            .retain(|_, job| job.status.read().state == FfmpegJobState::Running);

        let stdin_mode = request.stdin;
        let stdout_mode = request.stdout;
        let stderr_mode = request.stderr;
        let mut command = create_command(&request.args, stdin_mode, stdout_mode, stderr_mode)?;
        let mut child = command
            .spawn()
            .map_err(|error| format!("启动 FFmpeg 失败: {error}"))?;

        let id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string();
        let status = Arc::new(RwLock::new(FfmpegJobStatus {
            id: id.clone(),
            label: request.label,
            state: FfmpegJobState::Running,
            started_at: chrono::Utc::now().timestamp(),
            ended_at: None,
            exit_code: None,
            message: None,
        }));
        let cancel = CancellationToken::new();

        let stdin = if stdin_mode == PipeMode::Pipe {
            let mut child_stdin = child
                .stdin
                .take()
                .ok_or_else(|| "FFmpeg stdin 不可用".to_string())?;
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
            let task_cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = task_cancel.cancelled() => break,
                        chunk = rx.recv() => {
                            let Some(chunk) = chunk else { break };
                            if child_stdin.write_all(&chunk).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            Some(tx)
        } else {
            None
        };

        let stdout = if stdout_mode == PipeMode::Pipe {
            let child_stdout = child
                .stdout
                .take()
                .ok_or_else(|| "FFmpeg stdout 不可用".to_string())?;
            let (tx, rx) = mpsc::channel(16);
            tokio::spawn(read_binary_stream(child_stdout, tx));
            Some(rx)
        } else {
            None
        };

        let last_stderr = Arc::new(RwLock::new(None));
        let stderr = if stderr_mode == PipeMode::Pipe {
            let child_stderr = child
                .stderr
                .take()
                .ok_or_else(|| "FFmpeg stderr 不可用".to_string())?;
            let (tx, rx) = mpsc::channel(256);
            tokio::spawn(read_text_stream(child_stderr, tx, last_stderr.clone()));
            Some(rx)
        } else {
            None
        };

        let job = Arc::new(FfmpegJob {
            token: token.clone(),
            status: status.clone(),
            cancel: cancel.clone(),
            stdin,
            stdout: Mutex::new(stdout),
            stderr: Mutex::new(stderr),
        });
        self.jobs.write().insert(id.clone(), job);

        let task_status = status.clone();
        tokio::spawn(async move {
            let stopped = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = child.start_kill();
                    true
                }
                result = child.wait() => {
                    finish_status(&task_status, result, false, &last_stderr);
                    return;
                }
            };
            let result = child.wait().await;
            finish_status(&task_status, result, stopped, &last_stderr);
        });

        let job_status = status.read().clone();
        Ok(SpawnFfmpegJobResponse {
            job: job_status,
            token,
            ws_base_url: format!("{MEDIA_WS_BASE}/{id}"),
            has_stdin: stdin_mode == PipeMode::Pipe,
            has_stdout: stdout_mode == PipeMode::Pipe,
            has_stderr: stderr_mode == PipeMode::Pipe,
        })
    }

    pub async fn stop(&self, id: &str) -> Result<FfmpegJobStatus, String> {
        let job = self.job(id)?;
        job.cancel.cancel();
        for _ in 0..200 {
            let status = job.status.read().clone();
            if status.state != FfmpegJobState::Running {
                return Ok(status);
            }
            sleep(Duration::from_millis(50)).await;
        }
        Err("等待 FFmpeg 停止超时".to_string())
    }

    pub fn get(&self, id: &str) -> Result<FfmpegJobStatus, String> {
        Ok(self.job(id)?.status.read().clone())
    }

    pub fn list(&self) -> Vec<FfmpegJobStatus> {
        self.jobs
            .read()
            .values()
            .map(|job| job.status.read().clone())
            .collect()
    }

    pub async fn stop_all(&self) {
        let ids = self.jobs.read().keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let _ = self.stop(&id).await;
        }
    }

    pub fn stdin(&self, id: &str, token: &str) -> Result<mpsc::Sender<Vec<u8>>, String> {
        self.authorized_job(id, token)?
            .stdin
            .clone()
            .ok_or_else(|| "此 FFmpeg 任务未启用 stdin 管道".to_string())
    }

    pub fn take_stdout(&self, id: &str, token: &str) -> Result<mpsc::Receiver<Vec<u8>>, String> {
        self.authorized_job(id, token)?
            .stdout
            .lock()
            .take()
            .ok_or_else(|| "FFmpeg stdout 管道不存在或已被连接".to_string())
    }

    pub fn take_stderr(&self, id: &str, token: &str) -> Result<mpsc::Receiver<String>, String> {
        self.authorized_job(id, token)?
            .stderr
            .lock()
            .take()
            .ok_or_else(|| "FFmpeg stderr 管道不存在或已被连接".to_string())
    }

    fn job(&self, id: &str) -> Result<Arc<FfmpegJob>, String> {
        self.jobs
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| "FFmpeg 任务不存在".to_string())
    }

    fn authorized_job(&self, id: &str, token: &str) -> Result<Arc<FfmpegJob>, String> {
        let job = self.job(id)?;
        if job.token != token {
            return Err("FFmpeg 任务凭据无效".to_string());
        }
        Ok(job)
    }
}

fn create_command(
    args: &[String],
    stdin: PipeMode,
    stdout: PipeMode,
    stderr: PipeMode,
) -> Result<Command, String> {
    let local_ffmpeg = get_ffmpeg_local_path();
    let executable = if command_exists("ffmpeg") {
        "ffmpeg".to_string()
    } else if local_ffmpeg.exists() {
        local_ffmpeg.to_string_lossy().into_owned()
    } else {
        return Err("未找到 FFmpeg，请先通过客户端安装".to_string());
    };

    let mut command = Command::new(executable);
    command.args(args).kill_on_drop(true);
    command.stdin(stdio(stdin));
    command.stdout(stdio(stdout));
    command.stderr(stdio(stderr));
    Ok(command)
}

fn stdio(mode: PipeMode) -> Stdio {
    match mode {
        PipeMode::Null => Stdio::null(),
        PipeMode::Pipe => Stdio::piped(),
    }
}

fn command_exists(executable: &str) -> bool {
    std::process::Command::new(executable)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

async fn read_binary_stream(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    tx: mpsc::Sender<Vec<u8>>,
) {
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let Ok(size) = stream.read(&mut buffer).await else {
            return;
        };
        if size == 0 || tx.send(buffer[..size].to_vec()).await.is_err() {
            return;
        }
    }
}

async fn read_text_stream(
    stream: impl tokio::io::AsyncRead + Unpin,
    tx: mpsc::Sender<String>,
    last_line: Arc<RwLock<Option<String>>>,
) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        *last_line.write() = Some(line.clone());
        let _ = tx.try_send(line);
    }
}

fn finish_status(
    status: &Arc<RwLock<FfmpegJobStatus>>,
    result: std::io::Result<std::process::ExitStatus>,
    stopped: bool,
    last_stderr: &Arc<RwLock<Option<String>>>,
) {
    let mut status = status.write();
    status.ended_at = Some(chrono::Utc::now().timestamp());
    match result {
        Ok(exit) => {
            status.exit_code = exit.code();
            status.state = if stopped || exit.success() {
                FfmpegJobState::Stopped
            } else {
                FfmpegJobState::Failed
            };
            if status.state == FfmpegJobState::Failed {
                status.message = last_stderr.read().clone();
            }
        }
        Err(error) => {
            status.state = FfmpegJobState::Failed;
            status.message = Some(format!("等待 FFmpeg 退出失败: {error}"));
        }
    }
}

#[tauri::command]
pub async fn spawn_ffmpeg_job(
    request: SpawnFfmpegJobRequest,
) -> Result<SpawnFfmpegJobResponse, String> {
    FFMPEG_JOBS.spawn(request).await
}

#[tauri::command]
pub async fn stop_ffmpeg_job(id: String) -> Result<FfmpegJobStatus, String> {
    FFMPEG_JOBS.stop(&id).await
}

#[tauri::command]
pub fn get_ffmpeg_job(id: String) -> Result<FfmpegJobStatus, String> {
    FFMPEG_JOBS.get(&id)
}

#[tauri::command]
pub fn list_ffmpeg_jobs() -> Vec<FfmpegJobStatus> {
    FFMPEG_JOBS.list()
}
