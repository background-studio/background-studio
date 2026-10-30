use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// 不使用代理（当前默认行为）。
    #[default]
    Off,
    /// 使用环境变量 / 操作系统系统代理。
    System,
    /// 使用下方自定义代理地址。
    Custom,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxySettings {
    #[serde(default)]
    pub mode: ProxyMode,
    #[serde(default)]
    pub url: String,
}

impl ProxySettings {
    pub fn normalized(mut self) -> Self {
        self.url = self.url.trim().to_string();
        self
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.mode == ProxyMode::Custom && self.url.trim().is_empty() {
            return Err("自定义代理地址不能为空。".to_string());
        }
        Ok(())
    }
}

/// 按壳代理设置配置好的 blocking 客户端 builder，调用方可再叠加超时 / 重定向策略。
pub fn blocking_client_builder(
    settings: &ProxySettings,
) -> Result<reqwest::blocking::ClientBuilder, String> {
    settings.validate()?;
    let mut builder = reqwest::blocking::Client::builder();
    match settings.mode {
        ProxyMode::Off => {
            builder = builder.no_proxy();
        }
        ProxyMode::System => {
            // system-proxy feature：读取环境变量与 OS 系统代理。
        }
        ProxyMode::Custom => {
            let proxy = reqwest::Proxy::all(settings.url.trim())
                .map_err(|error| format!("无效的代理地址：{error}"))?;
            builder = builder.proxy(proxy);
        }
    }
    Ok(builder)
}

/// 让子进程（如 `gh api`）的代理行为尽量与壳设置一致。
pub fn apply_to_command(command: &mut Command, settings: &ProxySettings) {
    match settings.mode {
        ProxyMode::Off => {
            command
                .env_remove("HTTP_PROXY")
                .env_remove("HTTPS_PROXY")
                .env_remove("ALL_PROXY")
                .env_remove("http_proxy")
                .env_remove("https_proxy")
                .env_remove("all_proxy");
        }
        ProxyMode::System => {}
        ProxyMode::Custom => {
            let url = settings.url.trim();
            if !url.is_empty() {
                command
                    .env("HTTP_PROXY", url)
                    .env("HTTPS_PROXY", url)
                    .env("ALL_PROXY", url)
                    .env("http_proxy", url)
                    .env("https_proxy", url)
                    .env("all_proxy", url);
            }
        }
    }
}
