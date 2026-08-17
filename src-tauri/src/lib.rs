mod catalog;
mod config;
mod host;
mod host_update;
mod ipc;
mod plugins;
mod proxy;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, MutexGuard,
};

use plugins::{HostSnapshot, PluginManager};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::Mutex as AsyncMutex;

const SNAPSHOT_EVENT: &str = "host:snapshot-changed";

pub struct HostState {
    plugins: AsyncMutex<PluginManager>,
    tray: Mutex<Option<host::TrayUi>>,
    quitting: AtomicBool,
}

fn lock_tray(
    value: &Mutex<Option<host::TrayUi>>,
) -> Result<MutexGuard<'_, Option<host::TrayUi>>, String> {
    value.lock().map_err(|_| "托盘状态锁已损坏。".to_string())
}

async fn build_snapshot(state: &HostState) -> HostSnapshot {
    let mut plugins = state.plugins.lock().await;
    plugins.snapshot().await
}

pub async fn emit_snapshot(app: &AppHandle) -> Result<HostSnapshot, String> {
    let state = app.state::<HostState>();
    let snapshot = build_snapshot(&state).await;
    let running = snapshot
        .plugins
        .iter()
        .filter(|plugin| plugin.running)
        .count();
    let installed = snapshot
        .plugins
        .iter()
        .filter(|plugin| plugin.installed_version.is_some())
        .count();
    let summary = format!("{installed} 已装 / {running} 运行中");
    if let Ok(tray) = lock_tray(&state.tray) {
        if let Some(tray) = tray.as_ref() {
            host::update_tray(app, tray, &summary);
        }
    }
    app.emit(SNAPSHOT_EVENT, &snapshot)
        .map_err(|error| error.to_string())?;
    Ok(snapshot)
}

fn start_snapshot_publisher(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let _ = emit_snapshot(&app).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let state = app.state::<HostState>();
            if state.quitting.load(Ordering::SeqCst) {
                break;
            }
            let _ = emit_snapshot(&app).await;
        }
    });
}

#[tauri::command]
async fn get_snapshot(app: AppHandle) -> Result<HostSnapshot, String> {
    emit_snapshot(&app).await
}

#[tauri::command]
async fn refresh_releases(app: AppHandle) -> Result<HostSnapshot, String> {
    {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        plugins.reload_catalog()?;
        plugins.refresh_latest()?;
    }
    emit_snapshot(&app).await
}

#[tauri::command]
async fn reload_catalog(app: AppHandle) -> Result<HostSnapshot, String> {
    {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        plugins.reload_catalog()?;
    }
    emit_snapshot(&app).await
}

#[tauri::command]
async fn install_plugin(app: AppHandle, id: String) -> Result<HostSnapshot, String> {
    {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        plugins.install(&id, &app)?;
        plugins.wait_until_ready(&id).await?;
    }
    emit_snapshot(&app).await
}

#[tauri::command]
async fn uninstall_plugin(app: AppHandle, id: String) -> Result<HostSnapshot, String> {
    {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        plugins.uninstall(&id)?;
    }
    emit_snapshot(&app).await
}

#[tauri::command]
async fn set_plugin_enabled(
    app: AppHandle,
    id: String,
    enabled: bool,
) -> Result<HostSnapshot, String> {
    {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        plugins.set_enabled(&id, enabled)?;
        if enabled {
            plugins.wait_until_ready(&id).await?;
        }
    }
    emit_snapshot(&app).await
}

#[tauri::command]
async fn plugin_command(app: AppHandle, id: String, cmd: String) -> Result<HostSnapshot, String> {
    {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        plugins.plugin_command(&id, &cmd).await?;
    }
    emit_snapshot(&app).await
}

#[tauri::command]
async fn update_host_settings(
    app: AppHandle,
    auto_start_with_windows: bool,
    start_minimized: bool,
) -> Result<HostSnapshot, String> {
    {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        plugins.set_autostart(auto_start_with_windows, start_minimized)?;
        host::sync_autostart(auto_start_with_windows, start_minimized)?;
    }
    emit_snapshot(&app).await
}

#[tauri::command]
async fn update_proxy_settings(
    app: AppHandle,
    mode: String,
    url: String,
) -> Result<HostSnapshot, String> {
    let mode = match mode.trim().to_ascii_lowercase().as_str() {
        "off" => proxy::ProxyMode::Off,
        "system" => proxy::ProxyMode::System,
        "custom" => proxy::ProxyMode::Custom,
        other => return Err(format!("未知代理模式：{other}")),
    };
    {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        plugins.set_proxy(proxy::ProxySettings { mode, url })?;
    }
    emit_snapshot(&app).await
}

#[tauri::command]
async fn open_data_directory(state: State<'_, HostState>) -> Result<(), String> {
    let plugins = state.plugins.lock().await;
    host::open_data_directory(plugins.data_dir())
}

#[tauri::command]
async fn choose_data_directory(app: AppHandle) -> Result<HostSnapshot, String> {
    let folder = app
        .dialog()
        .file()
        .set_title("选择插件安装数据目录")
        .blocking_pick_folder()
        .and_then(|path| path.into_path().ok())
        .ok_or_else(|| "已取消选择目录。".to_string())?;

    let new_root = config::set_data_root(&folder)?;
    {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        plugins.relocate_data_directory(new_root)?;
    }
    emit_snapshot(&app).await
}

#[tauri::command]
async fn update_host(app: AppHandle) -> Result<(), String> {
    let release = {
        let state = app.state::<HostState>();
        let mut plugins = state.plugins.lock().await;
        if plugins.host_release().download_url.is_none()
            || plugins.host_release().asset_name.is_none()
        {
            plugins.refresh_latest()?;
        }
        plugins.host_release().clone()
    };

    let download_url = release
        .download_url
        .ok_or_else(|| "找不到壳安装包下载地址。".to_string())?;
    let asset_name = release
        .asset_name
        .ok_or_else(|| "找不到壳安装包文件名。".to_string())?;
    let latest = release.latest_version.unwrap_or_else(|| "最新".to_string());
    if !host_update::version_newer(&latest, &host_update::current_version()) {
        return Err("当前已是最新版本。".to_string());
    }

    let proxy_settings = {
        let state = app.state::<HostState>();
        let plugins = state.plugins.lock().await;
        plugins.proxy_settings()
    };

    let installer = host_update::installer_temp_path(&asset_name);
    host_update::emit_progress(&app, "download", Some(0.0), "开始下载壳安装包…");
    let mut last_reported = 0u8;
    host_update::download_with_progress(
        &download_url,
        &installer,
        &proxy_settings,
        |downloaded, total| {
            let percent = match total {
                Some(total) if total > 0 => (downloaded as f64 / total as f64) * 100.0,
                _ => 0.0,
            };
            let bucket = percent.floor() as u8 / 2;
            if bucket != last_reported || downloaded == total.unwrap_or(downloaded) {
                last_reported = bucket;
                host_update::emit_progress(
                    &app,
                    "download",
                    Some(percent),
                    &format!("下载壳更新 {percent:.0}%"),
                );
            }
        },
    )?;
    host_update::emit_progress(&app, "launch", None, "正在启动安装程序…");
    host_update::launch_installer(&installer)?;
    host::quit_keep_targets(app);
    Ok(())
}

#[tauri::command]
fn show_window(app: AppHandle) -> Result<(), String> {
    host::show_main_window(&app);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            host::show_main_window(&app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = config::resolve_data_directory().map_err(std::io::Error::other)?;
            let manager = PluginManager::load(data_dir)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
            let auto_start = manager.state().auto_start_with_windows;
            let start_minimized = manager.state().start_minimized;
            host::sync_autostart(auto_start, start_minimized).map_err(std::io::Error::other)?;
            let state = HostState {
                plugins: AsyncMutex::new(manager),
                tray: Mutex::new(None),
                quitting: AtomicBool::new(false),
            };
            {
                let mut plugins = state.plugins.blocking_lock();
                let _ = plugins.refresh_latest();
                plugins.start_enabled();
            }
            app.manage(state);
            let tray = host::setup_tray(app.handle()).map_err(std::io::Error::other)?;
            {
                let managed = app.state::<HostState>();
                *lock_tray(&managed.tray).map_err(std::io::Error::other)? = Some(tray);
            }
            let start_hidden =
                start_minimized || std::env::args().any(|argument| argument == "--hidden");
            if !start_hidden {
                host::show_main_window(app.handle());
            }
            start_snapshot_publisher(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let app = window.app_handle().clone();
                let state = app.state::<HostState>();
                if state.quitting.load(Ordering::SeqCst) {
                    return;
                }
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            refresh_releases,
            reload_catalog,
            install_plugin,
            uninstall_plugin,
            set_plugin_enabled,
            plugin_command,
            update_host_settings,
            update_proxy_settings,
            open_data_directory,
            choose_data_directory,
            update_host,
            show_window
        ])
        .run(tauri::generate_context!())
        .expect("运行 Background Studio 失败");
}
