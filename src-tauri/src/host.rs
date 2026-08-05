use std::{process::Command, sync::atomic::Ordering};

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Runtime, Wry,
};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

use crate::HostState;

const AUTOSTART_NAME: &str = "Background Studio";

pub struct TrayUi {
    status: MenuItem<Wry>,
}

pub fn sync_autostart(enabled: bool, start_hidden: bool) -> Result<(), String> {
    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = hkcu
        .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .map_err(|error| error.to_string())?;
    if enabled {
        let mut command = format!("\"{}\"", current_exe.display());
        if start_hidden {
            command.push_str(" --hidden");
        }
        run.set_value(AUTOSTART_NAME, &command)
            .map_err(|error| error.to_string())
    } else {
        match run.delete_value(AUTOSTART_NAME) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn quit_keep_targets(app: AppHandle) {
    let state = app.state::<HostState>();
    if state.quitting.swap(true, Ordering::SeqCst) {
        return;
    }
    {
        let mut plugins = state.plugins.blocking_lock();
        plugins.quit_all_keep_targets();
    }
    app.exit(0);
}

pub fn setup_tray(app: &AppHandle) -> Result<TrayUi, String> {
    let status = MenuItem::with_id(app, "status", "状态：加载中", false, None::<&str>)
        .map_err(|error| error.to_string())?;
    let open = MenuItem::with_id(app, "open", "打开 Background Studio", true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let refresh = MenuItem::with_id(app, "refresh", "刷新插件状态", true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let quit = MenuItem::with_id(app, "quit", "退出（保留目标应用）", true, None::<&str>)
        .map_err(|error| error.to_string())?;
    let separator = PredefinedMenuItem::separator(app).map_err(|error| error.to_string())?;
    let menu = Menu::with_items(app, &[&status, &separator, &open, &refresh, &quit])
        .map_err(|error| error.to_string())?;
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| "应用图标资源不存在。".to_string())?;
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("Background Studio")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main_window(app),
            "refresh" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = crate::emit_snapshot(&app).await;
                });
            }
            "quit" => quit_keep_targets(app.clone()),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)
        .map_err(|error| error.to_string())?;
    Ok(TrayUi { status })
}

pub fn update_tray(app: &AppHandle, ui: &TrayUi, summary: &str) {
    let _ = ui.status.set_text(format!("状态：{summary}"));
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(format!("Background Studio · {summary}")));
    }
}

pub fn open_data_directory(path: &std::path::Path) -> Result<(), String> {
    Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}
