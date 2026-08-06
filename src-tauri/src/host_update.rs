use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

pub const HOST_OWNER: &str = "background-studio";
pub const HOST_REPO: &str = "background-studio";
pub const HOST_PROGRESS_EVENT: &str = "host:host-update-progress";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostReleaseInfo {
    pub latest_version: Option<String>,
    pub asset_name: Option<String>,
    pub release_url: Option<String>,
    pub download_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostUpdateProgress {
    pub phase: String,
    pub percent: Option<f64>,
    pub message: String,
}

pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn version_newer(latest: &str, current: &str) -> bool {
    let latest = latest.trim_start_matches('v');
    let current = current.trim_start_matches('v');
    latest != current && compare_semver(latest, current) == std::cmp::Ordering::Greater
}

fn compare_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |value: &str| -> Vec<u64> {
        value
            .split(|c: char| !c.is_ascii_digit())
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse().ok())
            .collect()
    };
    let left = parse(a);
    let right = parse(b);
    let len = left.len().max(right.len());
    for index in 0..len {
        let l = left.get(index).copied().unwrap_or(0);
        let r = right.get(index).copied().unwrap_or(0);
        match l.cmp(&r) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

pub fn format_github_api_error(status: u16, body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if status == 403 && lower.contains("rate limit") {
        return "GitHub API 限流（未登录约 60 次/小时/IP），请稍后点「检查更新」重试".to_string();
    }
    if status == 404 {
        return "GitHub 上找不到最新 Release（仓库可能尚未发布）。".to_string();
    }
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(message) = value.get("message").and_then(|m| m.as_str()) {
            if message.to_ascii_lowercase().contains("rate limit") {
                return "GitHub API 限流（未登录约 60 次/小时/IP），请稍后点「检查更新」重试"
                    .to_string();
            }
            return format!("GitHub API {status}：{message}");
        }
    }
    format!("GitHub API 请求失败（HTTP {status}）")
}

pub fn fetch_latest_host_release() -> Result<HostReleaseInfo, String> {
    let url = format!("https://api.github.com/repos/{HOST_OWNER}/{HOST_REPO}/releases/latest");
    let response = reqwest::blocking::Client::new()
        .get(url)
        .header("User-Agent", "BackgroundStudioHost/0.1")
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|error| format!("请求 GitHub 失败：{error}"))?;
    let status = response.status();
    let body = response.text().map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format_github_api_error(status.as_u16(), &body));
    }
    let value: Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
    let tag = value
        .get("tag_name")
        .and_then(|tag| tag.as_str())
        .ok_or_else(|| "壳 Release 缺少 tag_name".to_string())?;
    let version = tag.trim_start_matches('v').to_string();
    let html_url = value
        .get("html_url")
        .and_then(|url| url.as_str())
        .unwrap_or("https://github.com/background-studio/background-studio/releases")
        .to_string();
    let assets = value
        .get("assets")
        .and_then(|assets| assets.as_array())
        .ok_or_else(|| "壳 Release 缺少 assets".to_string())?;
    let asset = assets
        .iter()
        .find(|asset| {
            asset
                .get("name")
                .and_then(|name| name.as_str())
                .is_some_and(|name| {
                    name.starts_with("Background.Studio_") && name.ends_with("_x64-setup.exe")
                })
        })
        .ok_or_else(|| "壳 Release 中没有 Background.Studio_*_x64-setup.exe".to_string())?;
    let asset_name = asset
        .get("name")
        .and_then(|name| name.as_str())
        .ok_or_else(|| "壳安装包缺少名称".to_string())?
        .to_string();
    let download_url = asset
        .get("browser_download_url")
        .and_then(|url| url.as_str())
        .ok_or_else(|| "壳安装包缺少下载地址".to_string())?
        .to_string();
    Ok(HostReleaseInfo {
        latest_version: Some(version),
        asset_name: Some(asset_name),
        release_url: Some(html_url),
        download_url: Some(download_url),
    })
}

#[cfg(test)]
mod tests {
    use super::format_github_api_error;

    #[test]
    fn maps_rate_limit_to_chinese_hint() {
        let message = format_github_api_error(
            403,
            r#"{"message":"API rate limit exceeded for 1.2.3.4."}"#,
        );
        assert!(message.contains("限流"));
        assert!(message.contains("检查更新"));
    }

    #[test]
    fn maps_404_to_missing_release() {
        let message = format_github_api_error(404, r#"{"message":"Not Found"}"#);
        assert!(message.contains("找不到最新 Release"));
    }
}

pub fn emit_progress(app: &AppHandle, phase: &str, percent: Option<f64>, message: &str) {
    let _ = app.emit(
        HOST_PROGRESS_EVENT,
        HostUpdateProgress {
            phase: phase.to_string(),
            percent,
            message: message.to_string(),
        },
    );
}

pub fn download_with_progress<F>(
    url: &str,
    path: &Path,
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(u64, Option<u64>),
{
    let mut response = reqwest::blocking::Client::new()
        .get(url)
        .header("User-Agent", "BackgroundStudioHost/0.1")
        .send()
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;
    let total = response.content_length();
    let mut file = File::create(path).map_err(|error| error.to_string())?;
    let mut buffer = [0u8; 64 * 1024];
    let mut downloaded = 0u64;
    loop {
        let n = response.read(&mut buffer).map_err(|error| error.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])
            .map_err(|error| error.to_string())?;
        downloaded += n as u64;
        on_progress(downloaded, total);
    }
    Ok(())
}

pub fn installer_temp_path(asset_name: &str) -> PathBuf {
    std::env::temp_dir().join(asset_name)
}

pub fn launch_installer(path: &Path) -> Result<(), String> {
    Command::new(path)
        .spawn()
        .map_err(|error| format!("启动安装程序失败：{error}"))?;
    Ok(())
}
