use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

pub const HOST_PROTOCOL: u32 = 2;
pub const CONFIGURE_SCHEMA_VERSION: u32 = 1;
pub const MAX_IPC_LINE_BYTES: usize = 64 * 1024;
pub const MAX_STRING_BYTES: usize = 2048;
pub const MAX_ID_BYTES: usize = 128;
pub const MAX_MIME_BYTES: usize = 128;
pub const MAX_DISPLAY_KEYS: usize = 64;
pub const MAX_CAPABILITIES: usize = 32;
pub const MAX_CAPABILITY_VALUES: usize = 64;
pub const MAX_MEDIA_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolRequest {
    pub id: String,
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProtocolResponse {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub plugin_protocol: u32,
    pub plugin_id: String,
    pub version: String,
    pub capabilities: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureMedia {
    pub url: String,
    pub kind: String,
    pub mime_type: String,
    pub sha256: String,
    pub byte_size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureParams {
    pub schema_version: u32,
    pub revision: String,
    pub media: ConfigureMedia,
    pub display: Value,
}

pub fn encode_request(id: &str, cmd: &str, params: Option<&Value>) -> Result<String, String> {
    limit_string("id", id, MAX_ID_BYTES)?;
    limit_string("cmd", cmd, 64)?;
    let request = ProtocolRequest {
        id: id.to_string(),
        cmd: cmd.to_string(),
        params: params.cloned(),
    };
    let mut line = serde_json::to_string(&request).map_err(|error| error.to_string())?;
    if line.len() > MAX_IPC_LINE_BYTES {
        return Err(format!("IPC 请求超过 {MAX_IPC_LINE_BYTES} 字节上限。"));
    }
    line.push('\n');
    Ok(line)
}

#[cfg(test)]
pub fn decode_response(line: &str) -> Result<Value, String> {
    decode_response_for(line, None)
}

pub fn decode_response_for(line: &str, expected_id: Option<&str>) -> Result<Value, String> {
    if line.len() > MAX_IPC_LINE_BYTES {
        return Err(format!("IPC 响应超过 {MAX_IPC_LINE_BYTES} 字节上限。"));
    }
    let value: ProtocolResponse =
        serde_json::from_str(line.trim()).map_err(|error| format!("无效插件响应：{error}"))?;
    if expected_id.is_some_and(|expected| value.id != expected) {
        return Err("插件响应 id 与请求不一致。".to_string());
    }
    if value.ok {
        Ok(value.result.unwrap_or(Value::Null))
    } else {
        Err(value
            .error
            .filter(|error| !error.is_empty() && error.len() <= MAX_STRING_BYTES)
            .unwrap_or_else(|| "插件命令失败。".to_string()))
    }
}

pub fn parse_hello(value: &Value) -> Result<HelloResult, String> {
    let plugin_protocol = required_u32(value, "pluginProtocol")?;
    if plugin_protocol != HOST_PROTOCOL {
        return Err(format!(
            "hello 声明的 pluginProtocol={plugin_protocol}，宿主当前需要 2。"
        ));
    }
    let plugin_id = required_string(value, "pluginId", MAX_ID_BYTES)?;
    let version = required_string(value, "version", 64)?;
    let capabilities = value
        .get("capabilities")
        .cloned()
        .ok_or_else(|| "hello 缺少 capabilities。".to_string())?;
    validate_capabilities(&capabilities, "hello.capabilities")?;
    Ok(HelloResult {
        plugin_protocol,
        plugin_id,
        version,
        capabilities,
    })
}

pub fn validate_media_url(url: &str) -> Result<Url, String> {
    limit_string("media.url", url, MAX_STRING_BYTES)?;
    let parsed = Url::parse(url).map_err(|_| "媒体 URL 无效。".to_string())?;
    if parsed.scheme() != "http" {
        return Err("媒体 URL 必须是 http 回环地址。".to_string());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "媒体 URL 缺少主机名。".to_string())?;
    if host != "127.0.0.1" && !host.eq_ignore_ascii_case("localhost") {
        return Err("媒体 URL 必须指向 127.0.0.1 或 localhost。".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("媒体 URL 不能包含账号信息。".to_string());
    }
    Ok(parsed)
}

pub fn validate_configure_media(media: &ConfigureMedia) -> Result<(), String> {
    validate_media_url(&media.url)?;
    if media.kind != "image" && media.kind != "video" {
        return Err("media.kind 只能是 image 或 video。".to_string());
    }
    limit_string("media.mimeType", &media.mime_type, MAX_MIME_BYTES)?;
    if media.mime_type.is_empty() {
        return Err("media.mimeType 不能为空。".to_string());
    }
    limit_string("media.sha256", &media.sha256, 64)?;
    if media.sha256.len() != 64 || !media.sha256.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err("media.sha256 必须是 64 位十六进制。".to_string());
    }
    if media.byte_size < 1 || media.byte_size > MAX_MEDIA_BYTES {
        return Err("media.byteSize 超出允许范围。".to_string());
    }
    Ok(())
}

pub fn build_configure_params(
    revision: String,
    media: ConfigureMedia,
    display: Value,
) -> Result<Value, String> {
    if revision.is_empty() || revision.len() > 128 {
        return Err("configure.revision 无效。".to_string());
    }
    validate_configure_media(&media)?;
    validate_display_object(&display)?;
    let params = ConfigureParams {
        schema_version: CONFIGURE_SCHEMA_VERSION,
        revision,
        media,
        display,
    };
    let value = serde_json::to_value(params).map_err(|error| error.to_string())?;
    let encoded = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_IPC_LINE_BYTES - 256 {
        return Err("configure 参数超过长度上限。".to_string());
    }
    Ok(value)
}

pub fn revision_digest(media_sha256: &str, display: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(media_sha256.as_bytes());
    hasher.update(b"\0");
    hasher.update(serde_json::to_vec(display).unwrap_or_default());
    format!("{:x}", hasher.finalize())
}

pub fn validate_display_object(display: &Value) -> Result<(), String> {
    let object = display
        .as_object()
        .ok_or_else(|| "display 必须是对象。".to_string())?;
    if object.len() > MAX_DISPLAY_KEYS {
        return Err(format!("display 字段数超过 {MAX_DISPLAY_KEYS}。"));
    }
    for (key, value) in object {
        limit_string("display key", key, 64)?;
        match value {
            Value::Null | Value::Bool(_) => {}
            Value::Number(number) => {
                let parsed = number
                    .as_f64()
                    .ok_or_else(|| format!("display.{key} 数字无效。"))?;
                if !parsed.is_finite() {
                    return Err(format!("display.{key} 必须是有限数字。"));
                }
            }
            Value::String(text) => {
                limit_string(&format!("display.{key}"), text, 128)?;
            }
            Value::Array(_) | Value::Object(_) => {
                return Err(format!("display.{key} 不能是嵌套结构。"));
            }
        }
    }
    Ok(())
}

pub fn validate_capabilities(value: &Value, label: &str) -> Result<(), String> {
    let capabilities = value
        .as_object()
        .ok_or_else(|| format!("{label} 必须是对象。"))?;
    if capabilities.is_empty() {
        return Err(format!("{label} 不能为空。"));
    }
    if capabilities.len() > MAX_CAPABILITIES {
        return Err(format!("{label} 超过 {MAX_CAPABILITIES} 项。"));
    }
    for (key, value) in capabilities {
        limit_string(&format!("{label} key"), key, 64)?;
        if key.is_empty()
            || !key
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err(format!("{label}.{key} 的名称无效。"));
        }
        match value {
            Value::Bool(_) => {}
            Value::Number(number) => {
                let number = number
                    .as_u64()
                    .ok_or_else(|| format!("{label}.{key} 必须是非负整数。"))?;
                if number > MAX_MEDIA_BYTES {
                    return Err(format!("{label}.{key} 超出允许范围。"));
                }
            }
            Value::String(text) => limit_string(&format!("{label}.{key}"), text, 128)?,
            Value::Array(items) => {
                if items.len() > MAX_CAPABILITY_VALUES {
                    return Err(format!("{label}.{key} 超过 {MAX_CAPABILITY_VALUES} 项。"));
                }
                for item in items {
                    let text = item
                        .as_str()
                        .ok_or_else(|| format!("{label}.{key} 必须是字符串数组。"))?;
                    limit_string(&format!("{label}.{key}[]"), text, 128)?;
                }
            }
            Value::Null | Value::Object(_) => {
                return Err(format!("{label}.{key} 的值类型无效。"));
            }
        }
    }
    Ok(())
}

pub fn capability_labels(value: &Value) -> Vec<String> {
    let Some(capabilities) = value.as_object() else {
        return Vec::new();
    };
    capabilities
        .iter()
        .filter_map(|(key, value)| {
            let enabled = match value {
                Value::Bool(enabled) => *enabled,
                Value::Number(number) => number.as_u64().is_some_and(|number| number > 0),
                Value::String(text) => !text.is_empty(),
                Value::Array(items) => !items.is_empty(),
                _ => false,
            };
            enabled.then(|| key.clone())
        })
        .collect()
}

fn required_u32(value: &Value, key: &str) -> Result<u32, String> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .filter(|number| *number <= u32::MAX as u64)
        .map(|number| number as u32)
        .ok_or_else(|| format!("hello 缺少有效的 {key}。"))
}

fn required_string(value: &Value, key: &str, max_len: usize) -> Result<String, String> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("hello 缺少 {key}。"))?;
    limit_string(key, text, max_len)?;
    if text.is_empty() {
        return Err(format!("hello.{key} 不能为空。"));
    }
    Ok(text.to_string())
}

fn limit_string(label: &str, value: &str, max_len: usize) -> Result<(), String> {
    if value.len() > max_len {
        return Err(format!("{label} 超过 {max_len} 字节上限。"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_v2_request_with_params_and_v1_without() {
        let with_params =
            encode_request("req-1", "configure", Some(&json!({ "schemaVersion": 1 }))).unwrap();
        assert!(with_params.ends_with('\n'));
        let parsed: ProtocolRequest = serde_json::from_str(with_params.trim()).unwrap();
        assert_eq!(parsed.cmd, "configure");
        assert_eq!(parsed.params.unwrap()["schemaVersion"], 1);

        let v1 = encode_request("req-2", "status", None).unwrap();
        let parsed: Value = serde_json::from_str(v1.trim()).unwrap();
        assert_eq!(parsed["cmd"], "status");
        assert!(parsed.get("params").is_none());
    }

    #[test]
    fn decodes_ok_and_error_responses() {
        let ok = decode_response(r#"{"id":"1","ok":true,"result":{"phase":"idle"}}"#).unwrap();
        assert_eq!(ok["phase"], "idle");
        assert!(
            decode_response_for(r#"{"id":"other","ok":true,"result":{}}"#, Some("expected"))
                .is_err()
        );
        let error = decode_response(r#"{"id":"1","ok":false,"error":"boom"}"#).unwrap_err();
        assert_eq!(error, "boom");
    }

    #[test]
    fn hello_requires_protocol_fields() {
        let hello = parse_hello(&json!({
            "pluginProtocol": 2,
            "pluginId": "codex",
            "version": "1.0.0",
            "capabilities": {
                "mediaKinds": ["image", "video"],
                "hotUpdate": true
            }
        }))
        .unwrap();
        assert_eq!(hello.plugin_id, "codex");
        assert_eq!(
            capability_labels(&hello.capabilities),
            vec!["hotUpdate", "mediaKinds"]
        );
        assert!(parse_hello(&json!({
            "pluginProtocol": 1,
            "pluginId": "codex",
            "version": "1.0.0",
            "capabilities": { "hotUpdate": true }
        }))
        .is_err());
        assert!(parse_hello(&json!({
            "pluginProtocol": 2,
            "pluginId": "codex",
            "version": "1.0.0"
        }))
        .is_err());
    }

    #[test]
    fn rejects_non_loopback_and_oversized_configure() {
        let media = ConfigureMedia {
            url: "http://example.com/media/1".to_string(),
            kind: "image".to_string(),
            mime_type: "image/png".to_string(),
            sha256: "a".repeat(64),
            byte_size: 12,
        };
        assert!(validate_configure_media(&media).is_err());
        assert!(validate_media_url("https://127.0.0.1/x").is_err());
        assert!(validate_media_url("http://127.0.0.1/ok").is_ok());
        assert!(validate_media_url("http://localhost/ok").is_ok());
        assert!(limit_string("x", &"a".repeat(MAX_STRING_BYTES + 1), MAX_STRING_BYTES).is_err());
    }
}
