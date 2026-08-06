use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    /// 插件安装与 plugins.json 所在根目录；缺省为 bootstrap 目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_root: Option<String>,
}

pub fn bootstrap_directory() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("BackgroundStudio")
}

pub fn host_config_path() -> PathBuf {
    bootstrap_directory().join("host.json")
}

pub fn load_host_config() -> Result<HostConfig, String> {
    let path = host_config_path();
    if !path.exists() {
        return Ok(HostConfig::default());
    }
    let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    serde_json::from_str(&raw).map_err(|error| error.to_string())
}

pub fn save_host_config(config: &HostConfig) -> Result<(), String> {
    let bootstrap = bootstrap_directory();
    fs::create_dir_all(&bootstrap).map_err(|error| error.to_string())?;
    let raw = serde_json::to_string_pretty(config).map_err(|error| error.to_string())?;
    fs::write(host_config_path(), raw).map_err(|error| error.to_string())
}

pub fn resolve_data_directory() -> Result<PathBuf, String> {
    let config = load_host_config()?;
    if let Some(root) = config.data_root.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let path = PathBuf::from(root);
        fs::create_dir_all(&path).map_err(|error| error.to_string())?;
        return Ok(path);
    }
    let bootstrap = bootstrap_directory();
    fs::create_dir_all(&bootstrap).map_err(|error| error.to_string())?;
    Ok(bootstrap)
}

pub fn set_data_root(path: &Path) -> Result<PathBuf, String> {
    let canonical = if path.exists() {
        fs::canonicalize(path).map_err(|error| error.to_string())?
    } else {
        fs::create_dir_all(path).map_err(|error| error.to_string())?;
        fs::canonicalize(path).map_err(|error| error.to_string())?
    };
    let mut config = load_host_config()?;
    config.data_root = Some(canonical.to_string_lossy().into_owned());
    save_host_config(&config)?;
    Ok(canonical)
}
