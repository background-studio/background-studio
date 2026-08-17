use serde::{Deserialize, Serialize};

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FitMode {
    #[default]
    Cover,
    Contain,
    Fill,
    Tile,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SlideshowOrder {
    #[default]
    Sequential,
    Random,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Video,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MediaOrigin {
    Local,
    Remote,
    Api,
    Folder,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaItem {
    pub id: String,
    pub name: String,
    pub kind: MediaKind,
    pub origin: MediaOrigin,
    pub file_name: String,
    pub mime_type: String,
    pub byte_size: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_count: Option<u64>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub added: Vec<MediaItem>,
    pub skipped: Vec<SkippedImport>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedImport {
    pub path: String,
    pub reason: String,
}
