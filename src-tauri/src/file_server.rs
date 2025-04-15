use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Response, Server};
use tokio::sync::oneshot;

// 文件服务器的配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileServerConfig {
    pub folder_path: String,
    pub port: u16,
}

// 文件服务器的状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileServerStatus {
    pub running: bool,
    pub folder_path: String,
    pub port: u16,
}

// 用于管理服务器的结构体
pub struct FileServerManager {
    config: Arc<Mutex<FileServerConfig>>,
    shutdown_sender: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    running: Arc<Mutex<bool>>,
}

impl FileServerManager {
    pub fn new() -> Self {
        FileServerManager {
            config: Arc::new(Mutex::new(FileServerConfig {
                folder_path: String::from(""),
                port: 8080,
            })),
            shutdown_sender: Arc::new(Mutex::new(None)),
            running: Arc::new(Mutex::new(false)),
        }
    }

    // 启动文件服务器
    pub fn start_server(&self) -> Result<FileServerStatus, String> {
        let mut running = self.running.lock().unwrap();
        if *running {
            return Err("服务器已经在运行中".to_string());
        }

        let config = self.config.lock().unwrap().clone();
        if config.folder_path.is_empty() {
            return Err("文件夹路径未设置".to_string());
        }

        // 检查文件夹是否存在
        let path = PathBuf::from(&config.folder_path);
        if !path.exists() || !path.is_dir() {
            return Err(format!("文件夹不存在: {}", config.folder_path));
        }

        // 创建关闭通道
        let (tx, rx) = oneshot::channel();
        *self.shutdown_sender.lock().unwrap() = Some(tx);

        // 复制变量用于线程
        let folder_path = config.folder_path.clone();
        let port = config.port;
        let running_arc = self.running.clone();

        // 启动服务器线程
        thread::spawn(move || {
            let addr = format!("127.0.0.1:{}", port);
            let server = match Server::http(&addr) {
                Ok(server) => server,
                Err(err) => {
                    eprintln!("无法启动服务器: {}", err);
                    *running_arc.lock().unwrap() = false;
                    return;
                }
            };

            println!("文件服务器启动在 http://{}", addr);
            *running_arc.lock().unwrap() = true;

            // 创建一个异步运行时来处理关闭信号
            let rt = tokio::runtime::Runtime::new().unwrap();
            let shutdown_future = async {
                if let Err(_) = rx.await {
                    // 关闭信号发送失败，可能发送端已关闭
                }
            };

            // 在另一个线程中等待关闭信号
            let server_ref = Arc::new(server);
            let server_clone = server_ref.clone();
            rt.spawn(async move {
                shutdown_future.await;
                let _ = server_clone.unblock(); // 解除阻塞，使服务器循环退出
            });

            // 处理请求
            for request in server_ref.incoming_requests() {
                let url_path = request.url();
                let file_path = path.join(&url_path[1..]); // 移除前导斜杠

                let response = if file_path.is_file() {
                    match fs::read(&file_path) {
                        Ok(content) => {
                            // 简单的MIME类型检测
                            let mime_type = match file_path.extension().and_then(|e| e.to_str()) {
                                Some("html") => "text/html",
                                Some("css") => "text/css",
                                Some("js") => "application/javascript",
                                Some("jpg") | Some("jpeg") => "image/jpeg",
                                Some("png") => "image/png",
                                Some("gif") => "image/gif",
                                Some("svg") => "image/svg+xml",
                                Some("json") => "application/json",
                                _ => "application/octet-stream",
                            };
                            Response::from_data(content).with_header(tiny_http::Header {
                                field: "Content-Type".parse().unwrap(),
                                value: mime_type.parse().unwrap(),
                            })
                        }
                        Err(err) => Response::from_string(format!("Error reading file: {}", err))
                            .with_status_code(500),
                    }
                } else if file_path.is_dir() {
                    // 生成目录列表
                    match generate_directory_listing(&file_path, &folder_path, url_path) {
                        Ok(listing) => {
                            Response::from_string(listing).with_header(tiny_http::Header {
                                field: "Content-Type".parse().unwrap(),
                                value: "text/html; charset=utf-8".parse().unwrap(),
                            })
                        }
                        Err(err) => {
                            Response::from_string(format!("Error listing directory: {}", err))
                                .with_status_code(500)
                        }
                    }
                } else {
                    Response::from_string("File not found").with_status_code(404)
                };

                if let Err(err) = request.respond(response) {
                    eprintln!("Error sending response: {}", err);
                }
            }

            // 服务器停止
            *running_arc.lock().unwrap() = false;
            println!("文件服务器已停止");
        });

        *running = true;
        Ok(FileServerStatus {
            running: true,
            folder_path: config.folder_path,
            port: config.port,
        })
    }

    // 停止文件服务器
    pub fn stop_server(&self) -> Result<FileServerStatus, String> {
        let mut running = self.running.lock().unwrap();
        if !*running {
            return Err("服务器未运行".to_string());
        }

        // 发送关闭信号
        let mut sender = self.shutdown_sender.lock().unwrap();
        if let Some(tx) = sender.take() {
            let _ = tx.send(());
        }

        *running = false;
        let config = self.config.lock().unwrap().clone();

        Ok(FileServerStatus {
            running: false,
            folder_path: config.folder_path,
            port: config.port,
        })
    }

    // 更新服务器配置
    pub fn update_config(
        &self,
        folder_path: Option<String>,
        port: Option<u16>,
    ) -> Result<FileServerConfig, String> {
        let mut config = self.config.lock().unwrap();

        if let Some(path) = folder_path {
            config.folder_path = path;
        }

        if let Some(p) = port {
            if p < 1024 || p > 65535 {
                return Err("端口号必须在1024到65535之间".to_string());
            }
            config.port = p;
        }

        Ok(config.clone())
    }

    // 获取当前服务器状态
    pub fn get_status(&self) -> FileServerStatus {
        let running = *self.running.lock().unwrap();
        let config = self.config.lock().unwrap().clone();

        FileServerStatus {
            running,
            folder_path: config.folder_path,
            port: config.port,
        }
    }
}

// 生成目录列表HTML
fn generate_directory_listing(
    dir_path: &PathBuf,
    base_path: &str,
    url_path: &str,
) -> io::Result<String> {
    let mut html = String::from("<!DOCTYPE html>\n<html>\n<head>\n<title>目录列表</title>\n");
    html.push_str("<style>body{font-family:Arial,sans-serif;margin:20px;}h1{color:#333;}ul{list-style-type:none;padding:0;}li{margin:5px 0;}a{text-decoration:none;color:#0077cc;}a:hover{text-decoration:underline;}</style>\n");
    html.push_str("</head>\n<body>\n");
    html.push_str(&format!("<h1>目录: {}</h1>\n<ul>\n", url_path));

    // 如果不是根目录，添加返回上级目录的链接
    if url_path != "/" {
        let parent = url_path.rsplitn(2, '/').collect::<Vec<&str>>();
        if parent.len() > 1 {
            let parent_url = if parent[1].is_empty() { "/" } else { parent[1] };
            html.push_str(&format!(
                "<li><a href=\"{}\">..</a> (上级目录)</li>\n",
                parent_url
            ));
        }
    }

    // 列出目录内容
    let entries = fs::read_dir(dir_path)?;
    for entry in entries {
        if let Ok(entry) = entry {
            let path = entry.path();
            if let Some(file_name) = path.file_name() {
                if let Some(file_name_str) = file_name.to_str() {
                    let file_url = format!(
                        "{}{}{}",
                        url_path.trim_end_matches('/'),
                        if url_path.ends_with('/') { "" } else { "/" },
                        file_name_str
                    );

                    let file_type = if path.is_dir() { "目录" } else { "文件" };
                    html.push_str(&format!(
                        "<li><a href=\"{}\">{}</a> ({})</li>\n",
                        file_url, file_name_str, file_type
                    ));
                }
            }
        }
    }

    html.push_str("</ul>\n</body>\n</html>");
    Ok(html)
}

// 创建文件服务器管理器的单例
lazy_static::lazy_static! {
    pub static ref FILE_SERVER: FileServerManager = FileServerManager::new();
}
