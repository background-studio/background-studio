use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const PLUGIN_PROTOCOL: u32 = 1;

const BUILTIN_CATALOG: &str = include_str!("../resources/catalog.json");

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDef {
    pub id: String,
    pub display_name: String,
    pub owner: String,
    pub repo: String,
    pub asset_prefix: String,
    pub exe_name: String,
    pub pipe_name: String,
    #[serde(default)]
    pub target_hint: String,
    /// 内置相对文件名（如 codex.png）或 http(s) URL。
    #[serde(default)]
    pub icon: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogFile {
    #[serde(default = "default_protocol")]
    plugin_protocol: u32,
    plugins: Vec<PluginDef>,
}

fn default_protocol() -> u32 {
    PLUGIN_PROTOCOL
}

/// 合并内置 catalog + `%LOCALAPPDATA%/BackgroundStudio/catalog.json` 扩展项。
/// 同 id 时本地覆盖内置，便于动态加 Multica 等插件。
pub fn load_catalog(bootstrap_dir: &Path) -> Result<Vec<PluginDef>, String> {
    let builtin: CatalogFile =
        serde_json::from_str(BUILTIN_CATALOG).map_err(|error| error.to_string())?;
    let mut by_id: HashMap<String, PluginDef> = HashMap::new();
    for plugin in builtin.plugins {
        by_id.insert(plugin.id.clone(), plugin);
    }

    let overlay = bootstrap_dir.join("catalog.json");
    if overlay.exists() {
        let raw = fs::read_to_string(&overlay).map_err(|error| error.to_string())?;
        let local: CatalogFile =
            serde_json::from_str(&raw).map_err(|error| format!("本地 catalog.json 无效：{error}"))?;
        for plugin in local.plugins {
            by_id.insert(plugin.id.clone(), plugin);
        }
    }

    let mut plugins: Vec<PluginDef> = by_id.into_values().collect();
    plugins.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Ok(plugins)
}

pub fn ensure_default_overlay(bootstrap_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(bootstrap_dir).map_err(|error| error.to_string())?;
    let overlay = bootstrap_dir.join("catalog.json");
    if !overlay.exists() {
        // 写出一份可编辑副本，方便以后直接加插件而不用重装壳。
        fs::write(&overlay, BUILTIN_CATALOG).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn resolve_icon_path(data_dir: &Path, plugin: &PluginDef) -> Option<PathBuf> {
    let icons = data_dir.join("icons");
    let local = icons.join(format!("{}.png", plugin.id));
    if local.exists() {
        return Some(local);
    }
    if let Some(icon) = plugin.icon.as_deref() {
        if icon.starts_with("http://") || icon.starts_with("https://") {
            return None;
        }
        let named = icons.join(icon);
        if named.exists() {
            return Some(named);
        }
    }
    None
}

pub fn sync_bundled_icons(data_dir: &Path) -> Result<(), String> {
    let icons = data_dir.join("icons");
    fs::create_dir_all(&icons).map_err(|error| error.to_string())?;
    for (name, bytes) in [
        ("codex.png", include_bytes!("../resources/codex.png").as_slice()),
        ("notion.png", include_bytes!("../resources/notion.png").as_slice()),
        (
            "multica.png",
            include_bytes!("../resources/multica.png").as_slice(),
        ),
    ] {
        let path = icons.join(name);
        if !path.exists() {
            fs::write(&path, bytes).map_err(|error| error.to_string())?;
        }
        // 同时按 id 名一份，方便前端统一用 {id}.png
        let id_name = name.trim_end_matches(".png");
        let aliased = icons.join(format!("{id_name}.png"));
        if !aliased.exists() {
            let _ = fs::copy(&path, &aliased);
        }
    }
    Ok(())
}

pub fn find<'a>(plugins: &'a [PluginDef], id: &str) -> Option<&'a PluginDef> {
    plugins.iter().find(|plugin| plugin.id == id)
}
