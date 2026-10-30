use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// catalog 文件自身的协议标记。插件运行时 exe/pipe 以安装目录 plugin.json 为准。
pub const PLUGIN_PROTOCOL: u32 = 1;

const BUILTIN_CATALOG: &str = include_str!("../resources/catalog.json");

/// 新装时写出的本地扩展目录模板：只放用户自定义项。
/// 旧版本会把整份内置目录复制进来，这类“旧镜像”由 [`merge_catalog`] 识别并忽略。
const DEFAULT_OVERLAY: &str = "{\n  \"pluginProtocol\": 1,\n  \"plugins\": []\n}\n";

/// 官方组织；官方目录项、官方清单与镜像识别都以它为身份。
pub const OFFICIAL_OWNER: &str = "background-studio";

/// 与壳内部虚拟档案 / 进度通道冲突的保留 id。
const RESERVED_IDS: &[&str] = &["console", "host"];

/// 目录项来源。合并优先级 builtin → official → local（后者覆盖前者）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogSource {
    /// 壳内置 resources/catalog.json（离线 bootstrap）。
    #[default]
    Builtin,
    /// 官方静态清单（网络或上次成功缓存）。
    Official,
    /// 用户本地 catalog.json 中的自定义项（非官方镜像）。
    Local,
}

impl CatalogSource {
    /// 内置与官方清单项都属于官方插件：版本只看官方清单，绝不回退 REST。
    pub fn is_official(self) -> bool {
        matches!(self, Self::Builtin | Self::Official)
    }
}

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
    /// 合并时标注的来源，不参与序列化。
    #[serde(skip)]
    pub source: CatalogSource,
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

/// 内置目录（离线 bootstrap）。
pub fn builtin_catalog() -> Result<Vec<PluginDef>, String> {
    let builtin: CatalogFile =
        serde_json::from_str(BUILTIN_CATALOG).map_err(|error| error.to_string())?;
    Ok(builtin
        .plugins
        .into_iter()
        .map(|mut plugin| {
            plugin.source = CatalogSource::Builtin;
            plugin
        })
        .collect())
}

/// 只读加载 `%LOCALAPPDATA%/BackgroundStudio/catalog.json`。
/// 返回 (目录项, 警告)；id 不安全的项被跳过并给出警告。
pub fn load_local_overlay(bootstrap_dir: &Path) -> Result<(Vec<PluginDef>, Vec<String>), String> {
    let overlay = bootstrap_dir.join("catalog.json");
    if !overlay.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let raw = fs::read_to_string(&overlay).map_err(|error| error.to_string())?;
    parse_local_overlay(&raw)
}

fn parse_local_overlay(raw: &str) -> Result<(Vec<PluginDef>, Vec<String>), String> {
    let local: CatalogFile =
        serde_json::from_str(raw).map_err(|error| format!("本地 catalog.json 无效：{error}"))?;
    let mut plugins = Vec::new();
    let mut warnings = Vec::new();
    for mut plugin in local.plugins {
        if let Err(error) = validate_plugin_id(&plugin.id) {
            warnings.push(format!("本地 catalog.json 已跳过目录项：{error}"));
            continue;
        }
        plugin.source = CatalogSource::Local;
        plugins.push(plugin);
    }
    Ok((plugins, warnings))
}

/// 本地项是否只是官方插件的镜像（旧版本写出的内置目录副本等）：
/// 与内置 / 官方项同 id 且 owner 仍为官方组织。镜像不得遮挡官方新元数据。
/// 同 id 但 owner 不同的项是用户刻意替换的第三方源，照常覆盖。
pub fn is_official_mirror(local: &PluginDef, existing: &PluginDef) -> bool {
    existing.source.is_official()
        && local.id == existing.id
        && local.owner.eq_ignore_ascii_case(OFFICIAL_OWNER)
}

/// 合并目录：builtin → official → local。
pub fn merge_catalog(
    builtin: &[PluginDef],
    official: Option<&[PluginDef]>,
    local: &[PluginDef],
) -> Vec<PluginDef> {
    let mut by_id: HashMap<String, PluginDef> = HashMap::new();
    for plugin in builtin {
        let mut plugin = plugin.clone();
        plugin.source = CatalogSource::Builtin;
        by_id.insert(plugin.id.clone(), plugin);
    }
    for plugin in official.unwrap_or_default() {
        let mut plugin = plugin.clone();
        plugin.source = CatalogSource::Official;
        by_id.insert(plugin.id.clone(), plugin);
    }
    for plugin in local {
        if by_id
            .get(&plugin.id)
            .is_some_and(|existing| is_official_mirror(plugin, existing))
        {
            continue;
        }
        let mut plugin = plugin.clone();
        plugin.source = CatalogSource::Local;
        by_id.insert(plugin.id.clone(), plugin);
    }
    let mut plugins: Vec<PluginDef> = by_id.into_values().collect();
    plugins.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then_with(|| a.id.cmp(&b.id))
    });
    plugins
}

pub fn ensure_default_overlay(bootstrap_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(bootstrap_dir).map_err(|error| error.to_string())?;
    let overlay = bootstrap_dir.join("catalog.json");
    if !overlay.exists() {
        // 只写空模板：官方插件来自内置目录与官方清单，本地文件仅用于自定义插件。
        fs::write(&overlay, DEFAULT_OVERLAY).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// 插件 id 会拼进安装目录、profile、图标路径，必须是安全的短标识。
pub fn validate_plugin_id(id: &str) -> Result<(), String> {
    let bytes = id.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-');
    if !valid {
        return Err(format!(
            "插件 id {id:?} 无效（仅允许小写字母、数字、-，且以字母或数字开头，最长 64）"
        ));
    }
    if RESERVED_IDS.contains(&id) {
        return Err(format!("插件 id {id:?} 为壳保留 id"));
    }
    Ok(())
}

pub fn resolve_icon_path(data_dir: &Path, plugin: &PluginDef) -> Option<PathBuf> {
    if let Some(path) = resolve_bundled_icon_path(data_dir, plugin) {
        return Some(path);
    }
    // 官方清单图标缓存（icons/official/<id>.png），仅在没有本地 / 内置图标时使用。
    let official = official_icon_cache_path(data_dir, &plugin.id);
    if plugin.source == CatalogSource::Official && official.exists() {
        return Some(official);
    }
    None
}

/// 本地覆盖（icons/<id>.png）或内置命名图标（icons/<icon>）。
pub fn resolve_bundled_icon_path(data_dir: &Path, plugin: &PluginDef) -> Option<PathBuf> {
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

pub fn official_icon_cache_path(data_dir: &Path, id: &str) -> PathBuf {
    data_dir
        .join("icons")
        .join("official")
        .join(format!("{id}.png"))
}

pub fn sync_bundled_icons(data_dir: &Path) -> Result<(), String> {
    let icons = data_dir.join("icons");
    fs::create_dir_all(&icons).map_err(|error| error.to_string())?;
    // 内置插件图标每次启动覆盖写入，避免旧错误图标残留。
    for (name, bytes) in [
        (
            "codex.png",
            include_bytes!("../resources/codex.png").as_slice(),
        ),
        (
            "notion.png",
            include_bytes!("../resources/notion.png").as_slice(),
        ),
        (
            "multica.png",
            include_bytes!("../resources/multica.png").as_slice(),
        ),
        (
            "grok-bot.png",
            include_bytes!("../resources/grok-bot.png").as_slice(),
        ),
        (
            "rebased.png",
            include_bytes!("../resources/rebased.png").as_slice(),
        ),
        (
            "cursor-agent.png",
            include_bytes!("../resources/cursor-agent.png").as_slice(),
        ),
        (
            "factory.png",
            include_bytes!("../resources/factory.png").as_slice(),
        ),
    ] {
        let path = icons.join(name);
        fs::write(&path, bytes).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn find<'a>(plugins: &'a [PluginDef], id: &str) -> Option<&'a PluginDef> {
    plugins.iter().find(|plugin| plugin.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(id: &str, owner: &str, name: &str) -> PluginDef {
        PluginDef {
            id: id.to_string(),
            display_name: name.to_string(),
            owner: owner.to_string(),
            repo: format!("{id}_repo"),
            asset_prefix: "X-".to_string(),
            exe_name: "x.exe".to_string(),
            pipe_name: r"\\.\pipe\x".to_string(),
            target_hint: String::new(),
            icon: None,
            source: CatalogSource::Local,
        }
    }

    #[test]
    fn merge_priority_is_builtin_then_official_then_local() {
        let builtin = builtin_catalog().unwrap();
        assert!(builtin.iter().any(|plugin| plugin.id == "factory"));
        let official = vec![
            def("codex", OFFICIAL_OWNER, "官方新名"),
            def("newbie", OFFICIAL_OWNER, "新插件"),
        ];
        let local = vec![
            def("codex", OFFICIAL_OWNER, "旧镜像"),
            def("notion", "someone", "第三方 Notion"),
            def("custom", "someone", "自定义"),
        ];
        let merged = merge_catalog(&builtin, Some(&official), &local);
        let get = |id: &str| merged.iter().find(|plugin| plugin.id == id).unwrap();
        assert_eq!(get("codex").display_name, "官方新名");
        assert_eq!(get("codex").source, CatalogSource::Official);
        assert_eq!(get("newbie").source, CatalogSource::Official);
        assert_eq!(get("notion").source, CatalogSource::Local);
        assert_eq!(get("notion").owner, "someone");
        assert_eq!(get("custom").source, CatalogSource::Local);
        assert_eq!(get("multica").source, CatalogSource::Builtin);
        // 离线（无官方清单）时旧镜像同样不遮挡内置项。
        let offline = merge_catalog(&builtin, None, &local);
        let codex = offline.iter().find(|plugin| plugin.id == "codex").unwrap();
        assert_eq!(codex.source, CatalogSource::Builtin);
    }

    #[test]
    fn local_overlay_skips_unsafe_ids_and_default_template_is_empty() {
        let raw = r#"{"plugins":[
            {"id":"../evil","displayName":"x","owner":"o","repo":"r","assetPrefix":"p","exeName":"e.exe","pipeName":"p"},
            {"id":"ok","displayName":"x","owner":"o","repo":"r","assetPrefix":"p","exeName":"e.exe","pipeName":"p"}
        ]}"#;
        let (plugins, warnings) = parse_local_overlay(raw).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].source, CatalogSource::Local);
        assert_eq!(warnings.len(), 1);
        let (empty, _) = parse_local_overlay(DEFAULT_OVERLAY).unwrap();
        assert!(empty.is_empty());
    }
}
