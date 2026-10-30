//! 官方静态清单客户端。
//!
//! 一次 `GET registry/v1.json` 覆盖官方目录、插件版本与壳版本；不依赖 `gh` / 登录，
//! 也不访问 GitHub REST API。TTL 内直接用缓存，过期后带 `If-None-Match` 重新验证；
//! 失败时指数退避并回用上次成功缓存（明确给出警告），绝不回退 REST。

use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    catalog::{self, PluginDef, OFFICIAL_OWNER},
    proxy::{self, ProxySettings},
};

pub const OFFICIAL_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/background-studio/.github/main/registry/v1.json";
pub const OFFICIAL_ICON_BASE_URL: &str =
    "https://raw.githubusercontent.com/background-studio/.github/main/registry/icons/";
pub const HOST_REPO: &str = "background-studio";
pub const SCHEMA_VERSION: u64 = 1;
pub const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;
pub const MAX_ICON_BYTES: u64 = 2 * 1024 * 1024;
/// 官方清单缓存新鲜期。
pub const REGISTRY_TTL_SECS: i64 = 5 * 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PLUGINS: usize = 256;
const CACHE_FILE: &str = "official-registry-v1.json";
pub const USER_AGENT: &str = concat!("BackgroundStudioHost/", env!("CARGO_PKG_VERSION"));

/// 更新渠道。沿用现有语义：壳版本号带预发布段（如 0.3.7-beta.1）时插件走 prerelease，
/// 否则走 stable；壳自身始终只看 stable。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Prerelease,
}

impl Channel {
    pub fn for_host_version(version: &str) -> Self {
        if version.contains('-') {
            Self::Prerelease
        } else {
            Self::Stable
        }
    }

    pub fn current() -> Self {
        Self::for_host_version(env!("CARGO_PKG_VERSION"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub version: String,
    pub tag: String,
    pub asset_name: String,
    pub download_url: String,
    pub release_url: String,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseChannels {
    pub stable: Option<Release>,
    pub prerelease: Option<Release>,
}

impl ReleaseChannels {
    pub fn for_channel(&self, channel: Channel) -> Option<&Release> {
        match channel {
            Channel::Stable => self.stable.as_ref(),
            Channel::Prerelease => self.prerelease.as_ref(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryPlugin {
    #[serde(flatten)]
    pub def: PluginDef,
    pub releases: ReleaseChannels,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub schema_version: u64,
    pub generated_at: String,
    pub plugins: Vec<RegistryPlugin>,
    pub host: ReleaseChannels,
}

impl Registry {
    pub fn plugin(&self, id: &str) -> Option<&RegistryPlugin> {
        self.plugins.iter().find(|plugin| plugin.def.id == id)
    }

    pub fn plugin_defs(&self) -> Vec<PluginDef> {
        self.plugins
            .iter()
            .map(|plugin| {
                let mut def = plugin.def.clone();
                def.source = catalog::CatalogSource::Official;
                def
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 解析与校验
// ---------------------------------------------------------------------------

/// 解析并严格校验官方清单；任一项不合规则整份拒绝。未知字段忽略以保持前向兼容。
pub fn parse_registry(bytes: &[u8]) -> Result<Registry, String> {
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(format!("清单超过 {MAX_REGISTRY_BYTES} 字节上限"));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("清单不是有效 JSON：{error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "清单顶层必须是对象".to_string())?;
    match object.get("schemaVersion").and_then(Value::as_u64) {
        Some(SCHEMA_VERSION) => {}
        Some(other) => return Err(format!("不支持的清单 schemaVersion {other}")),
        None => return Err("清单缺少 schemaVersion".to_string()),
    }
    require_channel_keys(object.get("host"), "host")?;
    let plugins = object
        .get("plugins")
        .and_then(Value::as_array)
        .ok_or_else(|| "清单缺少 plugins 数组".to_string())?;
    for (index, plugin) in plugins.iter().enumerate() {
        let label = plugin
            .get("id")
            .and_then(Value::as_str)
            .map(|id| format!("插件 {id}"))
            .unwrap_or_else(|| format!("plugins[{index}]"));
        require_channel_keys(plugin.get("releases"), &format!("{label}.releases"))?;
    }
    let registry: Registry =
        serde_json::from_value(value).map_err(|error| format!("清单结构无效：{error}"))?;
    validate_registry(&registry)?;
    Ok(registry)
}

/// stable / prerelease 两个键必须都出现（值可为 null），防止字段拼写错误被当成“无版本”。
fn require_channel_keys(value: Option<&Value>, label: &str) -> Result<(), String> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{label} 必须是对象"))?;
    for key in ["stable", "prerelease"] {
        if !object.contains_key(key) {
            return Err(format!("{label} 缺少 {key}（无版本时请写 null）"));
        }
    }
    Ok(())
}

fn validate_registry(registry: &Registry) -> Result<(), String> {
    chrono::DateTime::parse_from_rfc3339(&registry.generated_at)
        .map_err(|_| format!("generatedAt {:?} 不是 ISO 8601 时间", registry.generated_at))?;
    if registry.plugins.len() > MAX_PLUGINS {
        return Err(format!("插件数量超过 {MAX_PLUGINS} 上限"));
    }
    let mut seen = HashSet::new();
    for plugin in &registry.plugins {
        let def = &plugin.def;
        catalog::validate_plugin_id(&def.id)?;
        if !seen.insert(def.id.clone()) {
            return Err(format!("插件 id {} 重复", def.id));
        }
        validate_plugin_def(def).map_err(|error| format!("插件 {}：{error}", def.id))?;
        for (channel, release) in [
            (Channel::Stable, &plugin.releases.stable),
            (Channel::Prerelease, &plugin.releases.prerelease),
        ] {
            if let Some(release) = release {
                let expected_asset = format!("{}{}-plugin.zip", def.asset_prefix, release.version);
                validate_release(release, channel, &def.repo, &expected_asset)
                    .map_err(|error| format!("插件 {} 的 {channel:?} 版本：{error}", def.id))?;
            }
        }
    }
    for (channel, release) in [
        (Channel::Stable, &registry.host.stable),
        (Channel::Prerelease, &registry.host.prerelease),
    ] {
        if let Some(release) = release {
            let expected_asset = format!("Background.Studio_{}_x64-setup.exe", release.version);
            validate_release(release, channel, HOST_REPO, &expected_asset)
                .map_err(|error| format!("壳的 {channel:?} 版本：{error}"))?;
        }
    }
    Ok(())
}

fn validate_plugin_def(def: &PluginDef) -> Result<(), String> {
    if def.owner != OFFICIAL_OWNER {
        return Err(format!(
            "owner 必须是 {OFFICIAL_OWNER}，实际为 {:?}",
            def.owner
        ));
    }
    validate_repo(&def.repo)?;
    if def.asset_prefix.is_empty()
        || def.asset_prefix.len() > 64
        || !def.asset_prefix.bytes().all(is_safe_name_byte)
    {
        return Err(format!("assetPrefix {:?} 无效", def.asset_prefix));
    }
    validate_exe_name(&def.exe_name)?;
    let pipe_suffix = def
        .pipe_name
        .strip_prefix(r"\\.\pipe\")
        .ok_or_else(|| format!("pipeName {:?} 必须以 \\\\.\\pipe\\ 开头", def.pipe_name))?;
    if pipe_suffix.is_empty()
        || pipe_suffix.len() > 128
        || !pipe_suffix.bytes().all(is_safe_name_byte)
    {
        return Err(format!("pipeName {:?} 无效", def.pipe_name));
    }
    validate_text(&def.display_name, "displayName", 128, false)?;
    validate_text(&def.target_hint, "targetHint", 256, true)?;
    if let Some(icon) = &def.icon {
        let valid = icon.len() <= 128
            && icon.ends_with(".png")
            && !icon.starts_with('.')
            && icon.bytes().all(is_safe_name_byte);
        if !valid {
            return Err(format!("icon {icon:?} 必须是 .png 纯文件名"));
        }
    }
    Ok(())
}

fn validate_repo(repo: &str) -> Result<(), String> {
    if repo.is_empty()
        || repo.len() > 100
        || repo == "."
        || repo == ".."
        || !repo.bytes().all(is_safe_name_byte)
    {
        return Err(format!("repo {repo:?} 无效"));
    }
    Ok(())
}

fn validate_exe_name(name: &str) -> Result<(), String> {
    let valid = name.len() <= 128
        && name.len() > ".exe".len()
        && name.to_ascii_lowercase().ends_with(".exe")
        && !name.starts_with('.')
        && !name.starts_with(' ')
        && !name.contains("..")
        && name.chars().all(|ch| {
            !ch.is_control() && !matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        });
    if !valid {
        return Err(format!("exeName {name:?} 必须是 .exe 纯文件名"));
    }
    Ok(())
}

fn validate_text(
    value: &str,
    field: &str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<(), String> {
    if (!allow_empty && value.trim().is_empty())
        || value.chars().count() > max_chars
        || value.chars().any(char::is_control)
    {
        return Err(format!("{field} 无效"));
    }
    Ok(())
}

fn is_safe_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

fn validate_release(
    release: &Release,
    channel: Channel,
    repo: &str,
    expected_asset: &str,
) -> Result<(), String> {
    let version = semver::Version::parse(&release.version)
        .map_err(|_| format!("version {:?} 不是 semver（不带 v）", release.version))?;
    match channel {
        Channel::Stable if !version.pre.is_empty() => {
            return Err(format!("stable 版本 {} 不应带预发布段", release.version));
        }
        Channel::Prerelease if version.pre.is_empty() => {
            return Err(format!(
                "prerelease 版本 {} 必须带预发布段",
                release.version
            ));
        }
        _ => {}
    }
    if release.tag != format!("v{}", release.version) {
        return Err(format!("tag {:?} 必须为 v{}", release.tag, release.version));
    }
    if release.asset_name != expected_asset {
        return Err(format!(
            "assetName {:?} 必须为 {expected_asset}",
            release.asset_name
        ));
    }
    let expected_download = github_download_url(OFFICIAL_OWNER, repo, &release.tag, expected_asset);
    if release.download_url != expected_download {
        return Err(format!(
            "downloadUrl 与仓库身份不符（期望 {expected_download}）"
        ));
    }
    let expected_release = format!(
        "https://github.com/{OFFICIAL_OWNER}/{repo}/releases/tag/{}",
        release.tag
    );
    if release.release_url != expected_release {
        return Err(format!(
            "releaseUrl 与仓库身份不符（期望 {expected_release}）"
        ));
    }
    if let Some(sha256) = &release.sha256 {
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("sha256 必须是 64 位十六进制或 null".to_string());
        }
    }
    Ok(())
}

/// GitHub Release 资产下载地址；官方与自定义插件统一按此绑定 owner/repo/tag/asset。
pub fn github_download_url(owner: &str, repo: &str, tag: &str, asset: &str) -> String {
    format!("https://github.com/{owner}/{repo}/releases/download/{tag}/{asset}")
}

// ---------------------------------------------------------------------------
// 传输层
// ---------------------------------------------------------------------------

pub enum HttpOutcome {
    Body { body: Vec<u8>, etag: Option<String> },
    NotModified { etag: Option<String> },
}

pub trait RegistryTransport {
    fn get(
        &self,
        url: &str,
        if_none_match: Option<&str>,
        max_bytes: u64,
    ) -> Result<HttpOutcome, String>;
}

/// 生产 HTTP 传输：沿用壳代理设置，带超时、禁重定向、响应体上限。
pub struct HttpTransport {
    client: Result<reqwest::blocking::Client, String>,
}

impl HttpTransport {
    pub fn new(settings: &ProxySettings) -> Self {
        let client = proxy::blocking_client_builder(settings).and_then(|builder| {
            builder
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| format!("创建 HTTP 客户端失败：{error}"))
        });
        Self { client }
    }
}

impl RegistryTransport for HttpTransport {
    fn get(
        &self,
        url: &str,
        if_none_match: Option<&str>,
        max_bytes: u64,
    ) -> Result<HttpOutcome, String> {
        let client = self.client.as_ref().map_err(Clone::clone)?;
        let mut request = client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json, image/png;q=0.9");
        if let Some(etag) = if_none_match {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request
            .send()
            .map_err(|error| format!("请求失败：{error}"))?;
        let status = response.status();
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .and_then(sanitize_etag);
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(HttpOutcome::NotModified { etag });
        }
        if status != reqwest::StatusCode::OK {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes)
        {
            return Err(format!("响应体超过 {max_bytes} 字节上限"));
        }
        let mut body = Vec::new();
        response
            .take(max_bytes + 1)
            .read_to_end(&mut body)
            .map_err(|error| format!("读取响应失败：{error}"))?;
        if body.len() as u64 > max_bytes {
            return Err(format!("响应体超过 {max_bytes} 字节上限"));
        }
        Ok(HttpOutcome::Body { body, etag })
    }
}

fn sanitize_etag(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)))
    .then(|| value.to_string())
}

// ---------------------------------------------------------------------------
// 缓存与刷新
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RefreshMode {
    /// 启动 / 安装前：TTL 内用缓存，失败退避期内不发请求。
    Auto,
    /// 用户点「检查更新」：跳过 TTL 与退避，仍带 ETag 条件请求。
    Force,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RegistryOrigin {
    /// 本次 200 下载并校验通过。
    Network,
    /// 本次 304，沿用缓存正文。
    NotModified,
    /// TTL 内，未发请求。
    FreshCache,
    /// 本次 200 但 generatedAt 比缓存旧（CDN 回滚 / 边缘节点陈旧）：保留较新的缓存。
    KeptNewerCache,
    /// 请求失败或退避中，回用上次成功缓存。
    StaleCache,
    /// 请求失败且没有可用缓存。
    Unavailable,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheFile {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    etag: Option<String>,
    /// 最近一次成功（200 或 304）的 Unix 秒。
    #[serde(default)]
    fetched_at: Option<i64>,
    #[serde(default)]
    last_attempt_at: Option<i64>,
    #[serde(default)]
    consecutive_failures: u32,
    #[serde(default)]
    next_retry_at: Option<i64>,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheInfo {
    pub path: String,
    pub present: bool,
    pub fetched_at: Option<String>,
    pub etag: Option<String>,
    pub generated_at: Option<String>,
    pub consecutive_failures: u32,
    pub next_retry_at: Option<String>,
    pub write_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RegistryOutcome {
    pub registry: Option<Registry>,
    pub origin: RegistryOrigin,
    pub http_requests: u32,
    /// 回用缓存 / 不可用时给用户看的警告。
    pub warning: Option<String>,
    pub error: Option<String>,
    pub cache: CacheInfo,
}

pub fn cache_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(CACHE_FILE)
}

fn read_cache(path: &Path) -> CacheFile {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// 只读：返回上次成功缓存的清单（不校验 TTL、不发请求），用于启动时离线合并目录。
pub fn load_cached_registry(cache_dir: &Path) -> Option<Registry> {
    read_cache(&cache_path(cache_dir))
        .body
        .and_then(|body| parse_registry(body.as_bytes()).ok())
}

fn generated_at_of(registry: &Registry) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(&registry.generated_at).ok()
}

/// `candidate` 的 generatedAt 是否严格早于 `current`（防 CDN 回滚覆盖较新缓存）。
fn is_older(candidate: &Registry, current: &Registry) -> bool {
    match (generated_at_of(candidate), generated_at_of(current)) {
        (Some(candidate), Some(current)) => candidate < current,
        _ => false,
    }
}

pub fn backoff_secs(failures: u32) -> i64 {
    let exponent = failures.saturating_sub(1).min(10);
    (30i64 << exponent).min(600)
}

pub fn format_timestamp(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| seconds.to_string())
}

fn format_rfc3339(seconds: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(seconds, 0).map(|time| time.to_rfc3339())
}

pub fn refresh_official(
    cache_dir: &Path,
    transport: &dyn RegistryTransport,
    mode: RefreshMode,
    now: i64,
) -> RegistryOutcome {
    let path = cache_path(cache_dir);
    let mut cache = read_cache(&path);
    let cached = cache
        .body
        .as_deref()
        .and_then(|body| parse_registry(body.as_bytes()).ok());
    if cached.is_none() {
        // 缓存缺失或已不合本版本校验：丢弃正文与 ETag，下次必须完整下载。
        cache.body = None;
        cache.etag = None;
        cache.fetched_at = None;
    }

    let fresh = cached.is_some()
        && cache
            .fetched_at
            .is_some_and(|fetched| now >= fetched && now - fetched < REGISTRY_TTL_SECS);
    // 上次请求失败的状态必须保留到下一次成功（200 / 304）为止：
    // 先看失败与退避，再看 TTL，避免手动失败后的自动刷新把警告隐去。
    let last_failed = cache.consecutive_failures > 0;
    if mode == RefreshMode::Auto && last_failed {
        if let Some(retry_at) = cache.next_retry_at.filter(|retry_at| now < *retry_at) {
            let error = cache
                .last_error
                .clone()
                .unwrap_or_else(|| "上次请求失败".to_string());
            let error = format!("{error}（退避中，{} 后重试）", format_timestamp(retry_at));
            return failure_outcome(&path, &cache, cached, 0, error, None);
        }
        // 退避已结束：即使仍在 TTL 内也重新请求，成功才清除失败状态。
    } else if mode == RefreshMode::Auto && fresh {
        return outcome(
            &path,
            &cache,
            cached,
            RegistryOrigin::FreshCache,
            0,
            None,
            None,
        );
    }

    let etag = cached.as_ref().and(cache.etag.clone());
    cache.last_attempt_at = Some(now);
    let result = transport
        .get(OFFICIAL_REGISTRY_URL, etag.as_deref(), MAX_REGISTRY_BYTES)
        .and_then(|response| match response {
            HttpOutcome::NotModified { etag } => match &cached {
                Some(registry) => Ok((
                    registry.clone(),
                    None,
                    etag,
                    RegistryOrigin::NotModified,
                    None,
                )),
                None => Err("服务器返回 304，但本地没有可用缓存".to_string()),
            },
            HttpOutcome::Body { body, etag } => {
                let registry =
                    parse_registry(&body).map_err(|error| format!("官方清单无效：{error}"))?;
                if let Some(current) = cached.as_ref().filter(|current| is_older(&registry, current)) {
                    // 合法但更旧：不覆盖、不替换 ETag，下次仍会重新验证。
                    let warning = format!(
                        "官方清单服务器返回了较旧的版本（generatedAt {}，缓存为 {}），已保留较新的缓存。",
                        registry.generated_at, current.generated_at
                    );
                    return Ok((
                        current.clone(),
                        None,
                        None,
                        RegistryOrigin::KeptNewerCache,
                        Some(warning),
                    ));
                }
                let text = String::from_utf8(body).map_err(|_| "官方清单不是 UTF-8".to_string())?;
                Ok((registry, Some(text), etag, RegistryOrigin::Network, None))
            }
        });
    match result {
        Ok((registry, body, etag, origin, warning)) => {
            if let Some(body) = body {
                cache.body = Some(body);
                cache.etag = etag;
            } else if etag.is_some() {
                cache.etag = etag;
            }
            cache.fetched_at = Some(now);
            cache.consecutive_failures = 0;
            cache.next_retry_at = None;
            cache.last_error = None;
            let write_error = write_cache(&path, &cache).err();
            let mut result = outcome(&path, &cache, Some(registry), origin, 1, warning, None);
            result.cache.write_error = write_error;
            result
        }
        Err(error) => {
            cache.consecutive_failures = cache.consecutive_failures.saturating_add(1);
            cache.next_retry_at = Some(now + backoff_secs(cache.consecutive_failures));
            cache.last_error = Some(error.clone());
            let write_error = write_cache(&path, &cache).err();
            failure_outcome(&path, &cache, cached, 1, error, write_error)
        }
    }
}

fn failure_outcome(
    path: &Path,
    cache: &CacheFile,
    cached: Option<Registry>,
    http_requests: u32,
    error: String,
    write_error: Option<String>,
) -> RegistryOutcome {
    let (origin, warning) = match (&cached, cache.fetched_at) {
        (Some(_), fetched_at) => (
            RegistryOrigin::StaleCache,
            format!(
                "官方清单获取失败（{error}），正在使用 {} 的缓存，版本信息可能不是最新。",
                fetched_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "未知时间".to_string())
            ),
        ),
        (None, _) => (
            RegistryOrigin::Unavailable,
            format!("官方清单获取失败且没有本地缓存（{error}），暂时无法检查官方插件与壳的更新。"),
        ),
    };
    let mut result = outcome(
        path,
        cache,
        cached,
        origin,
        http_requests,
        Some(warning),
        Some(error),
    );
    result.cache.write_error = write_error;
    result
}

fn outcome(
    path: &Path,
    cache: &CacheFile,
    registry: Option<Registry>,
    origin: RegistryOrigin,
    http_requests: u32,
    warning: Option<String>,
    error: Option<String>,
) -> RegistryOutcome {
    let cache_info = CacheInfo {
        path: path.to_string_lossy().into_owned(),
        present: cache.body.is_some(),
        fetched_at: cache.fetched_at.and_then(format_rfc3339),
        etag: cache.etag.clone(),
        generated_at: registry
            .as_ref()
            .map(|registry| registry.generated_at.clone()),
        consecutive_failures: cache.consecutive_failures,
        next_retry_at: cache.next_retry_at.and_then(format_rfc3339),
        write_error: None,
    };
    RegistryOutcome {
        registry,
        origin,
        http_requests,
        warning,
        error,
        cache: cache_info,
    }
}

fn write_cache(path: &Path, cache: &CacheFile) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(cache).map_err(|error| error.to_string())?;
    atomic_write(path, &raw).map_err(|error| {
        eprintln!("写入官方清单缓存失败：{error}");
        error
    })
}

/// 同目录临时文件 + flush + rename 覆盖，避免半写文件。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("路径无效：{}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let temporary = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        handle
            .write_all(bytes)
            .and_then(|_| handle.sync_all())
            .map_err(|error| error.to_string())?;
        drop(handle);
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

// ---------------------------------------------------------------------------
// 官方图标（可选缓存）
// ---------------------------------------------------------------------------

/// 已有图标：仅当清单 generatedAt 变化且距上次检查超过该间隔才条件重验（ETag）。
pub const ICON_RECHECK_SECS: i64 = 6 * 60 * 60;
/// 缺失图标（如 404）重试的最短间隔；清单变化时可提前到该值。
const ICON_MISSING_RETRY_SECS: i64 = 10 * 60;
/// 单次查更最多发起的图标请求数。
pub const MAX_ICON_FETCHES_PER_CHECK: usize = 8;
const ICON_INDEX_FILE: &str = "official-icons-v1.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IconEntry {
    #[serde(default)]
    etag: Option<String>,
    /// 上次检查时清单的 generatedAt。
    #[serde(default)]
    generated_at: Option<String>,
    #[serde(default)]
    checked_at: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IconSyncReport {
    pub requests: u32,
    pub updated: u32,
    pub not_modified: u32,
    pub errors: Vec<String>,
    /// 因预算 / 单次上限而推迟的图标 id。
    pub deferred: Vec<String>,
}

fn icon_needs_check(
    entry: Option<&IconEntry>,
    file_exists: bool,
    generated_at: &str,
    now: i64,
) -> bool {
    let Some(entry) = entry else {
        return true;
    };
    let Some(checked_at) = entry.checked_at else {
        return true;
    };
    let elapsed = now - checked_at;
    if elapsed < 0 {
        return true;
    }
    let changed = entry.generated_at.as_deref() != Some(generated_at);
    if file_exists {
        changed && elapsed >= ICON_RECHECK_SECS
    } else {
        elapsed >= ICON_RECHECK_SECS || (changed && elapsed >= ICON_MISSING_RETRY_SECS)
    }
}

/// 有界同步官方图标 `registry/icons/<id>.png` → `icons/official/<id>.png`。
/// `ids` 为没有本地 / 内置图标的官方插件；`can_request` 判断剩余时间预算。
pub fn sync_official_icons(
    transport: &dyn RegistryTransport,
    cache_dir: &Path,
    data_dir: &Path,
    generated_at: &str,
    ids: &[String],
    now: i64,
    can_request: &dyn Fn() -> bool,
) -> IconSyncReport {
    let index_path = cache_dir.join(ICON_INDEX_FILE);
    let mut index: std::collections::HashMap<String, IconEntry> = fs::read_to_string(&index_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let mut report = IconSyncReport::default();
    let mut dirty = false;
    for id in ids {
        if catalog::validate_plugin_id(id).is_err() {
            continue;
        }
        let path = catalog::official_icon_cache_path(data_dir, id);
        let exists = path.exists();
        if !icon_needs_check(index.get(id), exists, generated_at, now) {
            continue;
        }
        if report.requests as usize >= MAX_ICON_FETCHES_PER_CHECK || !can_request() {
            report.deferred.push(id.clone());
            continue;
        }
        report.requests += 1;
        let entry = index.entry(id.clone()).or_default();
        let etag = if exists { entry.etag.clone() } else { None };
        let url = format!("{OFFICIAL_ICON_BASE_URL}{id}.png");
        let result = transport
            .get(&url, etag.as_deref(), MAX_ICON_BYTES)
            .and_then(|response| match response {
                HttpOutcome::NotModified { etag } if exists => Ok((None, etag)),
                HttpOutcome::NotModified { .. } => Err("图标返回 304 但本地没有缓存".to_string()),
                HttpOutcome::Body { body, etag } => {
                    if !body.starts_with(b"\x89PNG\r\n\x1a\n")
                        || imagesize::blob_size(&body).is_err()
                    {
                        return Err("图标不是有效 PNG".to_string());
                    }
                    Ok((Some(body), etag))
                }
            })
            .and_then(|(body, etag)| {
                if let Some(body) = &body {
                    atomic_write(&path, body)?;
                }
                Ok((body.is_some(), etag))
            });
        // 无论成功失败都记录检查时间，保证有界重试。
        entry.checked_at = Some(now);
        entry.generated_at = Some(generated_at.to_string());
        dirty = true;
        match result {
            Ok((updated, new_etag)) => {
                if new_etag.is_some() {
                    entry.etag = new_etag;
                }
                if updated {
                    report.updated += 1;
                } else {
                    report.not_modified += 1;
                }
            }
            Err(error) => report.errors.push(format!("{id}：{error}")),
        }
    }
    if dirty {
        if let Ok(raw) = serde_json::to_vec_pretty(&index) {
            if let Err(error) = atomic_write(&index_path, &raw) {
                report.errors.push(format!("写入图标索引失败：{error}"));
            }
        }
    }
    report
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};

    pub struct FakeTransport {
        pub responses: RefCell<VecDeque<Result<HttpOutcome, String>>>,
        pub requests: RefCell<Vec<(String, Option<String>)>>,
    }

    impl FakeTransport {
        pub fn new(responses: Vec<Result<HttpOutcome, String>>) -> Self {
            Self {
                responses: RefCell::new(responses.into()),
                requests: RefCell::new(Vec::new()),
            }
        }

        pub fn count(&self) -> usize {
            self.requests.borrow().len()
        }
    }

    impl RegistryTransport for FakeTransport {
        fn get(
            &self,
            url: &str,
            if_none_match: Option<&str>,
            _max_bytes: u64,
        ) -> Result<HttpOutcome, String> {
            self.requests
                .borrow_mut()
                .push((url.to_string(), if_none_match.map(str::to_string)));
            self.responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("没有预设响应".to_string()))
        }
    }

    pub fn release(repo: &str, prefix: &str, version: &str) -> Value {
        let tag = format!("v{version}");
        let asset = format!("{prefix}{version}-plugin.zip");
        serde_json::json!({
            "version": version,
            "tag": tag,
            "assetName": asset,
            "downloadUrl": format!("https://github.com/background-studio/{repo}/releases/download/{tag}/{asset}"),
            "releaseUrl": format!("https://github.com/background-studio/{repo}/releases/tag/{tag}"),
            "sha256": "a".repeat(64),
        })
    }

    pub fn host_release(version: &str) -> Value {
        let tag = format!("v{version}");
        let asset = format!("Background.Studio_{version}_x64-setup.exe");
        serde_json::json!({
            "version": version,
            "tag": tag,
            "assetName": asset,
            "downloadUrl": format!("https://github.com/background-studio/background-studio/releases/download/{tag}/{asset}"),
            "releaseUrl": format!("https://github.com/background-studio/background-studio/releases/tag/{tag}"),
            "sha256": null,
        })
    }

    pub fn plugin_entry(
        id: &str,
        repo: &str,
        prefix: &str,
        stable: Option<&str>,
        pre: Option<&str>,
    ) -> Value {
        serde_json::json!({
            "id": id,
            "displayName": format!("{id} Background Studio"),
            "owner": "background-studio",
            "repo": repo,
            "assetPrefix": prefix,
            "exeName": format!("{id} Background Studio.exe"),
            "pipeName": format!("\\\\.\\pipe\\background-studio-{id}"),
            "targetHint": "目标",
            "icon": format!("{id}.png"),
            "releases": {
                "stable": stable.map(|version| release(repo, prefix, version)),
                "prerelease": pre.map(|version| release(repo, prefix, version)),
            }
        })
    }

    pub fn registry_json(plugins: Vec<Value>, host_stable: Option<&str>) -> Value {
        serde_json::json!({
            "schemaVersion": 1,
            "generatedAt": "2026-05-01T08:00:00Z",
            "plugins": plugins,
            "host": {
                "stable": host_stable.map(host_release),
                "prerelease": null,
            }
        })
    }

    pub fn sample_registry() -> Value {
        registry_json(
            vec![plugin_entry(
                "codex",
                "codex_desktop_background",
                "CodexBackgroundStudio-",
                Some("0.6.0"),
                Some("0.6.1-beta.1"),
            )],
            Some("0.3.7"),
        )
    }

    pub fn body(value: &Value) -> Result<HttpOutcome, String> {
        Ok(HttpOutcome::Body {
            body: serde_json::to_vec(value).unwrap(),
            etag: Some("\"etag-1\"".to_string()),
        })
    }

    pub fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bs-registry-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn assert_rejected(value: Value, needle: &str) {
        let error = parse_registry(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(error.contains(needle), "{error} 不含 {needle}");
    }

    #[test]
    fn parses_valid_registry_and_ignores_unknown_fields() {
        let mut value = sample_registry();
        value["futureField"] = serde_json::json!(true);
        value["plugins"][0]["description"] = serde_json::json!("未来字段");
        let registry = parse_registry(&serde_json::to_vec(&value).unwrap()).unwrap();
        let codex = registry.plugin("codex").unwrap();
        assert_eq!(
            codex.releases.for_channel(Channel::Stable).unwrap().version,
            "0.6.0"
        );
        assert_eq!(
            codex
                .releases
                .for_channel(Channel::Prerelease)
                .unwrap()
                .version,
            "0.6.1-beta.1"
        );
        assert_eq!(registry.host.stable.as_ref().unwrap().version, "0.3.7");
        assert_eq!(
            registry.plugin_defs()[0].source,
            catalog::CatalogSource::Official
        );
    }

    #[test]
    fn rejects_malformed_registries() {
        assert!(parse_registry(b"not json").is_err());
        assert!(parse_registry(b"[]").is_err());
        assert!(parse_registry(&vec![b' '; MAX_REGISTRY_BYTES as usize + 1]).is_err());

        let mut value = sample_registry();
        value["schemaVersion"] = serde_json::json!(2);
        assert_rejected(value, "schemaVersion");

        let mut value = sample_registry();
        value["generatedAt"] = serde_json::json!("yesterday");
        assert_rejected(value, "generatedAt");

        let mut value = sample_registry();
        value["plugins"][0]["releases"]
            .as_object_mut()
            .unwrap()
            .remove("prerelease");
        assert_rejected(value, "prerelease");

        let mut value = sample_registry();
        value["host"].as_object_mut().unwrap().remove("stable");
        assert_rejected(value, "host");

        let mut value = sample_registry();
        value["plugins"][0]["releases"]["stable"]["version"] = serde_json::json!(5);
        assert_rejected(value, "结构无效");
    }

    #[test]
    fn rejects_unsafe_ids_and_duplicates() {
        for id in [
            "../evil", "Codex", "a/b", "", "console", "host", "-x", "a b",
        ] {
            let mut value = sample_registry();
            value["plugins"][0]["id"] = serde_json::json!(id);
            assert!(
                parse_registry(&serde_json::to_vec(&value).unwrap()).is_err(),
                "{id}"
            );
        }
        let mut value = sample_registry();
        let first = value["plugins"][0].clone();
        value["plugins"].as_array_mut().unwrap().push(first);
        assert_rejected(value, "重复");
    }

    #[test]
    fn rejects_foreign_owner_and_unsafe_names() {
        let mut value = sample_registry();
        value["plugins"][0]["owner"] = serde_json::json!("attacker");
        assert_rejected(value, "owner");

        for (field, bad) in [
            ("repo", ".."),
            ("repo", "a/b"),
            ("assetPrefix", "../x-"),
            ("exeName", "..\\evil.exe"),
            ("exeName", "C:evil.exe"),
            ("exeName", "evil.bat"),
            ("pipeName", "\\\\.\\pipe\\..\\x"),
            ("pipeName", "pipe-x"),
            ("icon", "../x.png"),
            ("icon", "https://evil/x.png"),
            ("displayName", ""),
        ] {
            let mut value = sample_registry();
            value["plugins"][0][field] = serde_json::json!(bad);
            assert!(
                parse_registry(&serde_json::to_vec(&value).unwrap()).is_err(),
                "{field}={bad}"
            );
        }
    }

    #[test]
    fn binds_release_urls_to_repository_identity() {
        let tamper = |field: &str, bad: Value| {
            let mut value = sample_registry();
            value["plugins"][0]["releases"]["stable"][field] = bad;
            parse_registry(&serde_json::to_vec(&value).unwrap())
        };
        assert!(tamper(
            "downloadUrl",
            serde_json::json!("https://evil.example/x.zip")
        )
        .is_err());
        assert!(tamper(
            "downloadUrl",
            serde_json::json!("https://github.com/attacker/codex_desktop_background/releases/download/v0.6.0/CodexBackgroundStudio-0.6.0-plugin.zip")
        )
        .is_err());
        assert!(tamper("releaseUrl", serde_json::json!("https://evil.example/")).is_err());
        assert!(tamper("tag", serde_json::json!("0.6.0")).is_err());
        assert!(tamper("version", serde_json::json!("v0.6.0")).is_err());
        assert!(tamper(
            "assetName",
            serde_json::json!("CodexBackgroundStudio-0.5.0-plugin.zip")
        )
        .is_err());
        assert!(tamper("sha256", serde_json::json!("abc")).is_err());
        assert!(tamper("sha256", serde_json::json!("z".repeat(64))).is_err());
        assert!(tamper("sha256", Value::Null).is_ok());
        assert!(tamper("sha256", serde_json::json!("A".repeat(64))).is_ok());

        let mut value = sample_registry();
        value["host"]["stable"]["downloadUrl"] = serde_json::json!(
            "https://github.com/background-studio/codex_desktop_background/releases/download/v0.3.7/Background.Studio_0.3.7_x64-setup.exe"
        );
        assert_rejected(value, "壳");
    }

    #[test]
    fn enforces_channel_version_shape() {
        let mut value = sample_registry();
        value["plugins"][0]["releases"]["stable"] = release(
            "codex_desktop_background",
            "CodexBackgroundStudio-",
            "0.6.1-beta.1",
        );
        assert_rejected(value, "预发布段");
        let mut value = sample_registry();
        value["plugins"][0]["releases"]["prerelease"] = release(
            "codex_desktop_background",
            "CodexBackgroundStudio-",
            "0.6.1",
        );
        assert_rejected(value, "预发布段");
    }

    #[test]
    fn ttl_cache_skips_network_and_force_revalidates_with_etag() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![
            body(&sample_registry()),
            Ok(HttpOutcome::NotModified { etag: None }),
        ]);
        let first = refresh_official(&dir, &transport, RefreshMode::Auto, 1_000);
        assert_eq!(first.origin, RegistryOrigin::Network);
        assert_eq!(first.http_requests, 1);

        let cached = refresh_official(&dir, &transport, RefreshMode::Auto, 1_000 + 60);
        assert_eq!(cached.origin, RegistryOrigin::FreshCache);
        assert_eq!(cached.http_requests, 0);
        assert_eq!(transport.count(), 1);

        let forced = refresh_official(&dir, &transport, RefreshMode::Force, 1_000 + 61);
        assert_eq!(forced.origin, RegistryOrigin::NotModified);
        assert!(forced.registry.is_some());
        assert_eq!(
            transport.requests.borrow()[1].1.as_deref(),
            Some("\"etag-1\"")
        );
        // 304 刷新了成功时间：随后自动刷新仍在 TTL 内。
        let again = refresh_official(&dir, &transport, RefreshMode::Auto, 1_000 + 61 + 200);
        assert_eq!(again.origin, RegistryOrigin::FreshCache);
        assert!(load_cached_registry(&dir).is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn expired_ttl_sends_conditional_request() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![
            body(&sample_registry()),
            Ok(HttpOutcome::NotModified {
                etag: Some("\"etag-2\"".to_string()),
            }),
        ]);
        refresh_official(&dir, &transport, RefreshMode::Auto, 0);
        let result = refresh_official(&dir, &transport, RefreshMode::Auto, REGISTRY_TTL_SECS);
        assert_eq!(result.origin, RegistryOrigin::NotModified);
        assert_eq!(result.cache.etag.as_deref(), Some("\"etag-2\""));
        assert_eq!(
            transport.requests.borrow()[1].1.as_deref(),
            Some("\"etag-1\"")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn offline_uses_last_good_cache_with_warning_and_backs_off() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![
            body(&sample_registry()),
            Err("网络不可达".to_string()),
            Err("网络不可达".to_string()),
        ]);
        refresh_official(&dir, &transport, RefreshMode::Auto, 0);
        let offline = refresh_official(&dir, &transport, RefreshMode::Auto, 1_000);
        assert_eq!(offline.origin, RegistryOrigin::StaleCache);
        assert!(offline.registry.is_some());
        assert!(offline.warning.as_deref().unwrap().contains("缓存"));
        assert_eq!(offline.cache.consecutive_failures, 1);

        // 退避期内自动刷新不发请求，仍回用缓存。
        let backoff = refresh_official(&dir, &transport, RefreshMode::Auto, 1_010);
        assert_eq!(backoff.http_requests, 0);
        assert_eq!(backoff.origin, RegistryOrigin::StaleCache);
        assert!(backoff.warning.as_deref().unwrap().contains("退避"));
        assert_eq!(transport.count(), 2);

        // 退避结束后重试，失败次数累加、退避加倍。
        let retry = refresh_official(&dir, &transport, RefreshMode::Auto, 1_000 + backoff_secs(1));
        assert_eq!(retry.http_requests, 1);
        assert_eq!(retry.cache.consecutive_failures, 2);
        assert_eq!(backoff_secs(2), 60);
        assert_eq!(backoff_secs(99), 600);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_response_keeps_previous_cache() {
        let dir = temp_dir();
        let mut evil = sample_registry();
        evil["plugins"][0]["owner"] = serde_json::json!("attacker");
        let transport = FakeTransport::new(vec![
            body(&sample_registry()),
            body(&evil),
            Ok(HttpOutcome::Body {
                body: b"{".to_vec(),
                etag: None,
            }),
        ]);
        refresh_official(&dir, &transport, RefreshMode::Auto, 0);
        let bad = refresh_official(&dir, &transport, RefreshMode::Force, 10);
        assert_eq!(bad.origin, RegistryOrigin::StaleCache);
        assert!(bad.error.as_deref().unwrap().contains("官方清单无效"));
        assert_eq!(
            bad.registry.unwrap().plugins[0].def.owner,
            "background-studio"
        );
        let bad = refresh_official(&dir, &transport, RefreshMode::Force, 20);
        assert_eq!(bad.origin, RegistryOrigin::StaleCache);
        assert!(load_cached_registry(&dir).is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn no_cache_and_offline_is_unavailable() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![Err("超时".to_string())]);
        let result = refresh_official(&dir, &transport, RefreshMode::Auto, 0);
        assert_eq!(result.origin, RegistryOrigin::Unavailable);
        assert!(result.registry.is_none());
        assert!(result.warning.unwrap().contains("没有本地缓存"));
        // 无缓存时不发送 If-None-Match。
        assert_eq!(transport.requests.borrow()[0].1, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_cache_file_is_ignored() {
        let dir = temp_dir();
        fs::write(cache_path(&dir), b"{garbage").unwrap();
        assert!(load_cached_registry(&dir).is_none());
        let transport = FakeTransport::new(vec![body(&sample_registry())]);
        let result = refresh_official(&dir, &transport, RefreshMode::Auto, 0);
        assert_eq!(result.origin, RegistryOrigin::Network);
        assert_eq!(transport.requests.borrow()[0].1, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn official_icon_must_be_png() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![Ok(HttpOutcome::Body {
            body: b"<html>".to_vec(),
            etag: None,
        })]);
        let ids = vec!["codex".to_string(), "../x".to_string()];
        let report = sync_official_icons(&transport, &dir, &dir, "g1", &ids, 0, &|| true);
        assert_eq!(report.requests, 1);
        assert_eq!(report.errors.len(), 1);
        assert!(!catalog::official_icon_cache_path(&dir, "codex").exists());
        // 失败也记录检查时间：短时间内不再重试。
        let again = sync_official_icons(&transport, &dir, &dir, "g1", &ids, 60, &|| true);
        assert_eq!(again.requests, 0);
        let _ = fs::remove_dir_all(dir);
    }

    pub fn tiny_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn icons_refresh_only_when_registry_changes_and_bounded() {
        let dir = temp_dir();
        let ids = vec!["codex".to_string()];
        let transport = FakeTransport::new(vec![
            Ok(HttpOutcome::Body {
                body: tiny_png(),
                etag: Some("\"i1\"".to_string()),
            }),
            Ok(HttpOutcome::NotModified { etag: None }),
        ]);
        let first = sync_official_icons(&transport, &dir, &dir, "g1", &ids, 0, &|| true);
        assert_eq!((first.requests, first.updated), (1, 1));
        // 清单未变：无论多久都不重拉。
        let same = sync_official_icons(
            &transport,
            &dir,
            &dir,
            "g1",
            &ids,
            10 * ICON_RECHECK_SECS,
            &|| true,
        );
        assert_eq!(same.requests, 0);
        // 清单变了但未到重验间隔：不拉。
        let soon = sync_official_icons(&transport, &dir, &dir, "g2", &ids, 60, &|| true);
        assert_eq!(soon.requests, 0);
        // 清单变了且超过间隔：带 ETag 条件请求。
        let later = sync_official_icons(
            &transport,
            &dir,
            &dir,
            "g2",
            &ids,
            ICON_RECHECK_SECS,
            &|| true,
        );
        assert_eq!((later.requests, later.not_modified), (1, 1));
        assert_eq!(transport.requests.borrow()[1].1.as_deref(), Some("\"i1\""));
        // 预算不足时推迟。
        let ids_many: Vec<String> = (0..20).map(|index| format!("p{index}")).collect();
        let budget = sync_official_icons(&transport, &dir, &dir, "g3", &ids_many, 0, &|| false);
        assert_eq!(budget.requests, 0);
        assert_eq!(budget.deferred.len(), 20);
        let capped = sync_official_icons(
            &FakeTransport::new(vec![]),
            &dir,
            &dir,
            "g3",
            &ids_many,
            0,
            &|| true,
        );
        assert_eq!(capped.requests as usize, MAX_ICON_FETCHES_PER_CHECK);
        assert_eq!(capped.deferred.len(), 20 - MAX_ICON_FETCHES_PER_CHECK);
        let _ = fs::remove_dir_all(dir);
    }

    /// 回归：TTL 内手动刷新失败后，紧接着的自动刷新不得变成干净的 FreshCache；
    /// 退避内 0 GET 且保留警告，直到下一次 200 / 304 成功才清除。
    #[test]
    fn force_failure_within_ttl_is_not_hidden_by_auto_fresh_cache() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![
            body(&sample_registry()),
            Err("代理连接失败".to_string()),
            Err("代理连接失败".to_string()),
            Ok(HttpOutcome::NotModified { etag: None }),
        ]);
        let ok = refresh_official(&dir, &transport, RefreshMode::Auto, 0);
        assert_eq!(ok.origin, RegistryOrigin::Network);

        let forced = refresh_official(&dir, &transport, RefreshMode::Force, 10);
        assert_eq!(forced.origin, RegistryOrigin::StaleCache);
        assert_eq!(forced.cache.consecutive_failures, 1);

        // 仍在 TTL 内、退避内：0 GET，保留失败状态与警告。
        let auto = refresh_official(&dir, &transport, RefreshMode::Auto, 11);
        assert_eq!(auto.origin, RegistryOrigin::StaleCache);
        assert_eq!(auto.http_requests, 0);
        assert_eq!(transport.count(), 2);
        assert!(auto.warning.as_deref().unwrap().contains("代理连接失败"));
        assert!(auto.error.as_deref().unwrap().contains("退避"));
        assert_eq!(auto.cache.consecutive_failures, 1);

        // 退避结束但仍在 TTL 内：重新请求；再失败继续保留。
        let retry = refresh_official(&dir, &transport, RefreshMode::Auto, 10 + backoff_secs(1));
        assert_eq!(retry.http_requests, 1);
        assert_eq!(retry.origin, RegistryOrigin::StaleCache);
        assert_eq!(retry.cache.consecutive_failures, 2);

        // 304 成功后才清除失败状态，随后自动刷新回到干净的 FreshCache。
        let recovered = refresh_official(&dir, &transport, RefreshMode::Force, 200);
        assert_eq!(recovered.origin, RegistryOrigin::NotModified);
        assert!(recovered.warning.is_none());
        assert_eq!(recovered.cache.consecutive_failures, 0);
        let clean = refresh_official(&dir, &transport, RefreshMode::Auto, 201);
        assert_eq!(clean.origin, RegistryOrigin::FreshCache);
        assert!(clean.warning.is_none());
        assert_eq!(clean.http_requests, 0);
        let _ = fs::remove_dir_all(dir);
    }
    /// 真实生成器输出的子集（codex + factory + host），保证与线上 schema 兼容。
    #[test]
    fn real_generator_fixture_is_accepted() {
        let raw = include_str!("testdata/registry-v1-sample.json");
        let registry = parse_registry(raw.as_bytes()).unwrap();
        let factory = registry.plugin("factory").unwrap();
        let stable = factory.releases.for_channel(Channel::Stable).unwrap();
        assert_eq!(stable.version, "0.1.1");
        assert_eq!(
            stable.sha256.as_deref(),
            Some("43d24b29ff8d077e783c1e219ddc9f9d5382ab0a421ed4cdbe2bb3b9f729a3b4")
        );
        assert!(factory.releases.prerelease.is_none());
        assert_eq!(factory.def.icon.as_deref(), Some("factory.png"));
        assert_eq!(registry.host.stable.as_ref().unwrap().version, "0.3.6");
        assert!(registry
            .plugin("codex")
            .unwrap()
            .releases
            .prerelease
            .is_some());
    }

    #[test]
    fn older_generated_at_keeps_newer_cache() {
        let dir = temp_dir();
        let mut newer = sample_registry();
        newer["generatedAt"] = serde_json::json!("2026-05-02T00:00:00Z");
        let mut older = sample_registry();
        older["generatedAt"] = serde_json::json!("2026-05-01T00:00:00Z");
        older["plugins"][0]["displayName"] = serde_json::json!("旧名字");
        let transport = FakeTransport::new(vec![body(&newer), body(&older)]);
        refresh_official(&dir, &transport, RefreshMode::Auto, 0);
        let result = refresh_official(&dir, &transport, RefreshMode::Force, 10);
        assert_eq!(result.origin, RegistryOrigin::KeptNewerCache);
        assert!(result.warning.as_deref().unwrap().contains("较旧"));
        let kept = result.registry.unwrap();
        assert_eq!(kept.generated_at, "2026-05-02T00:00:00Z");
        assert_ne!(kept.plugins[0].def.display_name, "旧名字");
        assert_eq!(
            load_cached_registry(&dir).unwrap().generated_at,
            "2026-05-02T00:00:00Z"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
