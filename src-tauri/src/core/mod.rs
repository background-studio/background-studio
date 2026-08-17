mod ipc;
mod manifest;
mod media;
mod media_server;
mod migrate;
mod models;
mod network;
mod persist;
mod profile;
pub mod protocol;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Instant,
};

use serde::Serialize;
use serde_json::Value;

use crate::{
    plugins::{HostSnapshot, PluginCard, PluginManager, PluginsState},
    proxy::ProxySettings,
};

use self::{
    media::MediaLibrary,
    media_server::MediaServer,
    migrate::migrate_standalone,
    models::{ImportResult, MediaItem, MediaKind, MediaOrigin},
    profile::{
        apply_patch, default_settings_schema, load_profile, sanitize_display, save_profile,
        PluginProfile,
    },
    protocol::{
        build_configure_params, capability_labels, parse_hello, revision_digest, ConfigureMedia,
        HelloResult, HOST_PROTOCOL,
    },
};

const DEFAULT_WORKER_MAX_MEDIA_BYTES: u64 = 64 * 1024 * 1024;

pub use self::{
    ipc::{request, request_with_params},
    manifest::{load_from_install_dir, PluginManifest as ParsedManifest},
    models::{MediaKind as SharedMediaKind, SlideshowOrder},
    profile::{ProfilePatch, SlideshowPatch},
};

pub struct HostCore {
    plugins: PluginManager,
    library: MediaLibrary,
    media_server: MediaServer,
    profiles: HashMap<String, PluginProfile>,
    hello: HashMap<String, HelloResult>,
    slideshow_ticks: HashMap<String, Instant>,
    last_phase: HashMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDetail {
    pub plugin: PluginCard,
    pub profile: PluginProfile,
    pub library: Vec<MediaItem>,
    pub thumbnail_sources: Vec<ThumbnailSource>,
    pub settings_schema: Value,
    pub preview_url: Option<String>,
    pub capabilities: Vec<String>,
    pub plugin_protocol: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThumbnailSource {
    pub media_id: String,
    pub path: PathBuf,
    pub cache_key: String,
    pub kind: MediaKind,
}

impl HostCore {
    pub fn load(data_dir: PathBuf) -> Result<Self, String> {
        let plugins = PluginManager::load(data_dir.clone())?;
        let mut library = MediaLibrary::load(&data_dir)?;
        migrate_standalone(
            &data_dir,
            &mut library,
            &migrate::default_standalone_sources(),
        )?;
        let media_server = MediaServer::start()?;
        let mut core = Self {
            plugins,
            library,
            media_server,
            profiles: HashMap::new(),
            hello: HashMap::new(),
            slideshow_ticks: HashMap::new(),
            last_phase: HashMap::new(),
        };
        core.load_profiles()?;
        core.sync_media_server();
        Ok(core)
    }

    fn load_profiles(&mut self) -> Result<(), String> {
        let ids: Vec<String> = self
            .plugins
            .state()
            .plugins
            .iter()
            .map(|plugin| plugin.id.clone())
            .collect();
        for id in ids {
            let profile = load_profile(self.plugins.data_dir(), &id)?;
            self.profiles.insert(id.clone(), profile);
            if self.plugins.installed_manifest(&id).is_some() {
                self.normalize_profile_for_plugin(&id)?;
            }
        }
        Ok(())
    }

    fn normalize_profile_for_plugin(&mut self, id: &str) -> Result<(), String> {
        let schema = self.settings_schema(id);
        let profile = self
            .profiles
            .get_mut(id)
            .ok_or_else(|| format!("插件 {id} 没有 Profile。"))?;
        let sanitized = sanitize_display(&profile.display, Some(&schema));
        if sanitized != profile.display {
            profile.display = sanitized;
            save_profile(self.plugins.data_dir(), id, profile)?;
        }
        Ok(())
    }

    pub fn state(&self) -> &PluginsState {
        self.plugins.state()
    }

    pub fn host_release(&self) -> &crate::host_update::HostReleaseInfo {
        self.plugins.host_release()
    }

    pub fn proxy_settings(&self) -> ProxySettings {
        self.plugins.proxy_settings()
    }

    pub fn reload_catalog(&mut self) -> Result<(), String> {
        self.plugins.reload_catalog()?;
        self.load_profiles()
    }

    pub fn set_autostart(&mut self, enabled: bool, start_minimized: bool) -> Result<(), String> {
        self.plugins.set_autostart(enabled, start_minimized)
    }

    pub fn set_proxy(&mut self, proxy: ProxySettings) -> Result<(), String> {
        self.plugins.set_proxy(proxy)
    }

    pub fn refresh_latest(&mut self) -> Result<(), String> {
        self.plugins.refresh_latest()
    }

    pub fn relocate_data_directory(&mut self, new_root: PathBuf) -> Result<(), String> {
        self.plugins.relocate_data_directory(new_root)?;
        self.library = MediaLibrary::load(self.plugins.data_dir())?;
        self.media_server = MediaServer::start()?;
        self.profiles.clear();
        self.load_profiles()?;
        self.sync_media_server();
        Ok(())
    }

    pub fn install<F>(&mut self, id: &str, on_progress: F) -> Result<(), String>
    where
        F: FnMut(&str, Option<f64>, &str),
    {
        self.plugins.install(id, on_progress)?;
        self.normalize_profile_for_plugin(id)
    }

    pub fn uninstall(&mut self, id: &str) -> Result<(), String> {
        self.hello.remove(id);
        self.slideshow_ticks.remove(id);
        self.last_phase.remove(id);
        self.plugins.uninstall(id)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), String> {
        if !enabled {
            self.hello.remove(id);
        }
        self.plugins.set_enabled(id, enabled)
    }

    pub fn start_enabled(&mut self) {
        self.plugins.start_enabled();
    }

    pub fn quit_all_keep_targets(&mut self) {
        self.plugins.quit_all_keep_targets();
    }

    pub async fn wait_until_ready(&mut self, id: &str) -> Result<Value, String> {
        self.plugins.wait_until_ready(id).await
    }

    pub async fn handshake_enabled(&mut self) -> Result<(), String> {
        let ids: Vec<String> = self
            .plugins
            .state()
            .plugins
            .iter()
            .filter(|plugin| plugin.enabled && plugin.installed_version.is_some())
            .map(|plugin| plugin.id.clone())
            .collect();
        let mut first_error = None;
        for id in ids {
            if let Err(error) = self.handshake(&id).await {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub async fn handshake(&mut self, id: &str) -> Result<(), String> {
        self.wait_until_ready(id).await?;
        if self.runtime_protocol(id) < HOST_PROTOCOL {
            return Ok(());
        }
        let hello = self
            .plugins
            .plugin_request(id, "hello", None)
            .await
            .map_err(|error| format!("插件 {id} hello 失败：{error}"))?;
        let hello = parse_hello(&hello)?;
        if hello.plugin_id != id {
            return Err(format!(
                "hello.pluginId={} 与宿主插件 {id} 不一致。",
                hello.plugin_id
            ));
        }
        self.hello.insert(id.to_string(), hello);
        if self.build_configure(id)?.is_some() {
            self.configure_profile(id).await?;
        }
        Ok(())
    }

    pub async fn plugin_command(&mut self, id: &str, cmd: &str) -> Result<Value, String> {
        match cmd {
            "apply" => self.apply_plugin(id).await,
            "open-ui" => Err("请在壳内打开插件详情，不再调用 open-ui。".to_string()),
            "shutdown" | "quit-keep-target" | "pause" | "restore" | "status" | "hello" => {
                self.plugins.plugin_request(id, cmd, None).await
            }
            other => Err(format!("未知命令：{other}")),
        }
    }

    pub async fn apply_plugin(&mut self, id: &str) -> Result<Value, String> {
        if self.runtime_protocol(id) >= HOST_PROTOCOL {
            self.configure_profile(id).await?;
        }
        let result = self.plugins.plugin_request(id, "apply", None).await?;
        if let Some(phase) = result.get("phase").and_then(Value::as_str) {
            self.last_phase.insert(id.to_string(), phase.to_string());
        }
        Ok(result)
    }

    pub async fn configure_profile(&mut self, id: &str) -> Result<Value, String> {
        let params = self
            .build_configure(id)?
            .ok_or_else(|| "请先选择要应用的媒体。".to_string())?;
        self.plugins
            .plugin_request(id, "configure", Some(params))
            .await
    }

    fn build_configure(&mut self, id: &str) -> Result<Option<Value>, String> {
        let profile = self
            .profiles
            .get(id)
            .cloned()
            .ok_or_else(|| format!("插件 {id} 没有 Profile。"))?;
        let Some(media_id) = profile.active_media_id.clone() else {
            return Ok(None);
        };
        let item = self
            .library
            .get_by_id(&media_id)
            .ok_or_else(|| "当前激活的媒体不存在。".to_string())?;
        let resolved = self
            .library
            .resolve_playback(&item, profile.slideshow.order, false)?;
        let maximum = self.max_media_bytes(id);
        if resolved.byte_size > maximum {
            return Err(format!(
                "当前插件最多接收 {} MiB 的单个媒体，所选文件为 {} MiB。",
                maximum / 1024 / 1024,
                resolved.byte_size.div_ceil(1024 * 1024)
            ));
        }
        self.sync_media_server();
        let url = self
            .media_server
            .url_for(&media_id)
            .ok_or_else(|| "媒体服务未能为当前媒体生成回环 URL。".to_string())?;
        let kind = match resolved.kind {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
        };
        let schema = self.settings_schema(id);
        let display = sanitize_display(&profile.display, Some(&schema));
        let media = ConfigureMedia {
            url,
            kind: kind.to_string(),
            mime_type: resolved.mime_type,
            sha256: resolved.sha256,
            byte_size: resolved.byte_size,
        };
        let revision = revision_digest(&media.sha256, &display);
        Ok(Some(build_configure_params(revision, media, display)?))
    }

    fn runtime_protocol(&self, id: &str) -> u32 {
        self.hello
            .get(id)
            .map(|hello| hello.plugin_protocol)
            .or_else(|| {
                self.plugins
                    .installed_manifest(id)
                    .map(|manifest| manifest.plugin_protocol)
            })
            .unwrap_or(1)
    }

    fn settings_schema(&self, id: &str) -> Value {
        self.plugins
            .installed_manifest(id)
            .map(|manifest| manifest.settings_schema)
            .filter(|schema| schema.is_object())
            .unwrap_or_else(default_settings_schema)
    }

    fn capabilities(&self, id: &str) -> Vec<String> {
        if let Some(hello) = self.hello.get(id) {
            return capability_labels(&hello.capabilities);
        }
        self.plugins
            .installed_manifest(id)
            .map(|manifest| capability_labels(&manifest.capabilities))
            .unwrap_or_default()
    }

    fn max_media_bytes(&self, id: &str) -> u64 {
        let declared = self
            .hello
            .get(id)
            .and_then(|hello| hello.capabilities.get("maxMediaBytes"))
            .and_then(Value::as_u64)
            .or_else(|| {
                self.plugins
                    .installed_manifest(id)
                    .and_then(|manifest| manifest.capabilities["maxMediaBytes"].as_u64())
            });
        declared
            .filter(|maximum| *maximum > 0 && *maximum <= protocol::MAX_MEDIA_BYTES)
            .unwrap_or(DEFAULT_WORKER_MAX_MEDIA_BYTES)
    }

    fn sync_media_server(&mut self) {
        let orders: Vec<(String, SlideshowOrder)> = self
            .library
            .items()
            .into_iter()
            .map(|item| {
                let order = self
                    .profiles
                    .values()
                    .find(|profile| {
                        profile.active_media_id.as_deref() == Some(item.id.as_str())
                            || profile.playlist_ids.iter().any(|id| id == &item.id)
                    })
                    .map(|profile| profile.slideshow.order)
                    .unwrap_or_default();
                (item.id, order)
            })
            .collect();
        self.media_server.sync(&mut self.library, &orders);
    }

    fn decorate_library(&mut self) -> Vec<MediaItem> {
        self.sync_media_server();
        self.library
            .items()
            .into_iter()
            .map(|mut item| {
                item.preview_url = self.media_server.url_for(&item.id);
                item
            })
            .collect()
    }

    pub async fn snapshot(&mut self) -> HostSnapshot {
        let mut snapshot = self.plugins.snapshot().await;
        for card in &mut snapshot.plugins {
            let protocol = self.runtime_protocol(&card.id);
            card.plugin_protocol = protocol;
            self.last_phase.insert(card.id.clone(), card.phase.clone());
        }
        snapshot
    }

    pub async fn plugin_detail(&mut self, id: &str) -> Result<PluginDetail, String> {
        let snapshot = self.snapshot().await;
        let plugin = snapshot
            .plugins
            .into_iter()
            .find(|plugin| plugin.id == id)
            .ok_or_else(|| format!("未知插件：{id}"))?;
        if !self.profiles.contains_key(id) {
            let profile = load_profile(self.plugins.data_dir(), id)?;
            self.profiles.insert(id.to_string(), profile);
        }
        let mut profile = self
            .profiles
            .get(id)
            .cloned()
            .ok_or_else(|| format!("插件 {id} 没有 Profile。"))?;
        let settings_schema = self.settings_schema(id);
        profile.display = sanitize_display(&profile.display, Some(&settings_schema));
        let library = self.decorate_library();
        let order = profile.slideshow.order;
        let thumbnail_sources = library
            .iter()
            .filter_map(|item| {
                let resolved = self.library.resolve_playback(item, order, false).ok()?;
                let modified = std::fs::metadata(&resolved.path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .unwrap_or_default();
                Some(ThumbnailSource {
                    media_id: item.id.clone(),
                    path: resolved.path,
                    cache_key: format!("{}-{modified}-{}", resolved.sha256, resolved.byte_size),
                    kind: resolved.kind,
                })
            })
            .collect();
        let preview_url = profile
            .active_media_id
            .as_deref()
            .and_then(|media_id| self.media_server.url_for(media_id));
        Ok(PluginDetail {
            plugin,
            profile,
            library,
            thumbnail_sources,
            settings_schema,
            preview_url,
            capabilities: self.capabilities(id),
            plugin_protocol: self.runtime_protocol(id),
        })
    }

    pub fn import_files(&mut self, paths: &[PathBuf]) -> ImportResult {
        let result = self.library.import_files(paths);
        self.sync_media_server();
        result
    }

    pub fn import_folder(&mut self, folder: &Path) -> ImportResult {
        let result = self.library.import_folder(folder);
        self.sync_media_server();
        result
    }

    pub fn import_remote(&mut self, url: &str, dynamic: bool) -> ImportResult {
        match network::download_remote_media(url, &self.library.temporary_directory) {
            Ok(download) => {
                let result = self.library.import_download(url, dynamic, download);
                self.sync_media_server();
                result
            }
            Err(error) => ImportResult {
                added: Vec::new(),
                skipped: vec![crate::core::models::SkippedImport {
                    path: url.to_string(),
                    reason: error,
                }],
            },
        }
    }

    pub async fn remove_media(
        &mut self,
        plugin_id: &str,
        media_id: &str,
    ) -> Result<PluginDetail, String> {
        self.library.remove(media_id)?;
        let affected: Vec<String> = self
            .profiles
            .iter()
            .filter(|(_, profile)| {
                profile.active_media_id.as_deref() == Some(media_id)
                    || profile.playlist_ids.iter().any(|id| id == media_id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &affected {
            if let Some(profile) = self.profiles.get_mut(id) {
                if profile.active_media_id.as_deref() == Some(media_id) {
                    profile.active_media_id = profile
                        .playlist_ids
                        .iter()
                        .find(|item| item.as_str() != media_id)
                        .cloned();
                }
                profile.playlist_ids.retain(|item| item != media_id);
                save_profile(self.plugins.data_dir(), id, profile)?;
            }
            self.reconfigure_running(id).await?;
        }
        self.plugin_detail(plugin_id).await
    }

    pub async fn set_active_media(
        &mut self,
        plugin_id: &str,
        media_id: Option<String>,
    ) -> Result<PluginDetail, String> {
        if let Some(media_id) = media_id.as_deref() {
            if self.library.get_by_id(media_id).is_none() {
                return Err("媒体不存在。".to_string());
            }
        }
        let profile = self
            .profiles
            .get_mut(plugin_id)
            .ok_or_else(|| format!("插件 {plugin_id} 没有 Profile。"))?;
        profile.active_media_id = media_id;
        save_profile(self.plugins.data_dir(), plugin_id, profile)?;
        self.reconfigure_running(plugin_id).await?;
        self.plugin_detail(plugin_id).await
    }

    pub async fn update_profile(
        &mut self,
        plugin_id: &str,
        patch: ProfilePatch,
    ) -> Result<PluginDetail, String> {
        if let Some(Some(media_id)) = patch.active_media_id.as_ref() {
            if self.library.get_by_id(media_id).is_none() {
                return Err("当前媒体不存在。".to_string());
            }
        }
        if let Some(playlist_ids) = patch.playlist_ids.as_ref() {
            if playlist_ids
                .iter()
                .any(|media_id| self.library.get_by_id(media_id).is_none())
            {
                return Err("轮播列表包含不存在的媒体。".to_string());
            }
        }
        let schema = self.settings_schema(plugin_id);
        let profile = self
            .profiles
            .get_mut(plugin_id)
            .ok_or_else(|| format!("插件 {plugin_id} 没有 Profile。"))?;
        apply_patch(profile, patch);
        profile.display = sanitize_display(&profile.display, Some(&schema));
        save_profile(self.plugins.data_dir(), plugin_id, profile)?;
        if profile.slideshow.enabled {
            self.slideshow_ticks
                .entry(plugin_id.to_string())
                .or_insert_with(Instant::now);
        }
        self.reconfigure_running(plugin_id).await?;
        self.plugin_detail(plugin_id).await
    }

    pub async fn refresh_media(
        &mut self,
        plugin_id: &str,
        media_id: &str,
    ) -> Result<PluginDetail, String> {
        let item = self
            .library
            .get_by_id(media_id)
            .ok_or_else(|| "媒体项目不存在。".to_string())?;
        match item.origin {
            MediaOrigin::Api => {
                let url = item
                    .source_url
                    .ok_or_else(|| "该媒体不是随机 API 来源。".to_string())?;
                let download =
                    network::download_remote_media(&url, &self.library.temporary_directory)?;
                self.library.refresh_with_download(media_id, download)?;
            }
            MediaOrigin::Folder => {
                let order = self
                    .profiles
                    .get(plugin_id)
                    .map(|profile| profile.slideshow.order)
                    .unwrap_or_default();
                self.library.advance_folder_cursor(&item, order)?;
            }
            _ => return Err("只有文件夹源或随机 API 可以刷新。".to_string()),
        }
        self.reconfigure_running(plugin_id).await?;
        self.plugin_detail(plugin_id).await
    }

    async fn reconfigure_running(&mut self, id: &str) -> Result<(), String> {
        if self.runtime_protocol(id) >= HOST_PROTOCOL && self.plugins.is_running(id) {
            if self.build_configure(id)?.is_some() {
                self.configure_profile(id).await?;
            } else {
                self.plugins.plugin_request(id, "restore", None).await?;
            }
        }
        Ok(())
    }

    pub async fn tick_slideshow(&mut self) -> Result<(), String> {
        let now = Instant::now();
        let ids: Vec<String> = self
            .profiles
            .iter()
            .filter(|(id, profile)| {
                profile.slideshow.enabled
                    && self.plugins.is_enabled(id)
                    && self.runtime_protocol(id) >= HOST_PROTOCOL
            })
            .map(|(id, _)| id.clone())
            .collect();
        let mut first_error = None;
        for id in ids {
            let interval = self
                .profiles
                .get(&id)
                .map(|profile| profile.slideshow.interval_seconds)
                .unwrap_or(300);
            let elapsed = self
                .slideshow_ticks
                .entry(id.clone())
                .or_insert(now)
                .elapsed()
                .as_secs();
            if elapsed < interval || !self.plugins.is_running(&id) {
                continue;
            }
            self.slideshow_ticks.insert(id.clone(), now);
            match self.advance_slideshow(&id) {
                Ok(true) => {
                    if let Err(error) = self.configure_profile(&id).await {
                        first_error.get_or_insert(error);
                        continue;
                    }
                    if self.last_phase.get(&id).map(String::as_str) == Some("active") {
                        if let Err(error) = self.plugins.plugin_request(&id, "apply", None).await {
                            first_error.get_or_insert(error);
                        }
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn advance_slideshow(&mut self, id: &str) -> Result<bool, String> {
        let profile = self
            .profiles
            .get(id)
            .cloned()
            .ok_or_else(|| format!("插件 {id} 没有 Profile。"))?;
        let playlist = if profile.playlist_ids.is_empty() {
            profile
                .active_media_id
                .clone()
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            profile.playlist_ids.clone()
        };
        if playlist.is_empty() {
            return Ok(false);
        }
        let current = profile.active_media_id.clone();
        if let Some(item) = current
            .as_ref()
            .and_then(|media_id| self.library.get_by_id(media_id))
        {
            if item.origin == MediaOrigin::Folder {
                self.library
                    .advance_folder_cursor(&item, profile.slideshow.order)?;
                return Ok(true);
            }
        }
        if playlist.len() < 2 {
            return Ok(false);
        }
        let index = current
            .as_ref()
            .and_then(|media_id| playlist.iter().position(|item| item == media_id))
            .unwrap_or(0);
        let next = match profile.slideshow.order {
            SlideshowOrder::Sequential => (index + 1) % playlist.len(),
            SlideshowOrder::Random => {
                if playlist.len() == 1 {
                    0
                } else {
                    let seed = now_seed();
                    let mut candidate = (seed as usize) % playlist.len();
                    if candidate == index {
                        candidate = (candidate + 1) % playlist.len();
                    }
                    candidate
                }
            }
        };
        if let Some(profile) = self.profiles.get_mut(id) {
            profile.active_media_id = Some(playlist[next].clone());
            save_profile(self.plugins.data_dir(), id, profile)?;
        }
        Ok(true)
    }
}

fn now_seed() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}
