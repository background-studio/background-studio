use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::proxy::{self, ProxySettings};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 进程内缓存：本机 `gh` 是否已登录。未装 / 未登录时静默走匿名 HTTP。
static GH_AUTHENTICATED: OnceLock<bool> = OnceLock::new();

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
    match (
        semver::Version::parse(latest),
        semver::Version::parse(current),
    ) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => false,
    }
}

const RATE_LIMIT_HINT: &str = "GitHub API 限流（未登录约 60 次/小时/IP）。若本机已安装 GitHub CLI，可先执行 gh auth login 后再点「检查更新」";

pub fn format_github_api_error(status: u16, body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    if status == 403 && lower.contains("rate limit") {
        return RATE_LIMIT_HINT.to_string();
    }
    if status == 404 {
        return "GitHub 上找不到最新 Release（仓库可能尚未发布）。".to_string();
    }
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(message) = value.get("message").and_then(|m| m.as_str()) {
            if message.to_ascii_lowercase().contains("rate limit") {
                return RATE_LIMIT_HINT.to_string();
            }
            return format!("GitHub API {status}：{message}");
        }
    }
    format!("GitHub API 请求失败（HTTP {status}）")
}

fn configure_no_window(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

fn gh_authenticated() -> bool {
    *GH_AUTHENTICATED.get_or_init(|| {
        let mut command = Command::new("gh");
        command
            .args(["auth", "status"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_no_window(&mut command);
        match command.status() {
            Ok(status) => status.success(),
            Err(_) => false,
        }
    })
}

fn reject_rate_limit_payload(value: &Value, body: &str) -> Result<(), String> {
    if value
        .get("message")
        .and_then(|message| message.as_str())
        .is_some_and(|message| message.to_ascii_lowercase().contains("rate limit"))
    {
        return Err(format_github_api_error(403, body));
    }
    Ok(())
}

fn github_api_get_via_gh(api_path: &str, settings: &ProxySettings) -> Result<Value, String> {
    let mut command = Command::new("gh");
    command.args(["api", api_path]);
    configure_no_window(&mut command);
    proxy::apply_to_command(&mut command, settings);
    let output = command
        .output()
        .map_err(|error| format!("调用 gh api 失败：{error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let body = if !stdout.is_empty() { &stdout } else { &stderr };
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            reject_rate_limit_payload(&value, body)?;
            if let Some(message) = value.get("message").and_then(|message| message.as_str()) {
                return Err(format!("gh api 失败：{message}"));
            }
        }
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "未知错误".to_string()
        };
        return Err(format!("gh api 失败：{detail}"));
    }
    let value: Value =
        serde_json::from_str(&stdout).map_err(|error| format!("解析 gh api 响应失败：{error}"))?;
    reject_rate_limit_payload(&value, &stdout)?;
    Ok(value)
}

fn github_api_get_via_http(api_path: &str, settings: &ProxySettings) -> Result<Value, String> {
    let url = format!("https://api.github.com/{api_path}");
    let client = proxy::build_blocking_client(settings)?;
    let response = client
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
    reject_rate_limit_payload(&value, &body)?;
    Ok(value)
}

/// 查 GitHub API JSON。本机已登录 `gh` 时优先走 `gh api`（已登录额度）；
/// 否则匿名 HTTPS。已登录但 `gh api` 失败时不回退匿名。
pub fn github_api_get(api_path: &str, settings: &ProxySettings) -> Result<Value, String> {
    if gh_authenticated() {
        github_api_get_via_gh(api_path, settings)
    } else {
        github_api_get_via_http(api_path, settings)
    }
}

pub fn fetch_latest_host_release(settings: &ProxySettings) -> Result<HostReleaseInfo, String> {
    let value = github_api_get(
        &format!("repos/{HOST_OWNER}/{HOST_REPO}/releases/latest"),
        settings,
    )?;
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
    use super::{format_github_api_error, version_newer};

    #[test]
    fn compares_semver_prereleases_correctly() {
        assert!(version_newer("0.5.4-beta.1", "0.5.3"));
        assert!(version_newer("0.5.4", "0.5.4-beta.1"));
        assert!(version_newer("0.5.4-beta.2", "0.5.4-beta.1"));
        assert!(!version_newer("0.5.4-beta.1", "0.5.4"));
        assert!(!version_newer("not-a-version", "0.5.4"));
    }

    #[test]
    fn maps_rate_limit_to_chinese_hint() {
        let message =
            format_github_api_error(403, r#"{"message":"API rate limit exceeded for 1.2.3.4."}"#);
        assert!(message.contains("限流"));
        assert!(message.contains("检查更新"));
        assert!(message.contains("gh auth login"));
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
    settings: &ProxySettings,
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(u64, Option<u64>),
{
    let client = proxy::build_blocking_client(settings)?;
    let mut response = client
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
        let n = response
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
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
