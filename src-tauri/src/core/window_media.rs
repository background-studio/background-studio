//! Per-window playback is owned by the host, not by scripts inside Cursor.
//! Only negotiated workers receive targeted configure messages.
use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    media::{MediaLibrary, ResolvedMedia},
    media_server::is_safe_media_id,
    profile::sanitize_display,
    protocol::{build_configure_params, revision_digest, ConfigureMedia},
    HostCore, MediaKind, SlideshowOrder,
};

#[derive(Clone)]
pub(super) struct WindowMedia {
    media: ResolvedMedia,
    source: Value,
    display: Value,
    changed_at: Instant,
    pub dirty: bool,
    pub refresh: bool,
}

impl WindowMedia {
    fn due(&self, now: Instant, enabled: bool, interval: u64) -> bool {
        self.refresh
            || (enabled
                && now.saturating_duration_since(self.changed_at) >= Duration::from_secs(interval))
    }
}

/// Prefer a different, unused file. When there are fewer files than windows,
/// still rotate rather than getting permanently stuck on the current file.
fn choose_path<'a>(
    paths: &'a [PathBuf],
    previous: Option<&PathBuf>,
    occupied: &HashSet<PathBuf>,
    order: SlideshowOrder,
    seed: u128,
) -> Option<&'a PathBuf> {
    if paths.is_empty() {
        return None;
    }
    let start = match order {
        SlideshowOrder::Random => (seed % paths.len() as u128) as usize,
        SlideshowOrder::Sequential => previous
            .and_then(|p| paths.iter().position(|v| v == p))
            .map(|i| (i + 1) % paths.len())
            .unwrap_or(0),
    };
    let ordered = (0..paths.len())
        .map(|offset| &paths[(start + offset) % paths.len()])
        .collect::<Vec<_>>();
    ordered
        .iter()
        .copied()
        .find(|p| Some(*p) != previous && !occupied.contains(*p))
        .or_else(|| ordered.iter().copied().find(|p| Some(*p) != previous))
        .or_else(|| ordered.first().copied())
}

fn target_ids(status: &Value, name: &str) -> Result<Vec<String>, String> {
    let values = status
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("窗口状态缺少 {name}。"))?;
    let mut ids = Vec::new();
    for value in values {
        let id = value
            .as_str()
            .filter(|id| is_safe_media_id(id))
            .ok_or_else(|| "窗口状态包含无效 targetId。".to_string())?;
        if !ids.iter().any(|v| v == id) {
            ids.push(id.to_string());
        }
    }
    ids.sort();
    Ok(ids)
}

impl HostCore {
    pub(super) fn supports_window_media(&self, id: &str) -> bool {
        self.hello.get(id).is_some_and(|hello| {
            hello
                .capabilities
                .get("perWindowMedia")
                .and_then(Value::as_bool)
                == Some(true)
        })
    }

    pub(super) fn clear_window_media(&mut self, id: &str) {
        if let Some(windows) = self.window_media.remove(id) {
            for target in windows.keys() {
                self.media_server.remove_window(id, target);
            }
        }
    }

    pub(super) async fn tick_window_media(&mut self) -> Result<(), String> {
        let ids = self
            .profiles
            .keys()
            .filter(|id| self.supports_window_media(id))
            .cloned()
            .collect::<Vec<_>>();
        let mut error = None;
        for id in ids {
            if !self.plugins.is_enabled(&id) || !self.plugins.is_running(&id) {
                self.clear_window_media(&id);
                continue;
            }
            if let Err(message) = self.sync_window_media(&id).await {
                error.get_or_insert(message);
            }
        }
        error.map_or(Ok(()), Err)
    }

    async fn sync_window_media(&mut self, id: &str) -> Result<(), String> {
        let profile = self
            .profiles
            .get(id)
            .cloned()
            .ok_or_else(|| "插件没有 Profile。".to_string())?;
        let Some(active) = &profile.active_media_id else {
            self.clear_window_media(id);
            return Ok(());
        };
        let mut status = self.plugins.plugin_request(id, "status", None).await?;
        // Polling must never wake a paused/restored worker.
        if status.get("paused").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        if status.get("independentWindows").and_then(Value::as_bool) != Some(true) {
            status = self.configure_profile(id).await?;
        }
        let targets = target_ids(&status, "targetIds")?;
        let configured = target_ids(&status, "configuredTargetIds")?;
        if let Some(windows) = self.window_media.get_mut(id) {
            windows.retain(|target, _| {
                if targets.contains(target) {
                    true
                } else {
                    self.media_server.remove_window(id, target);
                    false
                }
            });
        }
        if targets.is_empty() {
            return Ok(());
        }
        let sources = if !profile.slideshow.enabled || profile.playlist_ids.is_empty() {
            vec![active.clone()]
        } else {
            profile.playlist_ids.clone()
        };
        let source = json!({"active": active, "playlist": sources});
        let display = sanitize_display(&profile.display, Some(&self.settings_schema(id)));
        let now = Instant::now();
        let needs_update = targets.iter().any(|target| {
            self.window_media
                .get(id)
                .and_then(|windows| windows.get(target))
                .map_or(true, |old| {
                    old.dirty
                        || old.source != source
                        || old.display != display
                        || !configured.contains(target)
                        || old.due(
                            now,
                            profile.slideshow.enabled,
                            profile.slideshow.interval_seconds,
                        )
                })
        });
        if !needs_update {
            return Ok(());
        }
        let paths = self.library.window_candidates(&sources)?;
        let maximum = self.max_media_bytes(id);
        let mut first_error = None;
        for target in targets {
            let now = Instant::now();
            let previous = self
                .window_media
                .get(id)
                .and_then(|windows| windows.get(&target))
                .cloned();
            let advance = previous.as_ref().map_or(true, |old| {
                old.source != source
                    || !paths.contains(&old.media.path)
                    || old.due(
                        now,
                        profile.slideshow.enabled,
                        profile.slideshow.interval_seconds,
                    )
            });
            let dirty = previous
                .as_ref()
                .map_or(true, |old| old.dirty || old.display != display);
            if !advance && !dirty && configured.contains(&target) {
                continue;
            }
            let occupied = self
                .window_media
                .get(id)
                .into_iter()
                .flat_map(|windows| windows.iter())
                .filter(|(other, _)| *other != &target)
                .map(|(_, state)| state.media.path.clone())
                .collect::<HashSet<_>>();
            let path = if advance {
                choose_path(
                    &paths,
                    previous.as_ref().map(|p| &p.media.path),
                    &occupied,
                    profile.slideshow.order,
                    Uuid::new_v4().as_u128(),
                )
                .cloned()
                .ok_or_else(|| "图库没有可用媒体。".to_string())?
            } else {
                previous.as_ref().unwrap().media.path.clone()
            };
            let result = async {
                let media = if advance || dirty {
                    MediaLibrary::resolve_folder_file(path)?
                } else {
                    previous.as_ref().unwrap().media.clone()
                };
                if media.byte_size > maximum {
                    return Err(format!(
                        "窗口背景超过插件允许的 {} MiB。",
                        maximum / 1024 / 1024
                    ));
                }
                let url = self
                    .media_server
                    .configure_window_url_for(id, &target, &media)?;
                let configured_media = ConfigureMedia {
                    url,
                    kind: if media.kind == MediaKind::Image {
                        "image"
                    } else {
                        "video"
                    }
                    .to_string(),
                    mime_type: media.mime_type.clone(),
                    sha256: media.sha256.clone(),
                    byte_size: media.byte_size,
                };
                let mut params = build_configure_params(
                    revision_digest(&media.sha256, &display),
                    configured_media,
                    display.clone(),
                )?;
                params["independentWindows"] = json!(true);
                params["targetId"] = json!(target);
                self.plugins
                    .plugin_request(id, "configure", Some(params))
                    .await?;
                self.window_media.entry(id.to_string()).or_default().insert(
                    target.clone(),
                    WindowMedia {
                        media,
                        source: source.clone(),
                        display: display.clone(),
                        changed_at: if advance {
                            Instant::now()
                        } else {
                            previous.as_ref().unwrap().changed_at
                        },
                        dirty: false,
                        refresh: false,
                    },
                );
                Ok::<_, String>(())
            }
            .await;
            if let Err(error) = result {
                first_error.get_or_insert(format!("窗口 {target}：{error}"));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_window_avoids_existing_choices_and_does_not_mutate_them() {
        let paths = ["a", "b", "c"].map(PathBuf::from);
        let occupied = HashSet::from([paths[0].clone(), paths[1].clone()]);
        for seed in 0..12 {
            assert_eq!(
                choose_path(&paths, None, &occupied, SlideshowOrder::Random, seed),
                Some(&paths[2])
            );
        }
        assert_eq!(occupied.len(), 2);
    }

    #[test]
    fn rotation_is_independent_and_handles_small_libraries() {
        let paths = ["a", "b", "c"].map(PathBuf::from);
        let occupied = HashSet::from([paths[1].clone()]);
        assert_eq!(
            choose_path(
                &paths,
                Some(&paths[0]),
                &occupied,
                SlideshowOrder::Sequential,
                0
            ),
            Some(&paths[2])
        );
        assert_eq!(
            choose_path(
                &paths[..2],
                Some(&paths[0]),
                &occupied,
                SlideshowOrder::Random,
                0
            ),
            Some(&paths[1])
        );
        assert_eq!(
            choose_path(
                &paths[..1],
                Some(&paths[0]),
                &occupied,
                SlideshowOrder::Random,
                0
            ),
            Some(&paths[0])
        );
        assert!(choose_path(&[], None, &occupied, SlideshowOrder::Random, 0).is_none());
    }

    #[test]
    fn each_window_has_its_own_deadline_and_disabled_slideshow_stays_still() {
        let now = Instant::now();
        let window = |changed_at| WindowMedia {
            media: ResolvedMedia {
                path: "a".into(),
                mime_type: "image/png".into(),
                byte_size: 1,
                kind: MediaKind::Image,
                sha256: "a".repeat(64),
            },
            source: json!({}),
            display: json!({}),
            changed_at,
            dirty: false,
            refresh: false,
        };
        let a = window(now - Duration::from_secs(30));
        let b = window(now - Duration::from_secs(10));
        assert!(a.due(now, true, 20));
        assert!(!b.due(now, true, 20));
        assert!(!a.due(now, false, 20));
    }

    #[test]
    fn target_status_requires_safe_unique_ids() {
        assert_eq!(
            target_ids(&json!({"targetIds": ["B", "A", "A"]}), "targetIds").unwrap(),
            ["A", "B"]
        );
        for status in [
            json!({}),
            json!({"targetIds": ["../bad"]}),
            json!({"targetIds": [1]}),
        ] {
            assert!(target_ids(&status, "targetIds").is_err());
        }
    }
}
