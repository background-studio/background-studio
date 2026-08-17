use std::{
    collections::HashMap,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    catalog::{self, PluginDef},
    config,
    core::{load_from_install_dir, protocol::capability_labels, request, request_with_params},
    host_update,
    proxy::{ProxyMode, ProxySettings},
};

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
    #[serde(default)]
    pub proxy: ProxySettings,
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
    pub capabilities: Vec<String>,
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
    pub proxy_mode: ProxyMode,
    pub proxy_url: String,
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
                proxy: ProxySettings::default(),
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

    pub fn proxy_settings(&self) -> ProxySettings {
        self.state.proxy.clone()
    }

    pub fn set_proxy(&mut self, proxy: ProxySettings) -> Result<(), String> {
        let proxy = proxy.normalized();
        // 允许先切到自定义再填地址；真正发请求时再校验。
        self.state.proxy = proxy;
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
        for name in ["library.json", "migration-standalone-v1.marker"] {
            let from = self.data_dir.join(name);
            let to = new_root.join(name);
            if from.is_file() && !to.exists() {
                fs::copy(&from, &to).map_err(|error| error.to_string())?;
            }
        }
        for name in ["media", "temporary", "profiles"] {
            let from = self.data_dir.join(name);
            let to = new_root.join(name);
            if from.exists() {
                copy_dir_recursive(&from, &to)?;
            }
        }

        self.data_dir = new_root;
        catalog::sync_bundled_icons(&self.data_dir)?;
        if new_plugins_json.exists() {
            let raw = fs::read_to_string(&new_plugins_json).map_err(|error| error.to_string())?;
            self.state = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
        }
        self.save()?;
        self.data_dir = config::set_data_root(&self.data_dir)?;
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

    fn exe_path(&self, id: &str, version: &str) -> Result<PathBuf, String> {
        let dir = self.install_dir(id, version);
        if let Some(manifest) = load_from_install_dir(&dir)? {
            return Ok(manifest.exe_path(&dir));
        }
        Ok(dir.join(&self.spec(id)?.exe_name))
    }

    pub fn pipe_name(&self, id: &str) -> Result<String, String> {
        if let Some(manifest) = self.installed_manifest(id) {
            return Ok(manifest.pipe_name);
        }
        Ok(self.spec(id)?.pipe_name.clone())
    }

    pub fn installed_manifest(&self, id: &str) -> Option<crate::core::ParsedManifest> {
        let version = self
            .state
            .plugins
            .iter()
            .find(|plugin| plugin.id == id)?
            .installed_version
            .as_ref()?;
        load_from_install_dir(&self.install_dir(id, version))
            .ok()
            .flatten()
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.state
            .plugins
            .iter()
            .any(|plugin| plugin.id == id && plugin.enabled && plugin.installed_version.is_some())
    }

    pub fn refresh_latest(&mut self) -> Result<(), String> {
        let catalog = self.catalog.clone();
        let proxy = self.state.proxy.clone();
        for plugin in catalog {
            match fetch_latest_plugin_asset(&plugin, &proxy) {
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
        match host_update::fetch_latest_host_release(&proxy) {
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

    pub fn install<F>(&mut self, id: &str, mut on_progress: F) -> Result<(), String>
    where
        F: FnMut(&str, Option<f64>, &str),
    {
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
        let download_url =
            format!("https://github.com/{owner}/{repo}/releases/download/{tag}/{asset_name}");
        let zip_path = self.data_dir.join(format!("{id}-{version_dir}-plugin.zip"));
        let proxy = self.state.proxy.clone();
        on_progress("download", Some(0.0), "开始下载…");
        let mut last_reported = 0u8;
        host_update::download_with_progress(
            &download_url,
            &zip_path,
            &proxy,
            |downloaded, total| {
                let percent = match total {
                    Some(total) if total > 0 => (downloaded as f64 / total as f64) * 100.0,
                    _ => 0.0,
                };
                let bucket = percent.floor() as u8 / 2;
                if bucket != last_reported || downloaded == total.unwrap_or(downloaded) {
                    last_reported = bucket;
                    on_progress("download", Some(percent), &format!("下载中 {percent:.0}%"));
                }
            },
        )?;
        on_progress("extract", None, "解压中…");
        let target = self.install_dir(id, &version_dir);
        let staging = target.with_file_name(format!(
            ".{}.{}.installing",
            version_dir,
            uuid::Uuid::new_v4()
        ));
        let backup =
            target.with_file_name(format!(".{}.{}.backup", version_dir, uuid::Uuid::new_v4()));
        fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
        if let Err(error) = extract_zip(&zip_path, &staging) {
            let _ = fs::remove_dir_all(&staging);
            let _ = fs::remove_file(&zip_path);
            return Err(error);
        }
        let _ = fs::remove_file(&zip_path);
        match load_from_install_dir(&staging) {
            Ok(Some(manifest)) => {
                if manifest.id != id {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(format!(
                        "plugin.json 的 id 为 {}，与目录插件 {id} 不一致。",
                        manifest.id
                    ));
                }
                if !manifest.exe_path(&staging).exists() {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(format!("插件包缺少可执行文件：{}", manifest.exe_name));
                }
            }
            Ok(None) => {
                if !staging.join(&exe_name).exists() {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(format!("插件包缺少可执行文件：{exe_name}"));
                }
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        }
        self.stop(id)?;
        if target.exists() {
            fs::rename(&target, &backup).map_err(|error| {
                let _ = fs::remove_dir_all(&staging);
                error.to_string()
            })?;
        }
        if let Err(error) = fs::rename(&staging, &target) {
            if backup.exists() {
                let _ = fs::rename(&backup, &target);
            }
            let _ = fs::remove_dir_all(&staging);
            return Err(error.to_string());
        }
        if backup.exists() {
            let _ = fs::remove_dir_all(&backup);
        }
        on_progress("start", None, "启动中…");
        {
            let record = self.record_mut(id)?;
            record.installed_version = Some(version_dir);
            record.enabled = true;
        }
        self.save()?;
        self.start(id)?;
        on_progress("done", Some(100.0), "安装完成");
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
        // 先停干净（含孤儿旧版），再起当前 installedVersion，避免单实例把新版挡掉。
        self.stop(id)?;
        let version = self
            .state
            .plugins
            .iter()
            .find(|plugin| plugin.id == id)
            .and_then(|plugin| plugin.installed_version.clone())
            .ok_or_else(|| "插件尚未安装。".to_string())?;
        let exe = self.exe_path(id, &version)?;
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
        let graceful_pipe = self
            .installed_manifest(id)
            .filter(|manifest| manifest.plugin_protocol >= 2)
            .map(|manifest| manifest.pipe_name);
        let graceful_requested = graceful_pipe
            .as_deref()
            .is_some_and(|pipe| request_shutdown_blocking(pipe).is_ok());
        if let Some(mut child) = self.children.remove(id) {
            let deadline = Instant::now()
                + if graceful_requested {
                    Duration::from_secs(2)
                } else {
                    Duration::ZERO
                };
            let exited = loop {
                match child.try_wait() {
                    Ok(Some(_)) => break true,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    _ => break false,
                }
            };
            if !exited {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let exe_name = self
            .installed_manifest(id)
            .map(|manifest| manifest.exe_name)
            .unwrap_or_else(|| {
                self.spec(id)
                    .map(|spec| spec.exe_name.clone())
                    .unwrap_or_default()
            });
        let plugin_root = self.data_dir.join("plugins").join(id);
        kill_processes_under_plugin_root(&plugin_root, &exe_name);
        // 给命名管道一点释放时间，否则紧接着 start 新版仍可能被旧实例挡掉。
        std::thread::sleep(Duration::from_millis(200));
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

    pub async fn wait_until_ready(&mut self, id: &str) -> Result<Value, String> {
        let pipe = self.pipe_name(id)?;
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut last_error = "插件尚未创建控制管道。".to_string();
        loop {
            if !self.is_running(id) {
                return Err(format!("插件启动后意外退出：{last_error}"));
            }
            match request(&pipe, "status").await {
                Ok(status) => return Ok(status),
                Err(error) => last_error = error,
            }
            if Instant::now() >= deadline {
                return Err(format!("插件在 8 秒内未就绪：{last_error}"));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn plugin_request(
        &mut self,
        id: &str,
        cmd: &str,
        params: Option<Value>,
    ) -> Result<Value, String> {
        if !self.is_enabled(id) {
            return Err("请先启用插件。".to_string());
        }
        let pipe = self.pipe_name(id)?;
        if !self.is_running(id) {
            self.start(id)?;
        }
        self.wait_until_ready(id).await?;
        request_with_params(&pipe, cmd, params.as_ref()).await
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
            let pipe = self
                .pipe_name(&plugin.id)
                .unwrap_or_else(|_| plugin.pipe_name.clone());
            if running {
                match request(&pipe, "status").await {
                    Ok(status) => {
                        phase = status
                            .get("phase")
                            .and_then(|value| value.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        if let Some(message) =
                            status.get("message").and_then(|value| value.as_str())
                        {
                            status_message = message.to_string();
                        }
                    }
                    Err(error) => {
                        phase = "error".to_string();
                        status_message = format!("插件状态不可用：{error}");
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
            let manifest = self.installed_manifest(&plugin.id);
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
                plugin_protocol: manifest
                    .as_ref()
                    .map(|manifest| manifest.plugin_protocol)
                    .unwrap_or(1),
                update_available,
                icon_path,
                icon_web: format!("/plugins/{}.png", plugin.id),
                capabilities: manifest
                    .map(|manifest| capability_labels(&manifest.capabilities))
                    .unwrap_or_default(),
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
            proxy_mode: self.state.proxy.mode.clone(),
            proxy_url: self.state.proxy.url.clone(),
        }
    }
}

fn request_shutdown_blocking(pipe_name: &str) -> Result<Value, String> {
    let pipe_name = pipe_name.to_string();
    std::thread::Builder::new()
        .name("plugin-shutdown".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(request(&pipe_name, "shutdown"))
        })
        .map_err(|error| error.to_string())?
        .join()
        .map_err(|_| "插件 shutdown 线程异常退出。".to_string())?
}

fn extract_zip(zip_path: &Path, target: &Path) -> Result<(), String> {
    const MAX_ARCHIVE_ENTRIES: usize = 128;
    const MAX_ARCHIVE_FILE_BYTES: u64 = 128 * 1024 * 1024;
    const MAX_ARCHIVE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

    let file = File::open(zip_path).map_err(|error| error.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| error.to_string())?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(format!("插件包文件数超过 {MAX_ARCHIVE_ENTRIES} 项上限。"));
    }
    let mut total_size = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        if entry.size() > MAX_ARCHIVE_FILE_BYTES {
            return Err("插件包中单个文件超过 128 MiB 上限。".to_string());
        }
        total_size = total_size
            .checked_add(entry.size())
            .ok_or_else(|| "插件包解压大小溢出。".to_string())?;
        if total_size > MAX_ARCHIVE_TOTAL_BYTES {
            return Err("插件包解压后超过 256 MiB 上限。".to_string());
        }
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

fn fetch_latest_plugin_asset(
    spec: &PluginDef,
    proxy: &ProxySettings,
) -> Result<(String, String), String> {
    let prerelease_channel = env!("CARGO_PKG_VERSION").contains('-');
    let endpoint = if prerelease_channel {
        format!("repos/{}/{}/releases?per_page=20", spec.owner, spec.repo)
    } else {
        format!("repos/{}/{}/releases/latest", spec.owner, spec.repo)
    };
    let value = host_update::github_api_get(&endpoint, proxy)?;
    select_plugin_asset(spec, &value, prerelease_channel)
}

fn select_plugin_asset(
    spec: &PluginDef,
    value: &Value,
    prerelease_channel: bool,
) -> Result<(String, String), String> {
    let release = if prerelease_channel {
        value
            .as_array()
            .and_then(|releases| {
                releases.iter().find(|release| {
                    release.get("draft").and_then(Value::as_bool) != Some(true)
                        && release.get("prerelease").and_then(Value::as_bool) == Some(true)
                })
            })
            .ok_or_else(|| "没有找到可用的插件预发布版本。".to_string())?
    } else {
        value
    };
    let tag = release
        .get("tag_name")
        .and_then(|tag| tag.as_str())
        .ok_or_else(|| "Release 缺少 tag_name（仓库可能还没有正式发布）。".to_string())?
        .to_string();
    let assets = release
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

/// 结束 `plugins/{id}/` 下任意版本的同名 exe（含壳重启后丢失句柄的孤儿进程）。
/// 不碰独立 NSIS 安装路径，避免误杀非插件实例。
fn kill_processes_under_plugin_root(plugin_root: &Path, exe_name: &str) {
    let root = plugin_root
        .canonicalize()
        .unwrap_or_else(|_| plugin_root.to_path_buf());
    let root_norm = root
        .to_string_lossy()
        .replace('/', "\\")
        .trim_start_matches(r"\\?\")
        .to_ascii_lowercase();
    let exe_name = exe_name.replace('\'', "''");
    let root_ps = root_norm.replace('\'', "''");
    let script = format!(
        "$root = '{root_ps}'; \
         Get-CimInstance Win32_Process -Filter \"Name='{exe_name}'\" | ForEach-Object {{ \
           if ($_.ExecutablePath) {{ \
             $p = $_.ExecutablePath.Replace('/','\\').ToLower(); \
             if ($p.StartsWith('\\\\?\\')) {{ $p = $p.Substring(4) }}; \
             if ($p.StartsWith($root)) {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }} \
           }} \
         }}"
    );
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = command.status();
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

#[cfg(test)]
mod tests {
    use super::select_plugin_asset;
    use crate::catalog::PluginDef;
    use serde_json::json;

    fn plugin() -> PluginDef {
        PluginDef {
            id: "codex".to_string(),
            display_name: "Codex".to_string(),
            owner: "background-studio".to_string(),
            repo: "codex_desktop_background".to_string(),
            asset_prefix: "CodexBackgroundStudio-".to_string(),
            exe_name: "Codex Background Studio.exe".to_string(),
            pipe_name: r"\\.\pipe\background-studio-codex-v1".to_string(),
            target_hint: String::new(),
            icon: None,
        }
    }

    #[test]
    fn stable_channel_reads_latest_release_object() {
        let value = json!({
            "tag_name": "v0.5.4",
            "assets": [{ "name": "CodexBackgroundStudio-0.5.4-plugin.zip" }]
        });
        assert_eq!(
            select_plugin_asset(&plugin(), &value, false).unwrap(),
            (
                "0.5.4".to_string(),
                "CodexBackgroundStudio-0.5.4-plugin.zip".to_string()
            )
        );
    }

    #[test]
    fn prerelease_channel_ignores_stable_and_draft_releases() {
        let value = json!([
            {
                "tag_name": "v0.5.4",
                "draft": false,
                "prerelease": false,
                "assets": [{ "name": "CodexBackgroundStudio-0.5.4-plugin.zip" }]
            },
            {
                "tag_name": "v0.5.5-beta.2",
                "draft": true,
                "prerelease": true,
                "assets": [{ "name": "CodexBackgroundStudio-0.5.5-beta.2-plugin.zip" }]
            },
            {
                "tag_name": "v0.5.5-beta.1",
                "draft": false,
                "prerelease": true,
                "assets": [{ "name": "CodexBackgroundStudio-0.5.5-beta.1-plugin.zip" }]
            }
        ]);
        assert_eq!(
            select_plugin_asset(&plugin(), &value, true).unwrap().0,
            "0.5.5-beta.1"
        );
    }
}
