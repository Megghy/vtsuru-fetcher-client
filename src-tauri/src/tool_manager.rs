use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;
use tokio::net::TcpStream;
use tokio::time::timeout;

#[cfg(target_os = "windows")]
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
#[cfg(target_os = "windows")]
use winreg::RegKey;

const VTS_STEAM_APP_ID: &str = "1325860";
const DEFAULT_OBS_WS_PORT: u16 = 4455;
const DEFAULT_VTS_API_PORT: u16 = 8001;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub version: String,
    pub api_level: u32,
    pub os: String,
    pub arch: String,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortProbeResult {
    pub host: String,
    pub port: u16,
    pub open: bool,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObsWebsocketConfigInfo {
    pub config_path: String,
    pub exists: bool,
    pub server_enabled: bool,
    pub server_port: u16,
    pub auth_required: bool,
    pub server_password: Option<String>,
    pub requires_restart: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObsToolInfo {
    pub installed: bool,
    pub running: bool,
    pub pids: Vec<u32>,
    pub executable_path: Option<String>,
    pub config_dir: Option<String>,
    pub websocket_config: Option<ObsWebsocketConfigInfo>,
    pub spout2_plugin_installed: bool,
    pub ndi_plugin_installed: bool,
    pub port_open: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VtsToolInfo {
    pub installed: bool,
    pub running: bool,
    pub pids: Vec<u32>,
    pub executable_path: Option<String>,
    pub steam_installed: bool,
    pub install_dir: Option<String>,
    pub streaming_assets_dir: Option<String>,
    pub models_dir: Option<String>,
    pub items_dir: Option<String>,
    pub config_dir: Option<String>,
    pub api_enabled: Option<bool>,
    pub api_port: u16,
    pub port_open: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedToolsSnapshot {
    pub obs: ObsToolInfo,
    pub vts: VtsToolInfo,
    pub scanned_at: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchToolRequest {
    pub tool: String,
    pub launch_mode: Option<String>,
    pub custom_path: Option<String>,
    pub args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchToolResult {
    pub tool: String,
    pub launched: bool,
    pub already_running: bool,
    pub method: String,
    pub command_target: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureObsWebsocketRequest {
    pub enabled: bool,
    pub port: Option<u16>,
    pub auth_required: Option<bool>,
    pub password: Option<String>,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn validate_local_host(host: Option<String>) -> Result<String, String> {
    let raw = host.unwrap_or_else(|| "127.0.0.1".to_string());
    let trimmed = raw.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "127.0.0.1" | "localhost" | "::1" => Ok(trimmed),
        _ => Err("仅允许探测本机地址 (127.0.0.1 / localhost / ::1)".to_string()),
    }
}

async fn probe_tcp(host: &str, port: u16, timeout_ms: u64) -> PortProbeResult {
    let started = Instant::now();
    let addr = if host == "::1" {
        format!("[::1]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let dur = Duration::from_millis(timeout_ms.clamp(100, 5000));
    match timeout(dur, TcpStream::connect(&addr)).await {
        Ok(Ok(_stream)) => PortProbeResult {
            host: host.to_string(),
            port,
            open: true,
            latency_ms: Some(started.elapsed().as_millis() as u64),
            error: None,
        },
        Ok(Err(err)) => PortProbeResult {
            host: host.to_string(),
            port,
            open: false,
            latency_ms: None,
            error: Some(err.to_string()),
        },
        Err(_) => PortProbeResult {
            host: host.to_string(),
            port,
            open: false,
            latency_ms: None,
            error: Some("连接超时".to_string()),
        },
    }
}

fn scan_processes(target_names: &[&str]) -> (Vec<u32>, Option<PathBuf>) {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
    );

    let mut pids = Vec::new();
    let mut exe_path = None;
    for (pid, proc_) in sys.processes() {
        let name = proc_.name().to_string_lossy().to_ascii_lowercase();
        if target_names.iter().any(|target| name == *target) {
            pids.push(pid.as_u32());
            if exe_path.is_none() {
                if let Some(exe) = proc_.exe() {
                    if exe.is_file() {
                        exe_path = Some(exe.to_path_buf());
                    }
                }
            }
        }
    }
    pids.sort_unstable();
    (pids, exe_path)
}

fn get_obs_config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|p| PathBuf::from(p).join("obs-studio"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME")
            .ok()
            .map(|p| PathBuf::from(p).join("Library/Application Support/obs-studio"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(|p| PathBuf::from(p).join("obs-studio"))
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|p| PathBuf::from(p).join(".config/obs-studio"))
            })
    }
}

fn detect_obs_executable(custom_path: Option<&str>, running_exe: Option<&Path>) -> Option<PathBuf> {
    if let Some(custom) = custom_path.map(str::trim).filter(|s| !s.is_empty()) {
        let candidate = PathBuf::from(custom);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Some(exe) = running_exe {
        if exe.is_file() {
            return Some(exe.to_path_buf());
        }
    }

    #[cfg(target_os = "windows")]
    {
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        for subkey in ["SOFTWARE\\OBS Studio", "SOFTWARE\\WOW6432Node\\OBS Studio"] {
            if let Ok(key) = hklm.open_subkey(subkey) {
                if let Ok(dir) = key.get_value::<String, _>("") {
                    let exe = PathBuf::from(&dir).join("bin").join("64bit").join("obs64.exe");
                    if exe.is_file() {
                        return Some(exe);
                    }
                }
            }
        }

        for candidate in [
            r"C:\Program Files\obs-studio\bin\64bit\obs64.exe",
            r"C:\Program Files (x86)\obs-studio\bin\64bit\obs64.exe",
            r"D:\Program Files\obs-studio\bin\64bit\obs64.exe",
        ] {
            let path = PathBuf::from(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let app = PathBuf::from("/Applications/OBS.app/Contents/MacOS/OBS");
        if app.is_file() {
            return Some(app);
        }
    }

    None
}

fn read_obs_websocket_config(config_dir: &Path, obs_running: bool) -> ObsWebsocketConfigInfo {
    let ws_config_path = config_dir
        .join("plugin_config")
        .join("obs-websocket")
        .join("config.json");
    let config_path_str = ws_config_path.to_string_lossy().into_owned();

    let Ok(raw) = fs::read_to_string(&ws_config_path) else {
        return ObsWebsocketConfigInfo {
            config_path: config_path_str,
            exists: false,
            server_enabled: false,
            server_port: DEFAULT_OBS_WS_PORT,
            auth_required: true,
            server_password: None,
            requires_restart: false,
        };
    };

    let parsed: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    let server_enabled = parsed
        .get("server_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let server_port = parsed
        .get("server_port")
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(DEFAULT_OBS_WS_PORT);
    let auth_required = parsed
        .get("auth_required")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let server_password = parsed
        .get("server_password")
        .and_then(Value::as_str)
        .map(str::to_string);

    ObsWebsocketConfigInfo {
        config_path: config_path_str,
        exists: true,
        server_enabled,
        server_port,
        auth_required,
        server_password,
        requires_restart: obs_running,
    }
}

fn check_obs_plugin_exists(
    obs_exe: Option<&Path>,
    config_dir: Option<&Path>,
    dll_names: &[&str],
    plugin_folder_names: &[&str],
) -> bool {
    let mut search_roots: Vec<PathBuf> = Vec::new();
    if let Some(exe) = obs_exe {
        // obs-studio/bin/64bit/obs64.exe -> obs-studio
        if let Some(obs_root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            search_roots.push(obs_root.join("obs-plugins").join("64bit"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(program_data) = std::env::var("ProgramData") {
            let base = PathBuf::from(program_data).join("obs-studio").join("plugins");
            for folder in plugin_folder_names {
                search_roots.push(base.join(folder).join("bin").join("64bit"));
            }
        }
    }
    if let Some(cfg) = config_dir {
        let base = cfg.join("plugins");
        for folder in plugin_folder_names {
            search_roots.push(base.join(folder).join("bin").join("64bit"));
        }
    }

    for dir in search_roots {
        for dll in dll_names {
            if dir.join(dll).is_file() {
                return true;
            }
        }
    }
    false
}

/// 解析 Steam `libraryfolders.vdf` 中的所有库目录路径
pub fn parse_vdf_library_paths(vdf_content: &str) -> Vec<PathBuf> {
    let mut results = Vec::new();
    for line in vdf_content.lines() {
        let trimmed = line.trim();
        let mut tokens = Vec::new();
        let mut chars = trimmed.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '"' {
                let mut buf = String::new();
                while let Some(ch) = chars.next() {
                    if ch == '\\' {
                        if let Some(escaped) = chars.next() {
                            buf.push(escaped);
                        }
                    } else if ch == '"' {
                        break;
                    } else {
                        buf.push(ch);
                    }
                }
                tokens.push(buf);
            }
        }
        if tokens.len() >= 2 && tokens[0].eq_ignore_ascii_case("path") {
            let path = PathBuf::from(&tokens[1]);
            if !results.contains(&path) {
                results.push(path);
            }
        }
    }
    results
}

#[cfg(target_os = "windows")]
fn detect_steam_path() -> Option<PathBuf> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) = hkcu.open_subkey("Software\\Valve\\Steam") {
        if let Ok(path) = key.get_value::<String, _>("SteamPath") {
            let pb = PathBuf::from(path);
            if pb.is_dir() {
                return Some(pb);
            }
        }
    }
    let fallback = PathBuf::from(r"C:\Program Files (x86)\Steam");
    fallback.is_dir().then_some(fallback)
}

#[cfg(not(target_os = "windows"))]
fn detect_steam_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join("Library/Application Support/Steam"))
            .filter(|p| p.is_dir())
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".local/share/Steam"))
            .filter(|p| p.is_dir())
    }
}

fn detect_vts_executable(
    custom_path: Option<&str>,
    running_exe: Option<&Path>,
    steam_path: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(custom) = custom_path.map(str::trim).filter(|s| !s.is_empty()) {
        let candidate = PathBuf::from(custom);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Some(exe) = running_exe {
        if exe.is_file() {
            return Some(exe.to_path_buf());
        }
    }

    let mut libraries = Vec::new();
    if let Some(steam) = steam_path {
        libraries.push(steam.to_path_buf());
        let vdf = steam.join("steamapps").join("libraryfolders.vdf");
        if let Ok(content) = fs::read_to_string(vdf) {
            for lib in parse_vdf_library_paths(&content) {
                if !libraries.contains(&lib) {
                    libraries.push(lib);
                }
            }
        }
    }

    for lib in libraries {
        let candidate = lib
            .join("steamapps")
            .join("common")
            .join("VTube Studio")
            .join("VTube Studio.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    #[cfg(target_os = "windows")]
    {
        for drive in ["C", "D", "E", "F", "G"] {
            let candidate = PathBuf::from(format!(
                r"{drive}:\SteamLibrary\steamapps\common\VTube Studio\VTube Studio.exe"
            ));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn read_vts_registry_api_hints() -> (Option<bool>, u16) {
    let mut api_enabled = None;
    let mut api_port = DEFAULT_VTS_API_PORT;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) = hkcu.open_subkey("Software\\Denchi\\VTube Studio") {
        for item in key.enum_values().flatten() {
            let (name, _val) = item;
            let lower = name.to_ascii_lowercase();
            if lower.contains("pluginapi") && lower.contains("enabled") {
                if let Ok(v) = key.get_value::<u32, _>(&name) {
                    api_enabled = Some(v != 0);
                }
            } else if lower.contains("api") && lower.contains("port") {
                if let Ok(v) = key.get_value::<u32, _>(&name) {
                    if let Ok(port) = u16::try_from(v) {
                        if port >= 1024 {
                            api_port = port;
                        }
                    }
                }
            }
        }
    }
    (api_enabled, api_port)
}

#[cfg(not(target_os = "windows"))]
fn read_vts_registry_api_hints() -> (Option<bool>, u16) {
    (None, DEFAULT_VTS_API_PORT)
}

fn validate_obs_args(args: &[String]) -> Result<Vec<String>, String> {
    const ALLOWED_FLAGS: &[&str] = &[
        "--minimize-to-tray",
        "--startstreaming",
        "--startrecording",
        "--startreplaybuffer",
        "--startvirtualcam",
        "--multi",
        "--disable-shutdown-check",
        "--portable",
    ];
    const ALLOWED_KV_FLAGS: &[&str] = &[
        "--profile",
        "--collection",
        "--scene",
        "--websocket_port",
        "--websocket_password",
    ];

    let mut sanitized = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            continue;
        }
        if ALLOWED_FLAGS.contains(&trimmed) {
            sanitized.push(trimmed.to_string());
            continue;
        }
        if ALLOWED_KV_FLAGS.contains(&trimmed) {
            let Some(value) = iter.next() else {
                return Err(format!("参数 {trimmed} 缺少取值"));
            };
            if value.contains('\0') || value.contains('\n') || value.contains('\r') {
                return Err(format!("参数 {trimmed} 包含非法字符"));
            }
            sanitized.push(trimmed.to_string());
            sanitized.push(value.clone());
            continue;
        }
        return Err(format!("不支持的 OBS 启动参数: {trimmed}"));
    }
    Ok(sanitized)
}

fn validate_vts_args(args: &[String]) -> Result<Vec<String>, String> {
    const ALLOWED_FLAGS: &[&str] = &[
        "-nosteam",
        "-popupwindow",
        "-screen-fullscreen",
        "-batchmode",
    ];
    let mut sanitized = Vec::new();
    for arg in args {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            continue;
        }
        if ALLOWED_FLAGS.contains(&trimmed) {
            sanitized.push(trimmed.to_string());
        } else {
            return Err(format!("不支持的 VTS 启动参数: {trimmed}"));
        }
    }
    Ok(sanitized)
}

fn validate_custom_executable_name(path: &Path, allowed_names: &[&str]) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("找不到可执行文件: {}", path.display()));
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !allowed_names.iter().any(|n| *n == file_name) {
        return Err(format!("不允许启动非目标程序: {file_name}"));
    }
    Ok(())
}

#[tauri::command]
pub fn get_client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        version: env!("CARGO_PKG_VERSION").to_string(),
        api_level: 2,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        features: vec![
            "rpc".to_string(),
            "global_shortcut".to_string(),
            "transcription".to_string(),
            "tool_discovery".to_string(),
            "tool_launch".to_string(),
            "obs_config_read".to_string(),
            "obs_config_write".to_string(),
            "vts_config_read".to_string(),
            "port_probe".to_string(),
            "folder_open".to_string(),
        ],
    }
}

#[tauri::command]
pub async fn probe_local_port(
    host: Option<String>,
    port: u16,
    timeout_ms: Option<u64>,
) -> Result<PortProbeResult, String> {
    let safe_host = validate_local_host(host)?;
    Ok(probe_tcp(&safe_host, port, timeout_ms.unwrap_or(600)).await)
}

#[tauri::command]
pub async fn get_managed_tools_status(
    custom_obs_path: Option<String>,
    custom_vts_path: Option<String>,
) -> ManagedToolsSnapshot {
    let (obs_pids, obs_running_exe) = scan_processes(&["obs64.exe", "obs32.exe", "obs"]);
    let obs_running = !obs_pids.is_empty();
    let obs_exe = detect_obs_executable(custom_obs_path.as_deref(), obs_running_exe.as_deref());
    let obs_config_dir = get_obs_config_dir();
    let obs_ws_cfg = obs_config_dir
        .as_deref()
        .map(|dir| read_obs_websocket_config(dir, obs_running));
    let obs_port = obs_ws_cfg
        .as_ref()
        .map(|c| c.server_port)
        .unwrap_or(DEFAULT_OBS_WS_PORT);
    let obs_spout2 = check_obs_plugin_exists(
        obs_exe.as_deref(),
        obs_config_dir.as_deref(),
        &["win-spout.dll"],
        &["win-spout", "obs-spout2-plugin"],
    );
    let obs_ndi = check_obs_plugin_exists(
        obs_exe.as_deref(),
        obs_config_dir.as_deref(),
        &["obs-ndi.dll", "distroav.dll"],
        &["obs-ndi", "distroav"],
    );

    let (vts_pids, vts_running_exe) = scan_processes(&["vtube studio.exe", "vtube studio"]);
    let vts_running = !vts_pids.is_empty();
    let steam_path = detect_steam_path();
    let vts_exe = detect_vts_executable(
        custom_vts_path.as_deref(),
        vts_running_exe.as_deref(),
        steam_path.as_deref(),
    );
    let vts_install_dir = vts_exe.as_deref().and_then(Path::parent).map(Path::to_path_buf);
    let vts_streaming_assets = vts_install_dir
        .as_ref()
        .map(|d| d.join("VTube Studio_Data").join("StreamingAssets"))
        .filter(|p| p.is_dir());
    let vts_models_dir = vts_streaming_assets
        .as_ref()
        .map(|d| d.join("Live2DModels"))
        .filter(|p| p.is_dir());
    let vts_items_dir = vts_streaming_assets
        .as_ref()
        .map(|d| d.join("Items"))
        .filter(|p| p.is_dir());
    let vts_config_dir = vts_streaming_assets
        .as_ref()
        .map(|d| d.join("Config"))
        .filter(|p| p.is_dir());
    let (vts_api_enabled, vts_api_port) = read_vts_registry_api_hints();

    let (obs_probe, vts_probe) = tokio::join!(
        probe_tcp("127.0.0.1", obs_port, 350),
        probe_tcp("127.0.0.1", vts_api_port, 350)
    );

    ManagedToolsSnapshot {
        obs: ObsToolInfo {
            installed: obs_exe.is_some(),
            running: obs_running,
            pids: obs_pids,
            executable_path: obs_exe.map(|p| p.to_string_lossy().into_owned()),
            config_dir: obs_config_dir.map(|p| p.to_string_lossy().into_owned()),
            websocket_config: obs_ws_cfg,
            spout2_plugin_installed: obs_spout2,
            ndi_plugin_installed: obs_ndi,
            port_open: obs_probe.open,
        },
        vts: VtsToolInfo {
            installed: vts_exe.is_some(),
            running: vts_running,
            pids: vts_pids,
            executable_path: vts_exe.map(|p| p.to_string_lossy().into_owned()),
            steam_installed: steam_path.is_some(),
            install_dir: vts_install_dir.map(|p| p.to_string_lossy().into_owned()),
            streaming_assets_dir: vts_streaming_assets.map(|p| p.to_string_lossy().into_owned()),
            models_dir: vts_models_dir.map(|p| p.to_string_lossy().into_owned()),
            items_dir: vts_items_dir.map(|p| p.to_string_lossy().into_owned()),
            config_dir: vts_config_dir.map(|p| p.to_string_lossy().into_owned()),
            api_enabled: vts_api_enabled,
            api_port: vts_api_port,
            port_open: vts_probe.open,
        },
        scanned_at: now_unix_ms(),
    }
}

#[tauri::command]
pub fn configure_obs_websocket(
    request: ConfigureObsWebsocketRequest,
) -> Result<ObsWebsocketConfigInfo, String> {
    let config_dir = get_obs_config_dir().ok_or_else(|| "无法定位 OBS 配置目录".to_string())?;
    let ws_dir = config_dir.join("plugin_config").join("obs-websocket");
    fs::create_dir_all(&ws_dir).map_err(|e| format!("创建 obs-websocket 配置目录失败: {e}"))?;

    let ws_file = ws_dir.join("config.json");
    let mut json_obj = fs::read_to_string(&ws_file)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let port = request.port.unwrap_or_else(|| {
        json_obj
            .get("server_port")
            .and_then(Value::as_u64)
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(DEFAULT_OBS_WS_PORT)
    });
    if port < 1024 {
        return Err("WebSocket 端口必须 >= 1024".to_string());
    }

    json_obj.insert("server_enabled".to_string(), Value::Bool(request.enabled));
    json_obj.insert("server_port".to_string(), json!(port));
    if let Some(auth_required) = request.auth_required {
        json_obj.insert("auth_required".to_string(), Value::Bool(auth_required));
    }
    if let Some(password) = request.password {
        json_obj.insert("server_password".to_string(), Value::String(password));
    }

    let formatted = serde_json::to_string_pretty(&Value::Object(json_obj))
        .map_err(|e| format!("序列化 OBS WebSocket 配置失败: {e}"))?;
    fs::write(&ws_file, formatted).map_err(|e| format!("写入 OBS WebSocket 配置失败: {e}"))?;

    let (obs_pids, _) = scan_processes(&["obs64.exe", "obs32.exe", "obs"]);
    Ok(read_obs_websocket_config(&config_dir, !obs_pids.is_empty()))
}

#[tauri::command]
pub fn launch_managed_tool(
    app: AppHandle,
    request: LaunchToolRequest,
) -> Result<LaunchToolResult, String> {
    match request.tool.as_str() {
        "obs" => {
            let (pids, running_exe) = scan_processes(&["obs64.exe", "obs32.exe", "obs"]);
            let args = validate_obs_args(request.args.as_deref().unwrap_or(&[]))?;
            if !pids.is_empty() && !args.iter().any(|a| a == "--multi") {
                return Ok(LaunchToolResult {
                    tool: "obs".to_string(),
                    launched: false,
                    already_running: true,
                    method: "noop".to_string(),
                    command_target: running_exe
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "obs64.exe".to_string()),
                });
            }

            let exe = detect_obs_executable(request.custom_path.as_deref(), running_exe.as_deref())
                .ok_or_else(|| "未检测到 OBS Studio 安装路径，请在工具设置中手动指定 obs64.exe".to_string())?;
            validate_custom_executable_name(&exe, &["obs64.exe", "obs32.exe", "obs"])?;

            let mut cmd = Command::new(&exe);
            if let Some(parent) = exe.parent() {
                cmd.current_dir(parent);
            }
            cmd.args(&args);
            cmd.spawn().map_err(|e| format!("启动 OBS 失败: {e}"))?;

            Ok(LaunchToolResult {
                tool: "obs".to_string(),
                launched: true,
                already_running: false,
                method: "direct".to_string(),
                command_target: exe.to_string_lossy().into_owned(),
            })
        }
        "vts" => {
            let (pids, running_exe) = scan_processes(&["vtube studio.exe", "vtube studio"]);
            if !pids.is_empty() {
                return Ok(LaunchToolResult {
                    tool: "vts".to_string(),
                    launched: false,
                    already_running: true,
                    method: "noop".to_string(),
                    command_target: running_exe
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "VTube Studio.exe".to_string()),
                });
            }

            let args = validate_vts_args(request.args.as_deref().unwrap_or(&[]))?;
            let steam_path = detect_steam_path();
            let prefer_steam = request
                .launch_mode
                .as_deref()
                .map(|m| m == "steam")
                .unwrap_or(steam_path.is_some() && args.is_empty());

            if prefer_steam {
                let steam_uri = format!("steam://rungameid/{VTS_STEAM_APP_ID}");
                app.opener()
                    .open_url(&steam_uri, None::<&str>)
                    .map_err(|e| format!("通过 Steam 拉起 VTube Studio 失败: {e}"))?;
                return Ok(LaunchToolResult {
                    tool: "vts".to_string(),
                    launched: true,
                    already_running: false,
                    method: "steam".to_string(),
                    command_target: steam_uri,
                });
            }

            let exe = detect_vts_executable(
                request.custom_path.as_deref(),
                running_exe.as_deref(),
                steam_path.as_deref(),
            )
            .ok_or_else(|| "未检测到 VTube Studio 安装路径，请通过 Steam 启动或手动指定目录".to_string())?;
            validate_custom_executable_name(&exe, &["vtube studio.exe", "vtube studio"])?;

            let mut cmd = Command::new(&exe);
            if let Some(parent) = exe.parent() {
                cmd.current_dir(parent);
            }
            cmd.args(&args);
            cmd.spawn()
                .map_err(|e| format!("启动 VTube Studio 失败: {e}"))?;

            Ok(LaunchToolResult {
                tool: "vts".to_string(),
                launched: true,
                already_running: false,
                method: "direct".to_string(),
                command_target: exe.to_string_lossy().into_owned(),
            })
        }
        other => Err(format!("不支持的托管工具: {other}")),
    }
}

#[tauri::command]
pub fn stop_managed_tool(tool: String) -> Result<usize, String> {
    let target_names: &[&str] = match tool.as_str() {
        "obs" => &["obs64.exe", "obs32.exe", "obs"],
        "vts" => &["vtube studio.exe", "vtube studio"],
        other => return Err(format!("不支持的托管工具: {other}")),
    };

    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing(),
    );

    let mut stopped = 0usize;
    for proc_ in sys.processes().values() {
        let name = proc_.name().to_string_lossy().to_ascii_lowercase();
        if target_names.iter().any(|t| name == *t) && proc_.kill() {
            stopped += 1;
        }
    }
    Ok(stopped)
}

#[tauri::command]
pub fn open_managed_tool_dir(
    app: AppHandle,
    target: String,
    custom_path: Option<String>,
) -> Result<String, String> {
    let dir: PathBuf = match target.as_str() {
        "obs_config" => get_obs_config_dir().ok_or_else(|| "未找到 OBS 配置目录".to_string())?,
        "obs_plugins" => {
            let obs_cfg = get_obs_config_dir().ok_or_else(|| "未找到 OBS 配置目录".to_string())?;
            let plugins = obs_cfg.join("plugins");
            fs::create_dir_all(&plugins).map_err(|e| format!("创建 OBS 插件目录失败: {e}"))?;
            plugins
        }
        "vts_root" | "vts_models" | "vts_items" | "vts_config" | "vts_backgrounds" => {
            let steam = detect_steam_path();
            let (_, running_exe) = scan_processes(&["vtube studio.exe", "vtube studio"]);
            let exe = detect_vts_executable(
                custom_path.as_deref(),
                running_exe.as_deref(),
                steam.as_deref(),
            )
            .ok_or_else(|| "未检测到 VTube Studio 安装目录".to_string())?;
            let root = exe
                .parent()
                .ok_or_else(|| "无法解析 VTube Studio 根目录".to_string())?;
            let streaming = root.join("VTube Studio_Data").join("StreamingAssets");
            match target.as_str() {
                "vts_root" => root.to_path_buf(),
                "vts_models" => streaming.join("Live2DModels"),
                "vts_items" => streaming.join("Items"),
                "vts_config" => streaming.join("Config"),
                "vts_backgrounds" => streaming.join("Backgrounds"),
                _ => unreachable!(),
            }
        }
        other => return Err(format!("不支持的目录标识: {other}")),
    };

    if !dir.exists() {
        return Err(format!("目标目录不存在: {}", dir.display()));
    }

    let path_str = dir.to_string_lossy().into_owned();
    app.opener()
        .open_path(&path_str, None::<&str>)
        .map_err(|e| format!("打开目录失败: {e}"))?;
    Ok(path_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steam_libraryfolders_vdf() {
        let sample = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}
"#;
        let libs = parse_vdf_library_paths(sample);
        assert_eq!(libs.len(), 2);
        assert_eq!(libs[0], PathBuf::from(r"C:\Program Files (x86)\Steam"));
        assert_eq!(libs[1], PathBuf::from(r"D:\SteamLibrary"));
    }

    #[test]
    fn validates_local_host_strictly() {
        assert!(validate_local_host(None).is_ok());
        assert!(validate_local_host(Some("localhost".to_string())).is_ok());
        assert!(validate_local_host(Some("192.168.1.1".to_string())).is_err());
    }

    #[test]
    fn rejects_unknown_obs_args() {
        assert!(validate_obs_args(&["--minimize-to-tray".into(), "--profile".into(), "Live".into()]).is_ok());
        assert!(validate_obs_args(&["--evil-flag".into()]).is_err());
    }
}
