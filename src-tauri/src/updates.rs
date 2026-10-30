//! 查更编排：一次官方清单请求覆盖官方目录 / 插件版本 / 壳版本；
//! 仅本地自定义（非官方镜像）插件才走 GitHub REST（gh 或匿名），并带持久缓存与退避。
//! 生产「检查更新」与 `--check-updates-json` 诊断模式共用 [`check_updates`]。

use std::{
    cell::Cell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    catalog::{self, CatalogSource, PluginDef},
    config, host_update,
    plugins::PluginsState,
    proxy::ProxySettings,
    registry::{
        self, Channel, HttpTransport, RefreshMode, Registry, RegistryOrigin, RegistryOutcome,
        RegistryTransport,
    },
};

/// 自定义插件 REST 结果缓存新鲜期（匿名 API 约 60 次/小时/IP）。
pub const REST_TTL_SECS: i64 = 30 * 60;
const REST_CACHE_FILE: &str = "github-api-cache-v1.json";
/// 一次查更（持有核心锁期间）所有网络请求的总预算。
/// 官方清单单次请求 ≤ 15s；之后每个图标 / REST 请求都只在剩余预算足够覆盖其自身超时时才发起，
/// 因而整体耗时有上界。
pub const CHECK_BUDGET: Duration = Duration::from_secs(60);
/// 单次 REST 调用上限（`gh api` 子进程超时 / 匿名 HTTPS 超时取较大者）。
pub const REST_CALL_MAX: Duration = Duration::from_secs(20);
/// 单次图标请求上限（与官方传输层超时一致）。
pub const ICON_CALL_MAX: Duration = Duration::from_secs(15);

pub fn cache_directory() -> PathBuf {
    config::bootstrap_directory().join("cache")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReleaseSource {
    /// 官方静态清单。
    Official,
    /// GitHub REST API（仅自定义插件）。
    GithubApi,
}

/// 某插件当前可安装的最新版本，连同其来源身份；安装前据此复核，源变化即失效。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginLatest {
    pub version: String,
    pub tag: String,
    pub asset_name: String,
    pub download_url: String,
    pub release_url: Option<String>,
    pub sha256: Option<String>,
    pub source: ReleaseSource,
    pub owner: String,
    pub repo: String,
}

impl PluginLatest {
    /// 该版本是否仍属于当前目录项（来源类别与 owner/repo/assetPrefix 一致）。
    pub fn matches(&self, def: &PluginDef) -> bool {
        let source_ok = match self.source {
            ReleaseSource::Official => def.source.is_official(),
            ReleaseSource::GithubApi => def.source == CatalogSource::Local,
        };
        source_ok
            && self.owner.eq_ignore_ascii_case(&def.owner)
            && self.repo.eq_ignore_ascii_case(&def.repo)
            && self.asset_name.starts_with(&def.asset_prefix)
            && self.asset_name.ends_with("-plugin.zip")
            && self.download_url
                == registry::github_download_url(&def.owner, &def.repo, &self.tag, &self.asset_name)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCheck {
    pub id: String,
    pub display_name: String,
    pub catalog_source: CatalogSource,
    pub owner: String,
    pub repo: String,
    pub release_source: ReleaseSource,
    pub latest: Option<PluginLatest>,
    /// 展示给用户的错误 / 陈旧原因。
    pub error: Option<String>,
    /// latest 来自失败后的回退缓存。
    pub stale: bool,
    /// 是否单独列入顶部「检查更新失败」汇总（官方清单整体不可用时由清单警告统一提示）。
    #[serde(skip)]
    pub aggregate_error: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    pub channel: Channel,
    pub mode: RefreshMode,
    #[serde(skip)]
    pub registry: RegistryOutcome,
    pub catalog_warnings: Vec<String>,
    #[serde(skip)]
    pub catalog: Vec<PluginDef>,
    pub plugins: Vec<PluginCheck>,
    #[serde(skip)]
    pub host: Result<host_update::HostReleaseInfo, String>,
    pub rest_calls: u32,
    pub rest_cache_hits: u32,
    /// 官方图标 GET 次数（与官方清单 GET 分开统计）。
    pub icon_requests: u32,
    pub icon_errors: Vec<String>,
    /// 因总预算耗尽而跳过的请求说明。
    pub budget_skipped: Vec<String>,
}

impl UpdateCheck {
    /// 本次检查是否完全成功：官方清单来自网络 / 304 / TTL 内缓存、壳与各插件均无错误。
    pub fn status(&self) -> &'static str {
        match self.registry.origin {
            RegistryOrigin::Unavailable => "failed",
            RegistryOrigin::StaleCache | RegistryOrigin::KeptNewerCache => "degraded",
            _ if self.host.is_err()
                || self.plugins.iter().any(|plugin| plugin.error.is_some())
                || !self.budget_skipped.is_empty()
                || self.registry.cache.write_error.is_some() =>
            {
                "degraded"
            }
            _ => "ok",
        }
    }
}

/// GitHub REST 访问抽象（生产为 `gh api` / 匿名 HTTPS），便于测试计数。
pub trait GithubApi {
    fn get(&self, api_path: &str) -> Result<Value, String>;
}

pub struct LiveGithubApi<'a> {
    pub proxy: &'a ProxySettings,
}

impl GithubApi for LiveGithubApi<'_> {
    fn get(&self, api_path: &str) -> Result<Value, String> {
        host_update::github_api_get(api_path, self.proxy)
    }
}

pub struct CheckInputs<'a> {
    pub cache_dir: &'a Path,
    pub builtin: &'a [PluginDef],
    pub local: &'a [PluginDef],
    pub catalog_warnings: Vec<String>,
    pub mode: RefreshMode,
    pub channel: Channel,
    pub now: i64,
    /// 需要补齐官方图标缓存的数据目录；None 表示不取图标。
    pub icon_data_dir: Option<&'a Path>,
    /// 网络请求总预算截止时间。
    pub deadline: Instant,
}

/// 生产路径：真实 HTTP 传输 + 真实 GitHub API。
pub fn check_updates_live(inputs: CheckInputs<'_>, proxy: &ProxySettings) -> UpdateCheck {
    let transport = HttpTransport::new(proxy);
    let api = LiveGithubApi { proxy };
    check_updates(inputs, &transport, &api)
}

pub fn check_updates(
    inputs: CheckInputs<'_>,
    transport: &dyn RegistryTransport,
    api: &dyn GithubApi,
) -> UpdateCheck {
    let outcome = registry::refresh_official(inputs.cache_dir, transport, inputs.mode, inputs.now);
    let official_defs = outcome.registry.as_ref().map(Registry::plugin_defs);
    let catalog = catalog::merge_catalog(inputs.builtin, official_defs.as_deref(), inputs.local);

    let counting = CountingApi {
        inner: api,
        calls: Cell::new(0),
    };
    let mut rest_cache = RestCache::load(inputs.cache_dir);
    let mut rest_cache_hits = 0;
    let mut budget_skipped = Vec::new();
    let mut plugins = Vec::new();
    for def in &catalog {
        let check = if def.source.is_official() {
            official_plugin_check(def, &outcome, inputs.channel)
        } else {
            let (check, hit) = custom_plugin_check(
                def,
                &mut rest_cache,
                &counting,
                inputs.mode,
                inputs.channel,
                inputs.now,
                remaining(inputs.deadline) >= REST_CALL_MAX,
            );
            if check.error.as_deref() == Some(BUDGET_EXHAUSTED) {
                budget_skipped.push(format!("REST {}/{}", def.owner, def.repo));
            }
            rest_cache_hits += u32::from(hit);
            check
        };
        plugins.push(check);
    }
    if rest_cache.dirty {
        if let Err(error) = rest_cache.save(inputs.cache_dir) {
            eprintln!("写入 GitHub API 缓存失败：{error}");
        }
    }

    let host = host_release_from(&outcome);

    // 官方图标：只在本次确实联系过官方时、为没有本地 / 内置图标的官方插件有界同步；
    // 已缓存图标仅在清单 generatedAt 变化且超过重验间隔时才带 ETag 重验。
    let mut icon_requests = 0;
    let mut icon_errors = Vec::new();
    if let (Some(data_dir), Some(registry)) = (inputs.icon_data_dir, outcome.registry.as_ref()) {
        if matches!(
            outcome.origin,
            RegistryOrigin::Network | RegistryOrigin::NotModified
        ) {
            let ids: Vec<String> = catalog
                .iter()
                .filter(|def| def.source == CatalogSource::Official)
                .filter(|def| catalog::resolve_bundled_icon_path(data_dir, def).is_none())
                .map(|def| def.id.clone())
                .collect();
            let deadline = inputs.deadline;
            let report = registry::sync_official_icons(
                transport,
                inputs.cache_dir,
                data_dir,
                &registry.generated_at,
                &ids,
                inputs.now,
                &|| remaining(deadline) >= ICON_CALL_MAX,
            );
            icon_requests = report.requests;
            icon_errors = report.errors;
            // 单次上限推迟的图标下次再补，不算失败；仅预算耗尽时记入 budget_skipped。
            if remaining(deadline) < ICON_CALL_MAX {
                budget_skipped.extend(report.deferred.iter().map(|id| format!("图标 {id}")));
            }
        }
    }
    UpdateCheck {
        channel: inputs.channel,
        mode: inputs.mode,
        registry: outcome,
        catalog_warnings: inputs.catalog_warnings,
        catalog,
        plugins,
        host,
        rest_calls: counting.calls.get(),
        rest_cache_hits,
        icon_requests,
        icon_errors,
        budget_skipped,
    }
}

const BUDGET_EXHAUSTED: &str = "本次检查的网络时间预算已用完，已跳过，请稍后再检查更新";

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn base_check(def: &PluginDef, release_source: ReleaseSource) -> PluginCheck {
    PluginCheck {
        id: def.id.clone(),
        display_name: def.display_name.clone(),
        catalog_source: def.source,
        owner: def.owner.clone(),
        repo: def.repo.clone(),
        release_source,
        latest: None,
        error: None,
        stale: false,
        aggregate_error: true,
    }
}

/// 官方插件只看官方清单（网络或缓存）；清单缺失 / 未收录时报错，绝不回退 REST。
fn official_plugin_check(
    def: &PluginDef,
    outcome: &RegistryOutcome,
    channel: Channel,
) -> PluginCheck {
    let mut check = base_check(def, ReleaseSource::Official);
    let Some(registry) = &outcome.registry else {
        check.error = Some("官方清单不可用，暂时无法检查更新".to_string());
        check.aggregate_error = false;
        return check;
    };
    check.stale = outcome.origin == RegistryOrigin::StaleCache;
    let Some(entry) = registry.plugin(&def.id) else {
        check.error = Some("官方清单未收录该插件，无法检查更新".to_string());
        return check;
    };
    match entry.releases.for_channel(channel) {
        Some(release) => {
            check.latest = Some(PluginLatest {
                version: release.version.clone(),
                tag: release.tag.clone(),
                asset_name: release.asset_name.clone(),
                download_url: release.download_url.clone(),
                release_url: Some(release.release_url.clone()),
                sha256: release.sha256.as_ref().map(|sha| sha.to_ascii_lowercase()),
                source: ReleaseSource::Official,
                owner: entry.def.owner.clone(),
                repo: entry.def.repo.clone(),
            });
        }
        None => {
            check.error = Some(match channel {
                Channel::Stable => "官方清单中暂无该插件的正式版本。".to_string(),
                Channel::Prerelease => "没有找到可用的插件预发布版本。".to_string(),
            });
        }
    }
    check
}

fn host_release_from(outcome: &RegistryOutcome) -> Result<host_update::HostReleaseInfo, String> {
    let registry = outcome
        .registry
        .as_ref()
        .ok_or_else(|| "官方清单不可用，暂时无法检查壳更新".to_string())?;
    // 壳沿用现有语义：只跟随 stable。
    let release = registry
        .host
        .stable
        .as_ref()
        .ok_or_else(|| "官方清单中暂无壳的正式版本".to_string())?;
    Ok(host_update::HostReleaseInfo {
        latest_version: Some(release.version.clone()),
        asset_name: Some(release.asset_name.clone()),
        release_url: Some(release.release_url.clone()),
        download_url: Some(release.download_url.clone()),
        sha256: release.sha256.as_ref().map(|sha| sha.to_ascii_lowercase()),
    })
}

// ---------------------------------------------------------------------------
// 自定义插件：GitHub REST + 持久缓存 / 退避
// ---------------------------------------------------------------------------

struct CountingApi<'a> {
    inner: &'a dyn GithubApi,
    calls: Cell<u32>,
}

impl GithubApi for CountingApi<'_> {
    fn get(&self, api_path: &str) -> Result<Value, String> {
        self.calls.set(self.calls.get() + 1);
        self.inner.get(api_path)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestEntry {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    asset_name: Option<String>,
    #[serde(default)]
    fetched_at: Option<i64>,
    #[serde(default)]
    consecutive_failures: u32,
    #[serde(default)]
    next_retry_at: Option<i64>,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Default)]
struct RestCache {
    entries: HashMap<String, RestEntry>,
    dirty: bool,
}

impl RestCache {
    fn load(cache_dir: &Path) -> Self {
        let entries = fs::read_to_string(cache_dir.join(REST_CACHE_FILE))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            entries,
            dirty: false,
        }
    }

    fn save(&self, cache_dir: &Path) -> Result<(), String> {
        let raw = serde_json::to_vec_pretty(&self.entries).map_err(|error| error.to_string())?;
        registry::atomic_write(&cache_dir.join(REST_CACHE_FILE), &raw)
    }
}

fn rest_key(def: &PluginDef, channel: Channel) -> String {
    format!(
        "{}/{}#{}#{:?}",
        def.owner.to_ascii_lowercase(),
        def.repo.to_ascii_lowercase(),
        def.asset_prefix,
        channel
    )
}

/// 返回 (检查结果, 是否命中缓存而未调用 REST)。
fn custom_plugin_check(
    def: &PluginDef,
    cache: &mut RestCache,
    api: &dyn GithubApi,
    mode: RefreshMode,
    channel: Channel,
    now: i64,
    budget_left: bool,
) -> (PluginCheck, bool) {
    let mut check = base_check(def, ReleaseSource::GithubApi);
    let key = rest_key(def, channel);
    let mut entry = cache.entries.get(&key).cloned().unwrap_or_default();
    let cached = match (&entry.version, &entry.asset_name) {
        (Some(version), Some(asset)) => Some((version.clone(), asset.clone())),
        _ => None,
    };
    let fresh = cached.is_some()
        && entry
            .fetched_at
            .is_some_and(|fetched| now >= fetched && now - fetched < REST_TTL_SECS);
    let backing_off = entry.next_retry_at.is_some_and(|retry_at| now < retry_at);

    let fill = |check: &mut PluginCheck, version: &str, asset: &str| {
        let tag = format!("v{version}");
        check.latest = Some(PluginLatest {
            version: version.to_string(),
            tag: tag.clone(),
            asset_name: asset.to_string(),
            download_url: registry::github_download_url(&def.owner, &def.repo, &tag, asset),
            release_url: Some(format!(
                "https://github.com/{}/{}/releases/tag/{tag}",
                def.owner, def.repo
            )),
            sha256: None,
            source: ReleaseSource::GithubApi,
            owner: def.owner.clone(),
            repo: def.repo.clone(),
        });
    };

    if (mode == RefreshMode::Auto && fresh) || backing_off {
        // REST 有限流：即使手动刷新也遵守退避。
        if let Some((version, asset)) = &cached {
            fill(&mut check, version, asset);
        }
        if backing_off {
            let error = entry
                .last_error
                .clone()
                .unwrap_or_else(|| "上次查询失败".to_string());
            check.error = Some(format!(
                "{error}（退避中，{} 后重试）",
                entry
                    .next_retry_at
                    .map(registry::format_timestamp)
                    .unwrap_or_default()
            ));
            check.stale = cached.is_some();
        }
        return (check, true);
    }
    if !budget_left {
        // 预算耗尽不算失败、不进入退避；保留上次结果。
        if let Some((version, asset)) = &cached {
            fill(&mut check, version, asset);
            check.stale = true;
        }
        check.error = Some(BUDGET_EXHAUSTED.to_string());
        return (check, cached.is_some());
    }

    match fetch_latest_plugin_asset(def, api, channel) {
        Ok((version, asset)) => {
            fill(&mut check, &version, &asset);
            entry = RestEntry {
                version: Some(version),
                asset_name: Some(asset),
                fetched_at: Some(now),
                ..RestEntry::default()
            };
        }
        Err(error) => {
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.next_retry_at = Some(now + registry::backoff_secs(entry.consecutive_failures));
            entry.last_error = Some(error.clone());
            if let Some((version, asset)) = &cached {
                fill(&mut check, version, asset);
                check.stale = true;
                check.error = Some(format!("{error}（正在使用上次查询结果）"));
            } else {
                check.error = Some(error);
            }
        }
    }
    cache.entries.insert(key, entry);
    cache.dirty = true;
    (check, false)
}

fn fetch_latest_plugin_asset(
    spec: &PluginDef,
    api: &dyn GithubApi,
    channel: Channel,
) -> Result<(String, String), String> {
    let prerelease_channel = channel == Channel::Prerelease;
    let endpoint = if prerelease_channel {
        format!("repos/{}/{}/releases?per_page=20", spec.owner, spec.repo)
    } else {
        format!("repos/{}/{}/releases/latest", spec.owner, spec.repo)
    };
    let value = api.get(&endpoint)?;
    select_plugin_asset(spec, &value, prerelease_channel)
}

pub fn select_plugin_asset(
    spec: &PluginDef,
    value: &Value,
    prerelease_channel: bool,
) -> Result<(String, String), String> {
    let release = if prerelease_channel {
        value
            .as_array()
            .and_then(|releases| {
                releases.iter().find(|release| {
                    release.get("draft").and_then(Value::as_bool) != Some(true)
                        && release.get("prerelease").and_then(Value::as_bool) == Some(true)
                })
            })
            .ok_or_else(|| "没有找到可用的插件预发布版本。".to_string())?
    } else {
        value
    };
    let tag = release
        .get("tag_name")
        .and_then(|tag| tag.as_str())
        .ok_or_else(|| "Release 缺少 tag_name（仓库可能还没有正式发布）。".to_string())?
        .to_string();
    let assets = release
        .get("assets")
        .and_then(|assets| assets.as_array())
        .ok_or_else(|| "Release 缺少 assets".to_string())?;
    let asset = assets
        .iter()
        .filter_map(|asset| asset.get("name").and_then(|name| name.as_str()))
        .find(|name| {
            name.starts_with(&spec.asset_prefix)
                && name.ends_with("-plugin.zip")
                && !name.contains(['/', '\\'])
                && !name.contains("..")
        })
        .ok_or_else(|| {
            format!(
                "最新 Release {} 中没有 {}*-plugin.zip",
                tag, spec.asset_prefix
            )
        })?
        .to_string();
    Ok((tag.trim_start_matches('v').to_string(), asset))
}

// ---------------------------------------------------------------------------
// `--check-updates-json <outputPath>` 诊断模式
// ---------------------------------------------------------------------------

/// 只读诊断：与生产「检查更新」走同一 [`check_updates_live`]，
/// 不获取单实例锁、不启动 worker、不写 plugins.json / profile / 自启动。
/// 唯一的写入是生产路径本身会更新的查更缓存、官方图标缓存，以及输出文件。
/// 返回 status（ok / degraded / failed）。
#[derive(Clone, Debug, Default)]
pub struct DiagnosticsOptions {
    /// 跳过 TTL / 官方退避（等同手动「检查更新」）。
    pub force: bool,
    /// 覆盖数据目录（只读其中 plugins.json 取代理与已装版本；图标缓存写到此处）。
    pub data_root: Option<PathBuf>,
    /// 覆盖查更缓存目录，便于隔离测试 TTL / 304 / 离线。
    pub cache_dir: Option<PathBuf>,
    /// 覆盖 bootstrap 目录（读取其中的本地 catalog.json）。
    pub bootstrap_dir: Option<PathBuf>,
}

impl DiagnosticsOptions {
    /// `--diagnostic-data-dir <dir>`：数据目录、bootstrap 与缓存全部落在该目录下，完全隔离真实配置。
    pub fn isolated(dir: PathBuf) -> Self {
        Self {
            force: false,
            data_root: Some(dir.clone()),
            cache_dir: Some(dir.join("cache")),
            bootstrap_dir: Some(dir),
        }
    }
}

pub fn run_check_updates_json_status(
    output: &Path,
    options: &DiagnosticsOptions,
) -> Result<String, String> {
    let report = build_diagnostics(options);
    let status = report
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("failed")
        .to_string();
    let raw = serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?;
    registry::atomic_write(output, &raw)?;
    Ok(status)
}

fn build_diagnostics(options: &DiagnosticsOptions) -> Value {
    match collect_diagnostics(options) {
        Ok(value) => value,
        Err(error) => serde_json::json!({
            "diagnosticsVersion": 1,
            "ok": false,
            "status": "failed",
            "fatalError": error,
            "hostVersion": host_update::current_version(),
        }),
    }
}

fn collect_diagnostics(options: &DiagnosticsOptions) -> Result<Value, String> {
    let data_dir = match &options.data_root {
        Some(root) => root.clone(),
        None => config::peek_data_directory()?,
    };
    let state: PluginsState = match fs::read_to_string(data_dir.join("plugins.json")) {
        Ok(raw) => {
            serde_json::from_str(&raw).map_err(|error| format!("plugins.json 无效：{error}"))?
        }
        Err(_) => PluginsState::default(),
    };
    let bootstrap = options
        .bootstrap_dir
        .clone()
        .unwrap_or_else(config::bootstrap_directory);
    let builtin = catalog::builtin_catalog()?;
    let (local, catalog_warnings) = catalog::load_local_overlay(&bootstrap)?;
    let cache_dir = options.cache_dir.clone().unwrap_or_else(cache_directory);
    let mode = if options.force {
        RefreshMode::Force
    } else {
        RefreshMode::Auto
    };
    let check = check_updates_live(
        CheckInputs {
            cache_dir: &cache_dir,
            builtin: &builtin,
            local: &local,
            catalog_warnings,
            mode,
            channel: Channel::current(),
            now: chrono::Utc::now().timestamp(),
            icon_data_dir: Some(&data_dir),
            deadline: Instant::now() + CHECK_BUDGET,
        },
        &state.proxy,
    );
    let mut report = diagnostics_json(&check, &state, &data_dir);
    report["bootstrapDirectory"] = Value::String(bootstrap.to_string_lossy().into_owned());
    report["cacheDirectory"] = Value::String(cache_dir.to_string_lossy().into_owned());
    Ok(report)
}

fn redact_proxy_url(raw: &str) -> String {
    match url::Url::parse(raw.trim()) {
        Ok(mut parsed) => {
            if !parsed.username().is_empty() || parsed.password().is_some() {
                let _ = parsed.set_username("***");
                let _ = parsed.set_password(None);
            }
            parsed.to_string()
        }
        Err(_) if raw.trim().is_empty() => String::new(),
        Err(_) => "<无法解析>".to_string(),
    }
}

fn diagnostics_json(check: &UpdateCheck, state: &PluginsState, data_dir: &Path) -> Value {
    let host_version = host_update::current_version();
    let plugins: Vec<Value> = check
        .plugins
        .iter()
        .map(|plugin| {
            let installed = state
                .plugins
                .iter()
                .find(|record| record.id == plugin.id)
                .and_then(|record| record.installed_version.clone());
            let update_available = match (&installed, &plugin.latest) {
                (Some(installed), Some(latest)) => {
                    host_update::version_newer(&latest.version, installed)
                }
                _ => false,
            };
            serde_json::json!({
                "id": plugin.id,
                "displayName": plugin.display_name,
                "catalogSource": plugin.catalog_source,
                "owner": plugin.owner,
                "repo": plugin.repo,
                "releaseSource": plugin.release_source,
                "installedVersion": installed,
                "latestVersion": plugin.latest.as_ref().map(|latest| &latest.version),
                "assetName": plugin.latest.as_ref().map(|latest| &latest.asset_name),
                "downloadUrl": plugin.latest.as_ref().map(|latest| &latest.download_url),
                "sha256": plugin.latest.as_ref().and_then(|latest| latest.sha256.clone()),
                "updateAvailable": update_available,
                "stale": plugin.stale,
                "error": plugin.error,
            })
        })
        .collect();
    let host = match &check.host {
        Ok(info) => serde_json::json!({
            "currentVersion": host_version,
            "latestVersion": info.latest_version,
            "updateAvailable": info
                .latest_version
                .as_deref()
                .is_some_and(|latest| host_update::version_newer(latest, &host_version)),
            "assetName": info.asset_name,
            "downloadUrl": info.download_url,
            "releaseUrl": info.release_url,
            "sha256": info.sha256,
            "error": Value::Null,
        }),
        Err(error) => serde_json::json!({
            "currentVersion": host_version,
            "latestVersion": Value::Null,
            "updateAvailable": false,
            "error": error,
        }),
    };
    let outcome = &check.registry;
    let status = check.status();
    serde_json::json!({
        "diagnosticsVersion": 1,
        "ok": status == "ok",
        "status": status,
        "generatedAt": chrono::Utc::now().to_rfc3339(),
        "hostVersion": host_version,
        "channel": check.channel,
        "mode": check.mode,
        "dataDirectory": data_dir.to_string_lossy(),
        "proxy": {
            "mode": state.proxy.mode,
            "url": redact_proxy_url(&state.proxy.url),
        },
        "official": {
            "url": registry::OFFICIAL_REGISTRY_URL,
            "origin": outcome.origin,
            "usedCacheFallback": matches!(outcome.origin, RegistryOrigin::StaleCache | RegistryOrigin::KeptNewerCache),
            "httpRequests": outcome.http_requests,
            "iconRequests": check.icon_requests,
            "iconErrors": check.icon_errors,
            "warning": outcome.warning,
            "error": outcome.error,
            "generatedAt": outcome.registry.as_ref().map(|registry| &registry.generated_at),
            "pluginCount": outcome.registry.as_ref().map(|registry| registry.plugins.len()),
            "cache": outcome.cache,
        },
        "githubApi": {
            "restCalls": check.rest_calls,
            // 进程级计数（github_api_get 入口原子计数），与上面的本次检查计数互相印证。
            "processRestCalls": host_update::rest_calls_in_process(),
            "cacheHits": check.rest_cache_hits,
        },
        "totals": {
            "officialRegistryGets": outcome.http_requests,
            "officialIconGets": check.icon_requests,
            "officialHttpRequests": outcome.http_requests + check.icon_requests,
            "restCalls": check.rest_calls,
        },
        "budgetSkipped": check.budget_skipped,
        "catalogWarnings": check.catalog_warnings,
        "plugins": plugins,
        "host": host,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::tests::{
        body, plugin_entry, registry_json, sample_registry, temp_dir, FakeTransport,
    };
    use serde_json::json;
    use std::cell::RefCell;

    struct FakeApi {
        calls: RefCell<Vec<String>>,
        response: Result<Value, String>,
    }

    impl FakeApi {
        fn ok(value: Value) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                response: Ok(value),
            }
        }
        fn err(message: &str) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                response: Err(message.to_string()),
            }
        }
    }

    impl GithubApi for FakeApi {
        fn get(&self, api_path: &str) -> Result<Value, String> {
            self.calls.borrow_mut().push(api_path.to_string());
            self.response.clone()
        }
    }

    fn def(id: &str, owner: &str, repo: &str, prefix: &str) -> PluginDef {
        PluginDef {
            id: id.to_string(),
            display_name: format!("{id} 显示名"),
            owner: owner.to_string(),
            repo: repo.to_string(),
            asset_prefix: prefix.to_string(),
            exe_name: format!("{id}.exe"),
            pipe_name: format!(r"\\.\pipe\{id}"),
            target_hint: String::new(),
            icon: None,
            source: CatalogSource::Builtin,
        }
    }

    fn codex_builtin() -> PluginDef {
        def(
            "codex",
            "background-studio",
            "codex_desktop_background",
            "CodexBackgroundStudio-",
        )
    }

    fn run(
        dir: &Path,
        builtin: &[PluginDef],
        local: &[PluginDef],
        transport: &FakeTransport,
        api: &FakeApi,
        mode: RefreshMode,
        channel: Channel,
        now: i64,
    ) -> UpdateCheck {
        let mut local_defs = local.to_vec();
        for plugin in &mut local_defs {
            plugin.source = CatalogSource::Local;
        }
        check_updates(
            CheckInputs {
                cache_dir: dir,
                builtin,
                local: &local_defs,
                catalog_warnings: Vec::new(),
                mode,
                channel,
                now,
                icon_data_dir: None,
                deadline: Instant::now() + CHECK_BUDGET,
            },
            transport,
            api,
        )
    }

    fn find<'a>(check: &'a UpdateCheck, id: &str) -> &'a PluginCheck {
        check.plugins.iter().find(|plugin| plugin.id == id).unwrap()
    }

    #[test]
    fn one_official_request_covers_catalog_plugins_and_host_without_rest() {
        let dir = temp_dir();
        let registry = registry_json(
            vec![
                plugin_entry(
                    "codex",
                    "codex_desktop_background",
                    "CodexBackgroundStudio-",
                    Some("0.6.0"),
                    None,
                ),
                plugin_entry(
                    "newbie",
                    "newbie_desktop_background",
                    "NewbieBackgroundStudio-",
                    Some("0.1.0"),
                    None,
                ),
            ],
            Some("0.3.7"),
        );
        let transport = FakeTransport::new(vec![body(&registry)]);
        let api = FakeApi::err("不应调用");
        let check = run(
            &dir,
            &[codex_builtin()],
            &[],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Stable,
            0,
        );
        assert_eq!(transport.count(), 1);
        assert_eq!(check.rest_calls, 0);
        assert!(api.calls.borrow().is_empty());
        // 官方清单新增的插件进入目录。
        let newbie = find(&check, "newbie");
        assert_eq!(newbie.catalog_source, CatalogSource::Official);
        assert_eq!(newbie.latest.as_ref().unwrap().version, "0.1.0");
        assert_eq!(
            find(&check, "codex")
                .latest
                .as_ref()
                .unwrap()
                .sha256
                .as_deref(),
            Some("a".repeat(64).as_str())
        );
        assert_eq!(
            check.host.as_ref().unwrap().latest_version.as_deref(),
            Some("0.3.7")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn official_failure_never_falls_back_to_rest() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![Err("离线".to_string())]);
        let api = FakeApi::ok(json!({}));
        let check = run(
            &dir,
            &[codex_builtin()],
            &[],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Stable,
            0,
        );
        assert_eq!(check.rest_calls, 0);
        let codex = find(&check, "codex");
        assert!(codex.latest.is_none());
        assert!(!codex.aggregate_error);
        assert!(check.host.is_err());
        assert_eq!(check.registry.origin, RegistryOrigin::Unavailable);
        // 内置目录仍可离线 bootstrap。
        assert_eq!(check.catalog.len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_registry_entry_does_not_use_rest() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![body(&registry_json(vec![], Some("0.3.7")))]);
        let api = FakeApi::ok(json!({}));
        let check = run(
            &dir,
            &[codex_builtin()],
            &[],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Stable,
            0,
        );
        let codex = find(&check, "codex");
        assert_eq!(codex.catalog_source, CatalogSource::Builtin);
        assert!(codex.latest.is_none());
        assert!(codex.error.as_deref().unwrap().contains("未收录"));
        assert_eq!(check.rest_calls, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_local_mirror_does_not_shadow_official_metadata() {
        let dir = temp_dir();
        let mut registry = sample_registry();
        registry["plugins"][0]["displayName"] = json!("Codex 新名字");
        let transport = FakeTransport::new(vec![body(&registry)]);
        let api = FakeApi::ok(json!({}));
        // 旧版本写出的本地副本：同 id、owner 仍为官方。
        let mut mirror = codex_builtin();
        mirror.display_name = "Codex 旧名字".to_string();
        let check = run(
            &dir,
            &[codex_builtin()],
            &[mirror],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Stable,
            0,
        );
        let codex = find(&check, "codex");
        assert_eq!(codex.catalog_source, CatalogSource::Official);
        assert_eq!(codex.display_name, "Codex 新名字");
        assert_eq!(codex.release_source, ReleaseSource::Official);
        assert_eq!(check.rest_calls, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn custom_source_with_official_id_uses_its_own_repo_not_official_release() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![body(&sample_registry())]);
        let api = FakeApi::ok(json!({
            "tag_name": "v9.9.9",
            "assets": [{ "name": "ForkCodex-9.9.9-plugin.zip" }]
        }));
        let fork = def("codex", "someone", "codex_fork", "ForkCodex-");
        let check = run(
            &dir,
            &[codex_builtin()],
            &[fork],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Stable,
            0,
        );
        let codex = find(&check, "codex");
        assert_eq!(codex.catalog_source, CatalogSource::Local);
        assert_eq!(codex.release_source, ReleaseSource::GithubApi);
        let latest = codex.latest.as_ref().unwrap();
        assert_eq!(latest.version, "9.9.9");
        assert_eq!(latest.sha256, None);
        assert_eq!(
            latest.download_url,
            "https://github.com/someone/codex_fork/releases/download/v9.9.9/ForkCodex-9.9.9-plugin.zip"
        );
        assert_eq!(
            api.calls.borrow().as_slice(),
            ["repos/someone/codex_fork/releases/latest"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn custom_rest_results_are_cached_and_backed_off() {
        let dir = temp_dir();
        let custom = def("thirdparty", "someone", "tp", "Tp-");
        let ok = FakeApi::ok(
            json!({ "tag_name": "v1.0.0", "assets": [{ "name": "Tp-1.0.0-plugin.zip" }] }),
        );
        let transport = FakeTransport::new(vec![body(&sample_registry())]);
        let first = run(
            &dir,
            &[],
            &[custom.clone()],
            &transport,
            &ok,
            RefreshMode::Auto,
            Channel::Stable,
            0,
        );
        assert_eq!(first.rest_calls, 1);

        // TTL 内自动刷新命中缓存。
        let transport = FakeTransport::new(vec![]);
        let cached = run(
            &dir,
            &[],
            &[custom.clone()],
            &transport,
            &ok,
            RefreshMode::Auto,
            Channel::Stable,
            60,
        );
        assert_eq!(cached.rest_calls, 0);
        assert_eq!(cached.rest_cache_hits, 1);
        assert_eq!(
            find(&cached, "thirdparty").latest.as_ref().unwrap().version,
            "1.0.0"
        );

        // 手动刷新失败：回用上次结果并进入退避。
        let failing = FakeApi::err("GitHub API 限流");
        let transport = FakeTransport::new(vec![]);
        let failed = run(
            &dir,
            &[],
            &[custom.clone()],
            &transport,
            &failing,
            RefreshMode::Force,
            Channel::Stable,
            120,
        );
        assert_eq!(failed.rest_calls, 1);
        let plugin = find(&failed, "thirdparty");
        assert!(plugin.stale);
        assert_eq!(plugin.latest.as_ref().unwrap().version, "1.0.0");

        // 退避期内即使手动刷新也不再调用 REST。
        let transport = FakeTransport::new(vec![]);
        let backoff = run(
            &dir,
            &[],
            &[custom],
            &transport,
            &failing,
            RefreshMode::Force,
            Channel::Stable,
            125,
        );
        assert_eq!(backoff.rest_calls, 0);
        assert!(find(&backoff, "thirdparty")
            .error
            .as_deref()
            .unwrap()
            .contains("退避"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn channels_follow_existing_semantics() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![body(&sample_registry())]);
        let api = FakeApi::ok(json!({}));
        let stable = run(
            &dir,
            &[codex_builtin()],
            &[],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Stable,
            0,
        );
        assert_eq!(
            find(&stable, "codex").latest.as_ref().unwrap().version,
            "0.6.0"
        );
        let pre = run(
            &dir,
            &[codex_builtin()],
            &[],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Prerelease,
            1,
        );
        assert_eq!(
            find(&pre, "codex").latest.as_ref().unwrap().version,
            "0.6.1-beta.1"
        );
        // 壳始终跟随 stable。
        assert_eq!(
            pre.host.as_ref().unwrap().latest_version.as_deref(),
            Some("0.3.7")
        );

        // prerelease 渠道没有预发布时不回落 stable（沿用旧行为）。
        let dir2 = temp_dir();
        let registry = registry_json(
            vec![plugin_entry(
                "codex",
                "codex_desktop_background",
                "CodexBackgroundStudio-",
                Some("0.6.0"),
                None,
            )],
            Some("0.3.7"),
        );
        let transport = FakeTransport::new(vec![body(&registry)]);
        let pre = run(
            &dir2,
            &[codex_builtin()],
            &[],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Prerelease,
            0,
        );
        let codex = find(&pre, "codex");
        assert!(codex.latest.is_none());
        assert!(codex.error.as_deref().unwrap().contains("预发布"));
        assert_eq!(
            Channel::for_host_version("0.3.7-beta.1"),
            Channel::Prerelease
        );
        assert_eq!(Channel::for_host_version("0.3.7"), Channel::Stable);
        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(dir2);
    }

    #[test]
    fn latest_identity_detects_source_replacement() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![body(&sample_registry())]);
        let api = FakeApi::ok(json!({}));
        let check = run(
            &dir,
            &[codex_builtin()],
            &[],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Stable,
            0,
        );
        let latest = find(&check, "codex").latest.clone().unwrap();
        let mut official = codex_builtin();
        official.source = CatalogSource::Official;
        assert!(latest.matches(&official));
        assert!(latest.matches(&codex_builtin()));
        let mut fork = def("codex", "someone", "codex_fork", "CodexBackgroundStudio-");
        fork.source = CatalogSource::Local;
        assert!(!latest.matches(&fork));
        // 同 owner/repo 但来源变为本地自定义：官方版本也不得复用。
        let mut local_same = codex_builtin();
        local_same.source = CatalogSource::Local;
        assert!(!latest.matches(&local_same));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stable_channel_reads_latest_release_object() {
        let value = json!({
            "tag_name": "v0.5.4",
            "assets": [{ "name": "CodexBackgroundStudio-0.5.4-plugin.zip" }]
        });
        assert_eq!(
            select_plugin_asset(&codex_builtin(), &value, false).unwrap(),
            (
                "0.5.4".to_string(),
                "CodexBackgroundStudio-0.5.4-plugin.zip".to_string()
            )
        );
    }

    #[test]
    fn prerelease_channel_ignores_stable_and_draft_releases() {
        let value = json!([
            {
                "tag_name": "v0.5.4",
                "draft": false,
                "prerelease": false,
                "assets": [{ "name": "CodexBackgroundStudio-0.5.4-plugin.zip" }]
            },
            {
                "tag_name": "v0.5.5-beta.2",
                "draft": true,
                "prerelease": true,
                "assets": [{ "name": "CodexBackgroundStudio-0.5.5-beta.2-plugin.zip" }]
            },
            {
                "tag_name": "v0.5.5-beta.1",
                "draft": false,
                "prerelease": true,
                "assets": [{ "name": "CodexBackgroundStudio-0.5.5-beta.1-plugin.zip" }]
            }
        ]);
        assert_eq!(
            select_plugin_asset(&codex_builtin(), &value, true)
                .unwrap()
                .0,
            "0.5.5-beta.1"
        );
    }

    #[test]
    fn redacts_proxy_credentials() {
        assert_eq!(
            redact_proxy_url("http://user:pw@127.0.0.1:7890"),
            "http://***@127.0.0.1:7890/"
        );
        assert_eq!(redact_proxy_url(""), "");
    }

    #[test]
    fn stale_cache_fallback_is_reported_as_degraded() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![body(&sample_registry()), Err("离线".to_string())]);
        let api = FakeApi::ok(json!({}));
        let ok = run(
            &dir,
            &[codex_builtin()],
            &[],
            &transport,
            &api,
            RefreshMode::Auto,
            Channel::Stable,
            0,
        );
        assert_eq!(ok.status(), "ok");
        let stale = run(
            &dir,
            &[codex_builtin()],
            &[],
            &transport,
            &api,
            RefreshMode::Force,
            Channel::Stable,
            10,
        );
        assert_eq!(stale.registry.origin, RegistryOrigin::StaleCache);
        assert_eq!(stale.status(), "degraded");
        // 缓存版本仍可用，但标为陈旧。
        assert!(find(&stale, "codex").stale);
        let report = diagnostics_json(&stale, &PluginsState::default(), &dir);
        assert_eq!(report["ok"], json!(false));
        assert_eq!(report["official"]["usedCacheFallback"], json!(true));
        assert_eq!(report["totals"]["officialRegistryGets"], json!(1));
        assert_eq!(report["totals"]["officialIconGets"], json!(0));
        assert_eq!(report["totals"]["restCalls"], json!(0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn exhausted_budget_skips_rest_without_backoff() {
        let dir = temp_dir();
        let transport = FakeTransport::new(vec![body(&sample_registry())]);
        let api = FakeApi::ok(
            json!({ "tag_name": "v1.0.0", "assets": [{ "name": "Tp-1.0.0-plugin.zip" }] }),
        );
        let mut custom = def("thirdparty", "someone", "tp", "Tp-");
        custom.source = CatalogSource::Local;
        let check = check_updates(
            CheckInputs {
                cache_dir: &dir,
                builtin: &[],
                local: &[custom],
                catalog_warnings: Vec::new(),
                mode: RefreshMode::Force,
                channel: Channel::Stable,
                now: 0,
                icon_data_dir: None,
                deadline: Instant::now(),
            },
            &transport,
            &api,
        );
        assert_eq!(check.rest_calls, 0);
        assert_eq!(check.budget_skipped.len(), 1);
        assert_eq!(check.status(), "degraded");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn icon_gets_are_counted_separately() {
        let dir = temp_dir();
        let registry = registry_json(
            vec![plugin_entry(
                "newbie",
                "newbie_desktop_background",
                "NewbieBackgroundStudio-",
                Some("0.1.0"),
                None,
            )],
            Some("0.3.7"),
        );
        let png = {
            let mut bytes = Vec::new();
            image::DynamicImage::new_rgba8(2, 2)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .unwrap();
            bytes
        };
        let transport = FakeTransport::new(vec![
            body(&registry),
            Ok(crate::registry::HttpOutcome::Body {
                body: png,
                etag: None,
            }),
        ]);
        let api = FakeApi::ok(json!({}));
        let check = check_updates(
            CheckInputs {
                cache_dir: &dir,
                builtin: &[],
                local: &[],
                catalog_warnings: Vec::new(),
                mode: RefreshMode::Auto,
                channel: Channel::Stable,
                now: 0,
                icon_data_dir: Some(&dir),
                deadline: Instant::now() + CHECK_BUDGET,
            },
            &transport,
            &api,
        );
        assert_eq!(check.registry.http_requests, 1);
        assert_eq!(check.icon_requests, 1);
        assert!(check.icon_errors.is_empty(), "{:?}", check.icon_errors);
        assert!(transport.requests.borrow()[1]
            .0
            .ends_with("/registry/icons/newbie.png"));
        let newbie = check.catalog.iter().find(|def| def.id == "newbie").unwrap();
        assert!(catalog::resolve_icon_path(&dir, newbie).is_some());
        let _ = fs::remove_dir_all(dir);
    }
}
