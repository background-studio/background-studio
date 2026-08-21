use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    media::MediaLibrary,
    models::{MediaItem, MediaOrigin},
    persist::write_json_transaction,
    profile::{normalize_profile, profile_path, save_profile, PluginProfile},
};

pub const MIGRATION_MARKER: &str = "migration-standalone-v1.marker";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    pub migrated: bool,
    pub sources: Vec<String>,
    pub media_added: u32,
    pub profiles_written: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrationMarker {
    completed_at: String,
    sources: Vec<String>,
}

pub fn default_standalone_sources() -> Vec<(String, PathBuf)> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    vec![
        ("codex".to_string(), local.join("CodexBackgroundStudio")),
        ("notion".to_string(), local.join("NotionBackgroundStudio")),
        ("multica".to_string(), local.join("MulticaBackgroundStudio")),
    ]
}

pub fn migrate_standalone(
    host_dir: &Path,
    library: &mut MediaLibrary,
    sources: &[(String, PathBuf)],
) -> Result<MigrationReport, String> {
    let marker_path = host_dir.join(MIGRATION_MARKER);
    if marker_path.exists() {
        return Ok(MigrationReport {
            migrated: false,
            sources: Vec::new(),
            media_added: 0,
            profiles_written: Vec::new(),
        });
    }

    let mut report = MigrationReport {
        migrated: true,
        sources: Vec::new(),
        media_added: 0,
        profiles_written: Vec::new(),
    };
    let mut complete = true;

    for (plugin_id, source_dir) in sources {
        if !source_dir.is_dir() {
            continue;
        }
        report.sources.push(plugin_id.clone());
        let mut id_map: HashMap<String, String> = HashMap::new();
        let mut source_complete = true;
        let library_path = source_dir.join("library.json");
        if library_path.exists() {
            match fs::read_to_string(&library_path)
                .map_err(|error| error.to_string())
                .and_then(|raw| {
                    serde_json::from_str::<Vec<MediaItem>>(&raw).map_err(|error| error.to_string())
                }) {
                Ok(items) => {
                    for item in items {
                        match import_legacy_item(library, source_dir, item, &mut id_map) {
                            Ok(true) => report.media_added += 1,
                            Ok(false) => {}
                            Err(error) => {
                                source_complete = false;
                                eprintln!("迁移 {plugin_id} 媒体失败：{error}");
                            }
                        }
                    }
                }
                Err(error) => {
                    source_complete = false;
                    eprintln!("读取 {plugin_id} 旧媒体库失败：{error}");
                }
            }
        }

        let settings_path = source_dir.join("settings.json");
        let profile_file = profile_path(host_dir, plugin_id);
        if source_complete && settings_path.exists() && !profile_file.exists() {
            match fs::read_to_string(&settings_path)
                .map_err(|error| error.to_string())
                .and_then(|raw| {
                    serde_json::from_str::<Value>(&raw).map_err(|error| error.to_string())
                }) {
                Ok(value) => {
                    let mut profile = profile_from_legacy_settings(&value);
                    remap_profile_ids(&mut profile, &id_map);
                    save_profile(host_dir, plugin_id, &profile)?;
                    report.profiles_written.push(plugin_id.clone());
                }
                Err(error) => {
                    source_complete = false;
                    eprintln!("读取 {plugin_id} 旧设置失败：{error}");
                }
            }
        }
        complete &= source_complete;
    }

    if complete {
        write_json_transaction(
            &marker_path,
            &MigrationMarker {
                completed_at: Utc::now().to_rfc3339(),
                sources: report.sources.clone(),
            },
        )?;
    }
    Ok(report)
}

fn import_legacy_item(
    library: &mut MediaLibrary,
    source_dir: &Path,
    item: MediaItem,
    id_map: &mut HashMap<String, String>,
) -> Result<bool, String> {
    if item.origin == MediaOrigin::Folder {
        let Some(folder) = item.source_url.as_deref().map(Path::new) else {
            return Err("文件夹源缺少路径。".to_string());
        };
        if !folder.is_dir() {
            return Err(format!("文件夹源当前不可访问：{}", folder.display()));
        }
        if let Some(existing) = library.items().into_iter().find(|candidate| {
            candidate.origin == MediaOrigin::Folder && candidate.source_url == item.source_url
        }) {
            id_map.insert(item.id, existing.id);
            return Ok(false);
        }
        let imported = library.import_existing_file(folder, item.clone())?;
        if imported.id != item.id {
            id_map.insert(item.id, imported.id);
        }
        return Ok(true);
    }
    if let Some(existing) = library.find_by_sha256(&item.sha256) {
        id_map.insert(item.id, existing.id);
        return Ok(false);
    }
    let source = source_dir.join("media").join(&item.file_name);
    if !source.is_file() {
        return Err(format!("旧媒体文件不存在：{}", source.display()));
    }
    let old_id = item.id.clone();
    let imported = library.import_existing_file(&source, item)?;
    if imported.id != old_id {
        id_map.insert(old_id, imported.id);
    }
    Ok(true)
}

pub fn profile_from_legacy_settings(value: &Value) -> PluginProfile {
    normalize_profile(value)
}

fn remap_profile_ids(profile: &mut PluginProfile, id_map: &HashMap<String, String>) {
    if let Some(active) = profile.active_media_id.clone() {
        if let Some(mapped) = id_map.get(&active) {
            profile.active_media_id = Some(mapped.clone());
        }
    }
    profile.playlist_ids = profile
        .playlist_ids
        .iter()
        .map(|id| id_map.get(id).cloned().unwrap_or_else(|| id.clone()))
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::media::minimal_png;
    use serde_json::json;
    use std::io::Write;
    use uuid::Uuid;

    #[test]
    fn migrates_legacy_library_and_settings_once() {
        let root = std::env::temp_dir().join(format!("host-migrate-{}", Uuid::new_v4()));
        let host = root.join("host");
        let legacy = root.join("CodexBackgroundStudio");
        fs::create_dir_all(legacy.join("media")).unwrap();
        let png = minimal_png();
        let media_name = "aaaa1111-2222-3333-4444-555555555555.png";
        std::fs::File::create(legacy.join("media").join(media_name))
            .unwrap()
            .write_all(&png)
            .unwrap();
        let digest = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&png);
            format!("{:x}", hasher.finalize())
        };
        let item = json!([{
            "id": "aaaa1111-2222-3333-4444-555555555555",
            "name": "旧背景.png",
            "kind": "image",
            "origin": "local",
            "fileName": media_name,
            "mimeType": "image/png",
            "byteSize": png.len(),
            "sha256": digest,
            "createdAt": "2024-01-01T00:00:00Z"
        }]);
        fs::write(legacy.join("library.json"), item.to_string()).unwrap();
        fs::write(
            legacy.join("settings.json"),
            json!({
                "activeMediaId": "aaaa1111-2222-3333-4444-555555555555",
                "playlistIds": ["aaaa1111-2222-3333-4444-555555555555"],
                "display": { "opacity": 0.4, "blockFillOpacity": 0.2, "cardOpacity": 0.3 },
                "slideshow": { "enabled": true, "intervalSeconds": 12, "order": "random" }
            })
            .to_string(),
        )
        .unwrap();

        let mut library = MediaLibrary::load(&host).unwrap();
        let first = migrate_standalone(
            &host,
            &mut library,
            &[("codex".to_string(), legacy.clone())],
        )
        .unwrap();
        assert!(first.migrated);
        assert_eq!(first.media_added, 1);
        assert_eq!(first.profiles_written, vec!["codex".to_string()]);
        assert_eq!(library.items().len(), 1);
        let profile = crate::core::profile::load_profile(&host, "codex", None).unwrap();
        assert_eq!(
            profile.active_media_id.as_deref(),
            Some("aaaa1111-2222-3333-4444-555555555555")
        );
        assert_eq!(profile.display["opacity"], 0.4);
        assert_eq!(profile.display["blockFillOpacity"], 0.2);
        assert_eq!(profile.display["cardOpacity"], 0.3);
        assert!(legacy.join("settings.json").exists());

        let second = migrate_standalone(
            &host,
            &mut library,
            &[("codex".to_string(), legacy.clone())],
        )
        .unwrap();
        assert!(!second.migrated);
        assert_eq!(library.items().len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remaps_duplicate_sha256_to_existing_id() {
        let root = std::env::temp_dir().join(format!("host-migrate-dedup-{}", Uuid::new_v4()));
        let host = root.join("host");
        let mut library = MediaLibrary::load(&host).unwrap();
        let png = minimal_png();
        let source = root.join("one.png");
        fs::create_dir_all(&root).unwrap();
        std::fs::File::create(&source)
            .unwrap()
            .write_all(&png)
            .unwrap();
        let added = library.import_files(&[source]);
        let existing_id = added.added[0].id.clone();

        let legacy = root.join("NotionBackgroundStudio");
        fs::create_dir_all(legacy.join("media")).unwrap();
        let media_name = "bbbb1111-2222-3333-4444-555555555555.png";
        std::fs::File::create(legacy.join("media").join(media_name))
            .unwrap()
            .write_all(&png)
            .unwrap();
        fs::write(
            legacy.join("library.json"),
            json!([{
                "id": "bbbb1111-2222-3333-4444-555555555555",
                "name": "notion.png",
                "kind": "image",
                "origin": "local",
                "fileName": media_name,
                "mimeType": "image/png",
                "byteSize": png.len(),
                "sha256": added.added[0].sha256,
                "createdAt": "2024-01-01T00:00:00Z"
            }])
            .to_string(),
        )
        .unwrap();
        fs::write(
            legacy.join("settings.json"),
            json!({ "activeMediaId": "bbbb1111-2222-3333-4444-555555555555" }).to_string(),
        )
        .unwrap();

        let report =
            migrate_standalone(&host, &mut library, &[("notion".to_string(), legacy)]).unwrap();
        assert_eq!(report.media_added, 0);
        let profile = crate::core::profile::load_profile(&host, "notion", None).unwrap();
        assert_eq!(
            profile.active_media_id.as_deref(),
            Some(existing_id.as_str())
        );
        let _ = fs::remove_dir_all(root);
    }
}
