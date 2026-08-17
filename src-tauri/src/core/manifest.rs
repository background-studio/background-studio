use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::protocol::{validate_capabilities, HOST_PROTOCOL};

const MAX_ID: usize = 64;
const MAX_NAME: usize = 120;
const MAX_EXE: usize = 180;
const MAX_PIPE: usize = 180;
const MAX_MANIFEST_BYTES: usize = 128 * 1024;
const MAX_SETTINGS_FIELDS: usize = 64;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub schema_version: u32,
    pub plugin_protocol: u32,
    pub id: String,
    pub display_name: String,
    pub exe_name: String,
    pub pipe_name: String,
    pub capabilities: Value,
    pub settings_schema: Value,
}

impl PluginManifest {
    pub fn exe_path(&self, install_dir: &Path) -> PathBuf {
        if let Some(parent) = find_manifest_file(install_dir)
            .ok()
            .flatten()
            .and_then(|path| path.parent().map(Path::to_path_buf))
        {
            return parent.join(&self.exe_name);
        }
        install_dir.join(&self.exe_name)
    }
}

pub fn parse_manifest(raw: &str) -> Result<PluginManifest, String> {
    if raw.len() > MAX_MANIFEST_BYTES {
        return Err(format!("plugin.json 超过 {MAX_MANIFEST_BYTES} 字节上限。"));
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|error| format!("plugin.json 不是合法 JSON：{error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "plugin.json 必须是对象。".to_string())?;
    for required in [
        "schemaVersion",
        "pluginProtocol",
        "id",
        "displayName",
        "exeName",
        "pipeName",
        "capabilities",
        "settingsSchema",
    ] {
        if !object.contains_key(required) {
            return Err(format!("plugin.json 缺少 {required}。"));
        }
    }
    let manifest: PluginManifest = serde_json::from_value(value)
        .map_err(|error| format!("plugin.json 字段类型无效：{error}"))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub fn validate_manifest(manifest: &PluginManifest) -> Result<(), String> {
    if manifest.schema_version != 1 {
        return Err(format!(
            "plugin.json schemaVersion={} 不受支持。",
            manifest.schema_version
        ));
    }
    if manifest.plugin_protocol != 1 && manifest.plugin_protocol != HOST_PROTOCOL {
        return Err(format!(
            "plugin.json pluginProtocol={} 不受支持。",
            manifest.plugin_protocol
        ));
    }
    validate_id(&manifest.id)?;
    validate_len("displayName", &manifest.display_name, MAX_NAME, true)?;
    validate_exe_name(&manifest.exe_name)?;
    validate_pipe_name(&manifest.pipe_name)?;
    validate_capabilities(&manifest.capabilities, "plugin.json capabilities")?;
    validate_settings_schema(&manifest.settings_schema)?;
    Ok(())
}

pub fn find_manifest_file(install_dir: &Path) -> Result<Option<PathBuf>, String> {
    let root = install_dir.join("plugin.json");
    if root.is_file() {
        return Ok(Some(root));
    }
    let mut found = Vec::new();
    if install_dir.is_dir() {
        for entry in fs::read_dir(install_dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                continue;
            }
            let candidate = entry.path().join("plugin.json");
            if candidate.is_file() {
                found.push(candidate);
            }
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found.remove(0))),
        _ => Err("插件包包含多个 plugin.json。".to_string()),
    }
}

pub fn load_from_install_dir(install_dir: &Path) -> Result<Option<PluginManifest>, String> {
    let Some(path) = find_manifest_file(install_dir)? else {
        return Ok(None);
    };
    let metadata =
        fs::metadata(&path).map_err(|error| format!("读取 plugin.json 失败：{error}"))?;
    if metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(format!("plugin.json 超过 {MAX_MANIFEST_BYTES} 字节上限。"));
    }
    let raw =
        fs::read_to_string(&path).map_err(|error| format!("读取 plugin.json 失败：{error}"))?;
    Ok(Some(parse_manifest(&raw)?))
}

fn validate_settings_schema(value: &Value) -> Result<(), String> {
    let schema = value
        .as_object()
        .ok_or_else(|| "plugin.json settingsSchema 必须是对象。".to_string())?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("plugin.json settingsSchema.type 必须是 object。".to_string());
    }
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| "plugin.json settingsSchema.properties 必须是对象。".to_string())?;
    if properties.len() > MAX_SETTINGS_FIELDS {
        return Err(format!(
            "plugin.json settingsSchema.properties 超过 {MAX_SETTINGS_FIELDS} 项。"
        ));
    }
    for (key, field) in properties {
        validate_len("settingsSchema.properties key", key, 64, true)?;
        let field = field
            .as_object()
            .ok_or_else(|| format!("settingsSchema.properties.{key} 必须是对象。"))?;
        let field_type = field
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("settingsSchema.properties.{key}.type 缺失。"))?;
        if !matches!(field_type, "boolean" | "number" | "string") {
            return Err(format!("settingsSchema.properties.{key}.type 不受支持。"));
        }
        if let Some(title) = field.get("title") {
            let title = title
                .as_str()
                .ok_or_else(|| format!("settingsSchema.properties.{key}.title 必须是字符串。"))?;
            validate_len("settingsSchema title", title, 120, false)?;
        }
        for number_key in ["minimum", "maximum", "step"] {
            if let Some(number) = field.get(number_key) {
                let number = number.as_f64().ok_or_else(|| {
                    format!("settingsSchema.properties.{key}.{number_key} 必须是数字。")
                })?;
                if !number.is_finite() || (number_key == "step" && number <= 0.0) {
                    return Err(format!(
                        "settingsSchema.properties.{key}.{number_key} 无效。"
                    ));
                }
            }
        }
        if let Some(options) = field.get("enum") {
            let options = options
                .as_array()
                .ok_or_else(|| format!("settingsSchema.properties.{key}.enum 必须是数组。"))?;
            if options.is_empty() || options.len() > 32 {
                return Err(format!("settingsSchema.properties.{key}.enum 项数无效。"));
            }
            for option in options {
                let option = option.as_str().ok_or_else(|| {
                    format!("settingsSchema.properties.{key}.enum 必须是字符串数组。")
                })?;
                validate_len("settingsSchema enum", option, 128, true)?;
            }
        }
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), String> {
    validate_len("id", id, MAX_ID, true)?;
    if !id
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err("plugin.json id 只能包含小写字母、数字和连字符。".to_string());
    }
    Ok(())
}

fn validate_exe_name(name: &str) -> Result<(), String> {
    validate_len("exeName", name, MAX_EXE, true)?;
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("plugin.json exeName 不能包含路径。".to_string());
    }
    Ok(())
}

fn validate_pipe_name(name: &str) -> Result<(), String> {
    validate_len("pipeName", name, MAX_PIPE, true)?;
    let normalized = name.replace('/', "\\");
    if !normalized.starts_with(r"\\.\pipe\") {
        return Err(r"plugin.json pipeName 必须以 \\.\pipe\ 开头。".to_string());
    }
    let suffix = &normalized[r"\\.\pipe\".len()..];
    if suffix.is_empty() || suffix.contains('\\') || suffix.contains('/') || suffix.contains("..") {
        return Err("plugin.json pipeName 非法。".to_string());
    }
    Ok(())
}

fn validate_len(label: &str, value: &str, max: usize, required: bool) -> Result<(), String> {
    if required && value.trim().is_empty() {
        return Err(format!("plugin.json {label} 不能为空。"));
    }
    if value.len() > max {
        return Err(format!("plugin.json {label} 超过 {max} 字节。"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_json() -> String {
        serde_json::to_string(&json!({
            "schemaVersion": 1,
            "pluginProtocol": 2,
            "id": "codex",
            "displayName": "Codex Background Studio",
            "exeName": "Codex Background Studio.exe",
            "pipeName": r"\\.\pipe\background-studio-codex",
            "capabilities": {
                "mediaKinds": ["image", "video"],
                "hotUpdate": true,
                "managedLaunch": true
            },
            "settingsSchema": {
                "type": "object",
                "properties": {
                    "opacity": { "type": "number", "title": "不透明度", "minimum": 0, "maximum": 1 }
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn accepts_complete_manifest() {
        let manifest = parse_manifest(&valid_json()).unwrap();
        assert_eq!(manifest.id, "codex");
        assert_eq!(manifest.plugin_protocol, 2);
        assert!(manifest.settings_schema.is_object());
    }

    #[test]
    fn rejects_missing_and_invalid_fields() {
        assert!(parse_manifest(r#"{"id":"codex"}"#).is_err());
        let mut value: Value = serde_json::from_str(&valid_json()).unwrap();
        value["settingsSchema"] = json!(["nope"]);
        assert!(parse_manifest(&value.to_string()).is_err());
        value = serde_json::from_str(&valid_json()).unwrap();
        value["exeName"] = json!(r"..\evil.exe");
        assert!(parse_manifest(&value.to_string()).is_err());
        value = serde_json::from_str(&valid_json()).unwrap();
        value.as_object_mut().unwrap().remove("capabilities");
        assert!(parse_manifest(&value.to_string()).is_err());
    }
}
