use std::{
    collections::HashMap,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::{
    catalog::{self, PluginDef, PLUGIN_PROTOCOL},
    config, host_update, ipc,
};

pub const INSTALL_PROGRESS_EVENT: &str = "host:install-progress";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallProgress {
    pub id: String,
    pub phase: String,
    pub percent: Option<f64>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRecord {
    pub id: String,
    pub enabled: bool,
    pub installed_version: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginsState {
    pub plugins: Vec<PluginRecord>,
    pub auto_start_with_windows: bool,
    pub start_minimized: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCard {
    pub id: String,
    pub display_name: String,
    pub target_hint: String,
    pub enabled: bool,
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub latest_asset_name: Option<String>,
    pub running: bool,
    pub status_message: String,
    pub phase: String,
    pub plugin_protocol: u32,
    pub update_available: bool,
    /// 本地图标绝对路径；前端用 convertFileSrc 显示。缺省时回退 /plugins/{id}.png
    pub icon_path: Option<String>,
    pub icon_web: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSnapshot {
    pub plugins: Vec<PluginCard>,
    pub auto_start_with_windows: bool,
    pub start_minimized: bool,
    pub data_directory: String,
    pub warning: Option<String>,
    pub host_version: String,
    pub host_latest_version: Option<String>,
    pub host_update_available: bool,
    pub host_release_url: Option<String>,
}

pub struct PluginManager {
    data_dir: PathBuf,
    catalog: Vec<PluginDef>,
    state: PluginsState,
    children: HashMap<String, Child>,
    latest: HashMap<String, (String, String)>,
    /// 最近一次为该插件查询最新版失败的原因（限流 / 无资产等）。
    latest_errors: HashMap<String, String>,
    host_release: host_update::HostReleaseInfo,
    host_release_error: Option<String>,
}

impl PluginManager {
    pub fn load(data_dir: PathBuf) -> Result<Self, String> {
        let bootstrap = config::bootstrap_directory();
        catalog::ensure_default_overlay(&bootstrap)?;
        catalog::sync_bundled_icons(&data_dir)?;
        let catalog = catalog::load_catalog(&bootstrap)?;

        fs::create_dir_all(data_dir.join("plugins")).map_err(|error| error.to_string())?;
        let path = data_dir.join("plugins.json");
        let mut state = if path.exists() {
            let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
            serde_json::from_str(&raw).map_err(|error| error.to_string())?
        } else {
            PluginsState {
                plugins: catalog
                    .iter()
                    .map(|plugin| PluginRecord {
                        id: plugin.id.clone(),
                        enabled: false,
                        installed_version: None,
                    })
                    .collect(),
                auto_start_with_windows: false,
                start_minimized: true,
            }
        };
        for plugin in &catalog {
            if !state.plugins.iter().any(|item| item.id == plugin.id) {
                state.plugins.push(PluginRecord {
                    id: plugin.id.clone(),
                    enabled: false,
                    installed_version: None,
                });
            }
        }
        let manager = Self {
            data_dir,
            catalog,
            state,
            children: HashMap::new(),
            latest: HashMap::new(),
            latest_errors: HashMap::new(),
            host_release: host_update::HostReleaseInfo::default(),
            host_release_error: None,
        };
        manager.save()?;
        Ok(manager)
    }

    pub fn reload_catalog(&mut self) -> Result<(), String> {
        let bootstrap = config::bootstrap_directory();
        self.catalog = catalog::load_catalog(&bootstrap)?;
        for plugin in &self.catalog {
            if !self.state.plugins.iter().any(|item| item.id == plugin.id) {
                self.state.plugins.push(PluginRecord {
                    id: plugin.id.clone(),
                    enabled: false,
                    installed_version: None,
                });
            }
        }
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let path = self.data_dir.join("plugins.json");
        let raw = serde_json::to_string_pretty(&self.state).map_err(|error| error.to_string())?;
        fs::write(path, raw).map_err(|error| error.to_string())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn state(&self) -> &PluginsState {
        &self.state
    }

    fn spec(&self, id: &str) -> Result<&PluginDef, String> {
        catalog::find(&self.catalog, id).ok_or_else(|| format!("未知插件：{id}"))
    }

    pub fn set_autostart(&mut self, enabled: bool, start_minimized: bool) -> Result<(), String> {
        self.state.auto_start_with_windows = enabled;
        self.state.start_minimized = start_minimized;
        self.save()
    }

    pub fn relocate_data_directory(&mut self, new_root: PathBuf) -> Result<(), String> {
        if new_root == self.data_dir {
            return Ok(());
        }
        self.quit_all_keep_targets();
        fs::create_dir_all(&new_root).map_err(|error| error.to_string())?;
        fs::create_dir_all(new_root.join("plugins")).map_err(|error| error.to_string())?;
        fs::create_dir_all(new_root.join("icons")).map_err(|error| error.to_string())?;

        let old_plugins_json = self.data_dir.join("plugins.json");
        let new_plugins_json = new_root.join("plugins.json");
        if old_plugins_json.exists() && !new_plugins_json.exists() {
            fs::copy(&old_plugins_json, &new_plugins_json).map_err(|error| error.to_string())?;
        }

        let old_plugins_dir = self.data_dir.join("plugins");
        let new_plugins_dir = new_root.join("plugins");
        if old_plugins_dir.exists() {
            copy_dir_recursive(&old_plugins_dir, &new_plugins_dir)?;
        }
        let old_icons = self.data_dir.join("icons");
        let new_icons = new_root.join("icons");
        if old_icons.exists() {
            copy_dir_recursive(&old_icons, &new_icons)?;
        }

        self.data_dir = new_root;
        catalog::sync_bundled_icons(&self.data_dir)?;
        if new_plugins_json.exists() {
            let raw = fs::read_to_string(&new_plugins_json).map_err(|error| error.to_string())?;
            self.state = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
        }
        self.save()?;
        self.start_enabled();
        Ok(())
    }

    fn record_mut(&mut self, id: &str) -> Result<&mut PluginRecord, String> {
        if !self.state.plugins.iter().any(|plugin| plugin.id == id) {
            self.state.plugins.push(PluginRecord {
                id: id.to_string(),
                enabled: false,
                installed_version: None,
            });
        }
        self.state
            .plugins
            .iter_mut()
            .find(|plugin| plugin.id == id)
            .ok_or_else(|| format!("未知插件：{id}"))
    }

    fn install_dir(&self, id: &str, version: &str) -> PathBuf {
        self.data_dir.join("plugins").join(id).join(version)
    }

    fn exe_path(&self, spec: &PluginDef, version: &str) -> PathBuf {
        self.install_dir(&spec.id, version).join(&spec.exe_name)
    }

    pub fn refresh_latest(&mut self) -> Result<(), String> {
        let catalog = self.catalog.clone();
        for plugin in catalog {
            match fetch_latest_plugin_asset(&plugin) {
                Ok((version, asset)) => {
                    self.latest.insert(plugin.id.clone(), (version, asset));
                    self.latest_errors.remove(&plugin.id);
                }
                Err(error) => {
                    eprintln!("刷新 {} 最新版本失败：{error}", plugin.id);
                    self.latest_errors.insert(plugin.id.clone(), error);
                }
            }
        }
        match host_update::fetch_latest_host_release() {
            Ok(info) => {
                self.host_release = info;
                self.host_release_error = None;
            }
            Err(error) => {
                eprintln!("刷新壳最新版本失败：{error}");
                self.host_release_error = Some(error);
            }
        }
        Ok(())
    }

    pub fn host_release(&self) -> &host_update::HostReleaseInfo {
        &self.host_release
    }

    fn emit_install_progress(
        app: &AppHandle,
        id: &str,
        phase: &str,
        percent: Option<f64>,
        message: &str,
    ) {
        let _ = app.emit(
            INSTALL_PROGRESS_EVENT,
            InstallProgress {
                id: id.to_string(),
                phase: phase.to_string(),
                percent,
                message: message.to_string(),
            },
        );
    }

    pub fn install(&mut self, id: &str, app: &AppHandle) -> Result<(), String> {
        if !self.latest.contains_key(id) {
            self.refresh_latest()?;
        }
        let (version, asset_name) = match self.latest.get(id).cloned() {
            Some(pair) => pair,
            None => {
                if let Some(error) = self.latest_errors.get(id) {
                    return Err(format!("无法获取 {id} 的最新版：{error}"));
                }
                return Err(format!(
                    "找不到 {id} 的最新 Release（需要带 *-plugin.zip 的发布）。请先点「检查更新」。"
                ));
            }
        };
        let version_dir = version.trim_start_matches('v').to_string();
        let tag = format!("v{version_dir}");
        let (owner, repo, exe_name) = {
            let spec = self.spec(id)?;
            (spec.owner.clone(), spec.repo.clone(), spec.exe_name.clone())
        };
        let download_url = format!(
            "https://github.com/{owner}/{repo}/releases/download/{tag}/{asset_name}"
        );
        let zip_path = self.data_dir.join(format!("{id}-{version_dir}-plugin.zip"));
        Self::emit_install_progress(app, id, "download", Some(0.0), "开始下载…");
        let mut last_reported = 0u8;
        host_update::download_with_progress(&download_url, &zip_path, |downloaded, total| {
            let percent = match total {
                Some(total) if total > 0 => (downloaded as f64 / total as f64) * 100.0,
                _ => 0.0,
            };
            let bucket = percent.floor() as u8 / 2;
            if bucket != last_reported || downloaded == total.unwrap_or(downloaded) {
                last_reported = bucket;
                Self::emit_install_progress(
                    app,
                    id,
                    "download",
                    Some(percent),
                    &format!("下载中 {percent:.0}%"),
                );
            }
        })?;
        Self::emit_install_progress(app, id, "extract", None, "解压中…");
        let target = self.install_dir(id, &version_dir);
        if target.exists() {
            fs::remove_dir_all(&target).map_err(|error| error.to_string())?;
        }
        fs::create_dir_all(&target).map_err(|error| error.to_string())?;
        extract_zip(&zip_path, &target)?;
        let _ = fs::remove_file(&zip_path);
        let exe = target.join(&exe_name);
        if !exe.exists() {
            return Err(format!("插件包缺少可执行文件：{exe_name}"));
        }
        Self::emit_install_progress(app, id, "start", None, "启动中…");
        let _ = self.stop(id);
        {
            let record = self.record_mut(id)?;
            record.installed_version = Some(version_dir);
            record.enabled = true;
        }
        self.save()?;
        self.start(id)?;
        Self::emit_install_progress(app, id, "done", Some(100.0), "安装完成");
        Ok(())
    }

    pub fn uninstall(&mut self, id: &str) -> Result<(), String> {
        let _ = self.stop(id);
        let version = self.record_mut(id)?.installed_version.clone();
        if let Some(version) = version {
            let dir = self.install_dir(id, &version);
            if dir.exists() {
                fs::remove_dir_all(dir).map_err(|error| error.to_string())?;
            }
        }
        {
            let record = self.record_mut(id)?;
            record.installed_version = None;
            record.enabled = false;
        }
        self.save()
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), String> {
        {
            let record = self.record_mut(id)?;
            if enabled && record.installed_version.is_none() {
                return Err("请先安装插件。".to_string());
            }
            record.enabled = enabled;
        }
        self.save()?;
        if enabled {
            self.start(id)?;
        } else {
            self.stop(id)?;
        }
        Ok(())
    }

    pub fn start(&mut self, id: &str) -> Result<(), String> {
        if let Some(child) = self.children.get_mut(id) {
            if child.try_wait().ok().flatten().is_none() {
                return Ok(());
            }
            self.children.remove(id);
        }
        let version = self
            .state
            .plugins
            .iter()
            .find(|plugin| plugin.id == id)
            .and_then(|plugin| plugin.installed_version.clone())
            .ok_or_else(|| "插件尚未安装。".to_string())?;
        let exe = {
            let spec = self.spec(id)?;
            self.exe_path(spec, &version)
        };
        if !exe.exists() {
            return Err(format!("找不到插件可执行文件：{}", exe.display()));
        }
        let child = Command::new(&exe)
            .arg("--plugin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("启动插件失败：{error}"))?;
        self.children.insert(id.to_string(), child);
        Ok(())
    }

    pub fn stop(&mut self, id: &str) -> Result<(), String> {
        if let Some(mut child) = self.children.remove(id) {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }

    pub fn start_enabled(&mut self) {
        let ids: Vec<String> = self
            .state
            .plugins
            .iter()
            .filter(|plugin| plugin.enabled && plugin.installed_version.is_some())
            .map(|plugin| plugin.id.clone())
            .collect();
        for id in ids {
            if let Err(error) = self.start(&id) {
                eprintln!("自动启动插件 {id} 失败：{error}");
            }
        }
    }

    pub fn quit_all_keep_targets(&mut self) {
        let ids: Vec<String> = self.children.keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    pub fn is_running(&mut self, id: &str) -> bool {
        if let Some(child) = self.children.get_mut(id) {
            match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) => {
                    self.children.remove(id);
                    false
                }
                Err(_) => false,
            }
        } else {
            false
        }
    }

    pub async fn plugin_command(&mut self, id: &str, cmd: &str) -> Result<Value, String> {
        let pipe = self.spec(id)?.pipe_name.clone();
        if !self.is_running(id) {
            self.start(id)?;
            tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        }
        ipc::request(&pipe, cmd).await
    }

    pub async fn snapshot(&mut self) -> HostSnapshot {
        let mut cards = Vec::new();
        let mut warning = detect_standalone_warning();
        let catalog = self.catalog.clone();
        let mut refresh_failures: Vec<String> = Vec::new();
        for plugin in catalog {
            let record = self
                .state
                .plugins
                .iter()
                .find(|item| item.id == plugin.id)
                .cloned()
                .unwrap_or(PluginRecord {
                    id: plugin.id.clone(),
                    enabled: false,
                    installed_version: None,
                });
            let (latest_version, latest_asset_name) = self
                .latest
                .get(&plugin.id)
                .cloned()
                .map(|(version, asset)| (Some(version), Some(asset)))
                .unwrap_or((None, None));
            if latest_version.is_none() {
                if let Some(error) = self.latest_errors.get(&plugin.id) {
                    refresh_failures.push(format!("{}：{error}", plugin.display_name));
                }
            }
            let running = self.is_running(&plugin.id);
            let mut status_message = if record.installed_version.is_none() {
                if let Some(error) = self.latest_errors.get(&plugin.id) {
                    format!("未安装 · {error}")
                } else {
                    "未安装".to_string()
                }
            } else if !record.enabled {
                "未启用".to_string()
            } else if running {
                "运行中".to_string()
            } else {
                "已启用".to_string()
            };
            let mut phase = "idle".to_string();
            if running {
                if let Ok(status) = ipc::request(&plugin.pipe_name, "status").await {
                    phase = status
                        .get("phase")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    if let Some(message) = status.get("message").and_then(|value| value.as_str()) {
                        status_message = message.to_string();
                    }
                }
            }
            let update_available = match (&record.installed_version, &latest_version) {
                (Some(installed), Some(latest)) => {
                    installed.trim_start_matches('v') != latest.trim_start_matches('v')
                }
                _ => false,
            };
            let icon_path = catalog::resolve_icon_path(&self.data_dir, &plugin)
                .map(|path| path.to_string_lossy().into_owned());
            cards.push(PluginCard {
                id: plugin.id.clone(),
                display_name: plugin.display_name.clone(),
                target_hint: if plugin.target_hint.is_empty() {
                    "目标桌面应用".to_string()
                } else {
                    plugin.target_hint.clone()
                },
                enabled: record.enabled,
                installed_version: record.installed_version,
                latest_version,
                latest_asset_name,
                running,
                status_message,
                phase,
                plugin_protocol: PLUGIN_PROTOCOL,
                update_available,
                icon_path,
                icon_web: format!("/plugins/{}.png", plugin.id),
            });
        }
        let host_version = host_update::current_version();
        let host_latest_version = self.host_release.latest_version.clone();
        let host_update_available = host_latest_version
            .as_deref()
            .is_some_and(|latest| host_update::version_newer(latest, &host_version));
        if let Some(error) = &self.host_release_error {
            refresh_failures.push(format!("壳：{error}"));
        }
        if !refresh_failures.is_empty() {
            let detail = refresh_failures.join("；");
            warning = Some(match warning {
                Some(existing) => format!("{existing} 检查更新失败：{detail}"),
                None => format!("检查更新失败：{detail}"),
            });
        }
        HostSnapshot {
            plugins: cards,
            auto_start_with_windows: self.state.auto_start_with_windows,
            start_minimized: self.state.start_minimized,
            data_directory: self.data_dir.to_string_lossy().into_owned(),
            warning,
            host_version,
            host_latest_version,
            host_update_available,
            host_release_url: self.host_release.release_url.clone(),
        }
    }
}

fn extract_zip(zip_path: &Path, target: &Path) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|error| error.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| error.to_string())?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| "插件包包含非法路径。".to_string())?
            .to_owned();
        let out = target.join(name);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|error| error.to_string())?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            let mut outfile = File::create(&out).map_err(|error| error.to_string())?;
            std::io::copy(&mut entry, &mut outfile).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn fetch_latest_plugin_asset(spec: &PluginDef) -> Result<(String, String), String> {
    let value = host_update::github_api_get(&format!(
        "repos/{}/{}/releases/latest",
        spec.owner, spec.repo
    ))?;
    let tag = value
        .get("tag_name")
        .and_then(|tag| tag.as_str())
        .ok_or_else(|| "Release 缺少 tag_name（仓库可能还没有正式发布）。".to_string())?
        .to_string();
    let assets = value
        .get("assets")
        .and_then(|assets| assets.as_array())
        .ok_or_else(|| "Release 缺少 assets".to_string())?;
    let asset = assets
        .iter()
        .filter_map(|asset| asset.get("name").and_then(|name| name.as_str()))
        .find(|name| name.starts_with(&spec.asset_prefix) && name.ends_with("-plugin.zip"))
        .ok_or_else(|| {
            format!(
                "最新 Release {} 中没有 {}*-plugin.zip",
                tag, spec.asset_prefix
            )
        })?
        .to_string();
    Ok((tag.trim_start_matches('v').to_string(), asset))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(src).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if !to.exists() {
            fs::copy(&from, &to).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn detect_standalone_warning() -> Option<String> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let run = hkcu
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .ok()?;
    let mut hits = Vec::new();
    for name in [
        "Codex Background Studio",
        "Notion Background Studio",
        "Multica Background Studio",
    ] {
        if run.get_value::<String, _>(name).is_ok() {
            hits.push(name);
        }
    }
    if hits.is_empty() {
        None
    } else {
        Some(format!(
            "检测到独立版自启动：{}。建议改用壳插件模式，避免双托盘。",
            hits.join("、")
        ))
    }
}
