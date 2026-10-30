use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU32, Ordering},
        OnceLock,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::proxy::{self, ProxySettings};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 进程内缓存：本机 `gh` 是否已登录。未装 / 未登录时静默走匿名 HTTP。
static GH_AUTHENTICATED: OnceLock<bool> = OnceLock::new();

/// 进程内 GitHub REST 调用总数（gh api 与匿名 HTTPS 都计入），供诊断核对。
static REST_CALLS: AtomicU32 = AtomicU32::new(0);

/// `gh api` 子进程与匿名 REST 请求的超时，保证持锁查更有上界。
const REST_TIMEOUT: Duration = Duration::from_secs(15);
const GH_TIMEOUT: Duration = Duration::from_secs(20);
const GH_AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// 下载：连接超时 + 整体上限（大包慢网也必须结束）。
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostReleaseInfo {
    pub latest_version: Option<String>,
    pub asset_name: Option<String>,
    pub release_url: Option<String>,
    pub download_url: Option<String>,
    /// 官方清单给出的安装包 SHA-256（小写 hex）；null 表示清单未提供。
    #[serde(default)]
    pub sha256: Option<String>,
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
        match command.spawn() {
            Ok(child) => wait_with_timeout(child, GH_AUTH_TIMEOUT)
                .map(|output| output.status.success())
                .unwrap_or(false),
            Err(_) => false,
        }
    })
}

/// 等待子进程结束，超时即 kill，避免持锁无限阻塞。
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(stream) = stdout.as_mut() {
            let _ = stream.take(8 * 1024 * 1024).read_to_end(&mut buffer);
        }
        buffer
    });
    let err_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(stream) = stderr.as_mut() {
            let _ = stream.take(1024 * 1024).read_to_end(&mut buffer);
        }
        buffer
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("子进程超过 {} 秒未结束，已终止", timeout.as_secs()));
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    Ok(std::process::Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
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
    command
        .args(["api", api_path])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_no_window(&mut command);
    proxy::apply_to_command(&mut command, settings);
    let child = command
        .spawn()
        .map_err(|error| format!("调用 gh api 失败：{error}"))?;
    let output =
        wait_with_timeout(child, GH_TIMEOUT).map_err(|error| format!("gh api 超时：{error}"))?;
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
    let client = proxy::blocking_client_builder(settings)?
        .timeout(REST_TIMEOUT)
        .connect_timeout(REST_TIMEOUT)
        .build()
        .map_err(|error| format!("创建 HTTP 客户端失败：{error}"))?;
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
/// 仅用于本地自定义插件；官方插件与壳版本只来自官方静态清单。
pub fn github_api_get(api_path: &str, settings: &ProxySettings) -> Result<Value, String> {
    REST_CALLS.fetch_add(1, Ordering::SeqCst);
    if gh_authenticated() {
        github_api_get_via_gh(api_path, settings)
    } else {
        github_api_get_via_http(api_path, settings)
    }
}

pub fn rest_calls_in_process() -> u32 {
    REST_CALLS.load(Ordering::SeqCst)
}

/// 计算文件 SHA-256（小写 hex）。
pub fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 校验下载文件与官方清单 SHA-256 一致；不一致时删除文件并报错。
pub fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let actual = match file_sha256(path) {
        Ok(actual) => actual,
        Err(error) => {
            let _ = std::fs::remove_file(path);
            return Err(format!(
                "读取下载文件计算 SHA-256 失败（{error}），已删除，未执行安装。"
            ));
        }
    };
    if actual.eq_ignore_ascii_case(expected.trim()) {
        return Ok(());
    }
    let _ = std::fs::remove_file(path);
    Err(format!(
        "下载文件 SHA-256 校验失败（期望 {expected}，实际 {actual}），已删除，未执行安装。"
    ))
}

#[cfg(test)]
mod tests {
    use super::{format_github_api_error, verify_sha256, version_newer};

    #[test]
    fn sha256_mismatch_deletes_download() {
        let path = std::env::temp_dir().join(format!("bs-sha-{}.bin", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"hello").unwrap();
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(&path, &good.to_ascii_uppercase()).is_ok());
        assert!(path.exists());
        assert!(verify_sha256(&path, &"0".repeat(64)).is_err());
        assert!(!path.exists());
    }

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

pub fn download_with_progress<F>(
    url: &str,
    path: &Path,
    settings: &ProxySettings,
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(u64, Option<u64>),
{
    let client = proxy::blocking_client_builder(settings)?
        .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TOTAL_TIMEOUT)
        .build()
        .map_err(|error| format!("创建 HTTP 客户端失败：{error}"))?;
    let mut response = client
        .get(url)
        .header("User-Agent", "BackgroundStudioHost/0.1")
        .send()
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;
    let total = response.content_length();
    if total.is_some_and(|total| total > MAX_DOWNLOAD_BYTES) {
        return Err(format!(
            "下载文件超过 {} MiB 上限。",
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        ));
    }
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
        if downloaded > MAX_DOWNLOAD_BYTES {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(format!(
                "下载文件超过 {} MiB 上限。",
                MAX_DOWNLOAD_BYTES / 1024 / 1024
            ));
        }
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
