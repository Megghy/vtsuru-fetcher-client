use crate::diagnostics::open_log_dir;
use crate::quit_app_internal;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

pub const MAIN_TRAY_ID: &str = "main-tray";

pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn reload_main_window_internal(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.reload();
    }
}

pub fn setup_native_tray(app: &AppHandle) -> tauri::Result<()> {
    let show_item = MenuItemBuilder::with_id("show", "显示主窗口").build(app)?;
    let reload_item = MenuItemBuilder::with_id("reload", "重新加载界面").build(app)?;
    let logs_item = MenuItemBuilder::with_id("logs", "打开日志目录").build(app)?;
    let update_item = MenuItemBuilder::with_id("check-update", "检查更新").build(app)?;
    let quit_item = MenuItemBuilder::with_id("quit", "退出客户端").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&show_item)
        .item(&reload_item)
        .separator()
        .item(&logs_item)
        .item(&update_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let mut builder = TrayIconBuilder::with_id(MAIN_TRAY_ID)
        .tooltip("VTsuru EventFetcher 客户端")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "reload" => reload_main_window_internal(app),
            "logs" => {
                let _ = open_log_dir(app.clone());
            }
            "check-update" => {
                show_main_window(app);
                let _ = app.emit("tray://check-update", ());
            }
            "quit" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    quit_app_internal(handle).await;
                });
            }
            _ => {}
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}
