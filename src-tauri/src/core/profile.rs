use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::{
    models::SlideshowOrder,
    persist::write_json_transaction,
    protocol::{validate_display_object, MAX_DISPLAY_KEYS, MAX_ID_BYTES},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SlideshowSettings {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub order: SlideshowOrder,
}

impl Default for SlideshowSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 300,
            order: SlideshowOrder::Sequential,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginProfile {
    pub schema_version: u32,
    pub active_media_id: Option<String>,
    pub playlist_ids: Vec<String>,
    pub display: Value,
    pub slideshow: SlideshowSettings,
}

impl Default for PluginProfile {
    fn default() -> Self {
        Self {
            schema_version: 1,
            active_media_id: None,
            playlist_ids: Vec::new(),
            display: json!({}),
            slideshow: SlideshowSettings::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlideshowPatch {
    pub enabled: Option<bool>,
    pub interval_seconds: Option<f64>,
    pub order: Option<SlideshowOrder>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePatch {
    pub active_media_id: Option<Option<String>>,
    pub playlist_ids: Option<Vec<String>>,
    pub display: Option<Value>,
    pub slideshow: Option<SlideshowPatch>,
}

pub fn default_display() -> Value {
    json!({
        "fit": "cover",
        "positionX": 50.0,
        "positionY": 50.0,
        "opacity": 0.72,
        "blur": 0.0,
        "scale": 1.0,
        "overlayColor": "#101416",
        "overlayOpacity": 0.12,
        "blockFillOpacity": 0.55,
        "homeIntensity": 1.0,
        "taskIntensity": 0.32,
        "sidebarOpacity": 0.18,
        "surfaceOpacity": 0.12,
        "cardOpacity": 0.82,
        "composerOpacity": 0.88,
        "menuOpacity": 0.9,
        "terminalOpacity": 0.9,
        "enabledOnHome": true,
        "enabledOnTasks": true,
        "videoMuted": true,
        "videoPlaybackRate": 1.0
    })
}

pub fn default_settings_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "fit": {
                "type": "string",
                "title": "填充方式",
                "enum": ["cover", "contain", "fill", "tile"],
                "enumLabels": ["覆盖", "适应", "拉伸", "平铺"]
            },
            "positionX": { "type": "number", "title": "水平位置", "minimum": 0, "maximum": 100, "step": 1 },
            "positionY": { "type": "number", "title": "垂直位置", "minimum": 0, "maximum": 100, "step": 1 },
            "opacity": { "type": "number", "title": "不透明度", "minimum": 0, "maximum": 1, "step": 0.01 },
            "blur": { "type": "number", "title": "模糊", "minimum": 0, "maximum": 40, "step": 1 },
            "scale": { "type": "number", "title": "缩放", "minimum": 1, "maximum": 1.3, "step": 0.01 },
            "overlayColor": { "type": "string", "title": "遮罩颜色", "format": "color" },
            "overlayOpacity": { "type": "number", "title": "遮罩不透明度", "minimum": 0, "maximum": 0.9, "step": 0.01 },
            "blockFillOpacity": { "type": "number", "title": "色块填充", "minimum": 0, "maximum": 1, "step": 0.01 },
            "homeIntensity": { "type": "number", "title": "主页强度", "minimum": 0, "maximum": 1, "step": 0.01 },
            "taskIntensity": { "type": "number", "title": "任务页强度", "minimum": 0, "maximum": 1, "step": 0.01 },
            "sidebarOpacity": { "type": "number", "title": "侧栏不透明度", "minimum": 0, "maximum": 1, "step": 0.01 },
            "surfaceOpacity": { "type": "number", "title": "表面不透明度", "minimum": 0, "maximum": 1, "step": 0.01 },
            "cardOpacity": { "type": "number", "title": "卡片不透明度", "minimum": 0, "maximum": 1, "step": 0.01 },
            "composerOpacity": { "type": "number", "title": "输入区不透明度", "minimum": 0, "maximum": 1, "step": 0.01 },
            "menuOpacity": { "type": "number", "title": "菜单不透明度", "minimum": 0, "maximum": 1, "step": 0.01 },
            "terminalOpacity": { "type": "number", "title": "终端不透明度", "minimum": 0, "maximum": 1, "step": 0.01 },
            "enabledOnHome": { "type": "boolean", "title": "主页启用" },
            "enabledOnTasks": { "type": "boolean", "title": "任务页启用" },
            "videoMuted": { "type": "boolean", "title": "视频静音" },
            "videoPlaybackRate": { "type": "number", "title": "视频倍速", "minimum": 0.25, "maximum": 2, "step": 0.05 }
        }
    })
}

pub fn profile_path(data_dir: &Path, plugin_id: &str) -> std::path::PathBuf {
    data_dir.join("profiles").join(format!("{plugin_id}.json"))
}

pub fn load_profile(
    data_dir: &Path,
    plugin_id: &str,
    schema: Option<&Value>,
) -> Result<PluginProfile, String> {
    let path = profile_path(data_dir, plugin_id);
    if !path.exists() {
        let mut profile = PluginProfile::default();
        // 让新档案的 display 直接落到该档案 schema 的默认值上。
        profile.display = sanitize_display(&profile.display, schema);
        write_json_transaction(&path, &profile)?;
        return Ok(profile);
    }
    let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let value: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
    Ok(normalize_profile_with_schema(&value, schema))
}

pub fn save_profile(
    data_dir: &Path,
    plugin_id: &str,
    profile: &PluginProfile,
) -> Result<(), String> {
    write_json_transaction(&profile_path(data_dir, plugin_id), profile)
}

pub fn normalize_profile(value: &Value) -> PluginProfile {
    normalize_profile_with_schema(value, None)
}

/// 按指定 schema 归一化档案；schema 为 None 时用插件默认 schema。
/// console 等自带 schema 的档案必须传入自己的 schema，否则专属字段会被洗掉。
pub fn normalize_profile_with_schema(value: &Value, schema: Option<&Value>) -> PluginProfile {
    let defaults = PluginProfile::default();
    let active_media_id = value
        .get("activeMediaId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
        .map(str::to_string);
    let mut playlist_ids = Vec::new();
    if let Some(values) = value.get("playlistIds").and_then(Value::as_array) {
        for id in values
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| id.len() <= MAX_ID_BYTES)
        {
            if !playlist_ids.iter().any(|existing| existing == id) {
                playlist_ids.push(id.to_string());
            }
        }
    }
    let slideshow = value.get("slideshow").unwrap_or(&Value::Null);
    let order = slideshow
        .get("order")
        .and_then(Value::as_str)
        .and_then(|order| match order {
            "sequential" => Some(SlideshowOrder::Sequential),
            "random" => Some(SlideshowOrder::Random),
            _ => None,
        })
        .unwrap_or(defaults.slideshow.order);
    PluginProfile {
        schema_version: 1,
        active_media_id,
        playlist_ids,
        display: sanitize_display(value.get("display").unwrap_or(&Value::Null), schema),
        slideshow: SlideshowSettings {
            enabled: slideshow
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(defaults.slideshow.enabled),
            interval_seconds: clamp_number(
                slideshow.get("intervalSeconds"),
                10.0,
                86_400.0,
                defaults.slideshow.interval_seconds as f64,
            )
            .round() as u64,
            order,
        },
    }
}

pub fn apply_patch(profile: &mut PluginProfile, patch: ProfilePatch) {
    if let Some(id) = patch.active_media_id {
        profile.active_media_id = id.filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES);
    }
    if let Some(ids) = patch.playlist_ids {
        profile.playlist_ids.clear();
        for id in ids.into_iter().filter(|id| id.len() <= MAX_ID_BYTES) {
            if !profile.playlist_ids.contains(&id) {
                profile.playlist_ids.push(id);
            }
        }
    }
    if let Some(display) = patch.display {
        let mut merged = profile.display.as_object().cloned().unwrap_or_default();
        if let Some(incoming) = display.as_object() {
            for (key, value) in incoming {
                merged.insert(key.clone(), value.clone());
            }
        }
        profile.display = Value::Object(merged);
    }
    if let Some(slideshow) = patch.slideshow {
        if let Some(enabled) = slideshow.enabled {
            profile.slideshow.enabled = enabled;
        }
        if let Some(interval) = slideshow.interval_seconds.filter(|value| value.is_finite()) {
            profile.slideshow.interval_seconds = interval.clamp(10.0, 86_400.0).round() as u64;
        }
        if let Some(order) = slideshow.order {
            profile.slideshow.order = order;
        }
    }
}

pub fn sanitize_display(value: &Value, schema: Option<&Value>) -> Value {
    let defaults = default_display();
    let default_map = defaults.as_object().cloned().unwrap_or_default();
    let incoming = value.as_object();
    let fallback_schema = default_settings_schema();
    let schema = schema.unwrap_or(&fallback_schema);
    let properties = schema.get("properties").and_then(Value::as_object);
    let mut output = Map::new();
    let keys: Vec<String> = if let Some(properties) = properties {
        properties.keys().cloned().collect()
    } else {
        default_map.keys().cloned().collect()
    };
    for key in keys.into_iter().take(MAX_DISPLAY_KEYS) {
        let schema_field = properties.and_then(|properties| properties.get(&key));
        let raw = incoming.and_then(|map| map.get(&key));
        let fallback = schema_field
            .and_then(|field| field.get("default"))
            .cloned()
            .or_else(|| default_map.get(&key).cloned())
            .unwrap_or(Value::Null);
        output.insert(key.clone(), sanitize_field(raw, schema_field, fallback));
    }
    if properties.is_none() {
        if let Some(incoming) = incoming {
            for (key, value) in incoming {
                if output.contains_key(key) {
                    continue;
                }
                if output.len() >= MAX_DISPLAY_KEYS {
                    break;
                }
                if let Ok(()) = validate_display_object(&json!({ key: value })) {
                    output.insert(key.clone(), value.clone());
                }
            }
        }
    }
    Value::Object(output)
}

fn sanitize_field(raw: Option<&Value>, schema: Option<&Value>, fallback: Value) -> Value {
    let field_type = schema
        .and_then(|schema| schema.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| match &fallback {
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            _ => "string",
        });
    match field_type {
        "boolean" => Value::Bool(
            raw.and_then(Value::as_bool)
                .unwrap_or(fallback.as_bool().unwrap_or(false)),
        ),
        "number" => {
            let minimum = schema
                .and_then(|schema| schema.get("minimum"))
                .and_then(Value::as_f64)
                .unwrap_or(f64::MIN);
            let maximum = schema
                .and_then(|schema| schema.get("maximum"))
                .and_then(Value::as_f64)
                .unwrap_or(f64::MAX);
            let fallback_number = fallback.as_f64().unwrap_or(0.0);
            json!(clamp_number(raw, minimum, maximum, fallback_number))
        }
        _ => {
            if let Some(options) = schema
                .and_then(|schema| schema.get("enum"))
                .and_then(Value::as_array)
            {
                let text = raw.and_then(Value::as_str);
                if let Some(text) = text {
                    if options.iter().any(|option| option.as_str() == Some(text)) {
                        return json!(text);
                    }
                }
                return fallback;
            }
            if schema
                .and_then(|schema| schema.get("format"))
                .and_then(Value::as_str)
                == Some("color")
            {
                return json!(as_hex_color(raw, fallback.as_str().unwrap_or("#101416")));
            }
            raw.and_then(Value::as_str)
                .filter(|text| text.len() <= 128)
                .map(|text| json!(text))
                .unwrap_or(fallback)
        }
    }
}

fn clamp_number(value: Option<&Value>, minimum: f64, maximum: f64, fallback: f64) -> f64 {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .filter(|value| value.is_finite())
        .unwrap_or(fallback)
        .clamp(minimum, maximum)
}

fn as_hex_color(value: Option<&Value>, fallback: &str) -> String {
    value
        .and_then(Value::as_str)
        .filter(|color| {
            color.len() == 7
                && color.starts_with('#')
                && color[1..]
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
        })
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| fallback.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn clamps_and_round_trips_profile() {
        let profile = normalize_profile(&json!({
            "activeMediaId": "背景-一号",
            "playlistIds": ["a", "a", "b"],
            "display": {
                "opacity": 9,
                "blur": -4,
                "overlayColor": "red",
                "fit": "cover"
            },
            "slideshow": { "intervalSeconds": 2, "order": "sideways" }
        }));
        assert_eq!(profile.active_media_id.as_deref(), Some("背景-一号"));
        assert_eq!(profile.playlist_ids, vec!["a", "b"]);
        assert_eq!(profile.display["opacity"], 1.0);
        assert_eq!(profile.display["blur"], 0.0);
        assert_eq!(profile.display["overlayColor"], "#101416");
        assert_eq!(profile.slideshow.interval_seconds, 10);

        let root = std::env::temp_dir().join(format!("host-profile-{}", Uuid::new_v4()));
        save_profile(&root, "codex", &profile).unwrap();
        let loaded = load_profile(&root, "codex", None).unwrap();
        assert_eq!(loaded.active_media_id, profile.active_media_id);
        assert_eq!(loaded.display["opacity"], 1.0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn applies_display_patch_without_dropping_known_fields() {
        let mut profile = PluginProfile::default();
        apply_patch(
            &mut profile,
            ProfilePatch {
                display: Some(json!({ "opacity": 0.2 })),
                ..ProfilePatch::default()
            },
        );
        assert_eq!(profile.display["opacity"], 0.2);
        assert!(profile.display.get("fit").is_none());
    }
}
