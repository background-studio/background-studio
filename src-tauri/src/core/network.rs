use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Write},
    net::{IpAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use ipnet::{Ipv4Net, Ipv6Net};
use percent_encoding::percent_decode_str;
use reqwest::{
    header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, LOCATION},
    redirect::Policy,
};
use url::Url;
use uuid::Uuid;

use super::models::MediaKind;

const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

pub struct RemoteDownload {
    pub temporary_path: PathBuf,
    pub original_name: String,
    pub mime_type: String,
    pub kind: MediaKind,
    pub byte_size: u64,
    pub source_url: String,
}

fn blocked_ipv4_networks() -> Vec<Ipv4Net> {
    [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "224.0.0.0/4",
        "240.0.0.0/4",
    ]
    .into_iter()
    .filter_map(|network| network.parse().ok())
    .collect()
}

fn blocked_ipv6_networks() -> Vec<Ipv6Net> {
    [
        "::/128",
        "::1/128",
        "fc00::/7",
        "fe80::/10",
        "ff00::/8",
        "2001:db8::/32",
    ]
    .into_iter()
    .filter_map(|network| network.parse().ok())
    .collect()
}

pub fn is_blocked_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => blocked_ipv4_networks()
            .iter()
            .any(|network| network.contains(&address)),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return is_blocked_ip(IpAddr::V4(mapped));
            }
            blocked_ipv6_networks()
                .iter()
                .any(|network| network.contains(&address))
        }
    }
}

#[cfg(test)]
pub fn is_blocked_address(address: &str) -> bool {
    let unscoped = address.split('%').next().unwrap_or(address);
    IpAddr::from_str(unscoped).map_or(true, is_blocked_ip)
}

pub fn validate_remote_url(value: &str) -> Result<Url, String> {
    if value.len() > 2048 {
        return Err("网络地址过长。".to_string());
    }
    let url = Url::parse(value).map_err(|_| "请输入有效的网络地址。".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("仅支持不含账号信息的 HTTP 或 HTTPS 地址。".to_string());
    }
    let hostname = url
        .host_str()
        .filter(|hostname| !hostname.is_empty())
        .ok_or_else(|| "请输入有效的网络地址。".to_string())?;
    if hostname.eq_ignore_ascii_case("localhost") {
        return Err("不允许访问本机或私有网络地址。".to_string());
    }
    if let Ok(address) = IpAddr::from_str(hostname) {
        if is_blocked_ip(address) {
            return Err("不允许访问本机、私有或保留网络地址。".to_string());
        }
    }
    Ok(url)
}

fn checked_lookup(url: &Url) -> Result<Vec<std::net::SocketAddr>, String> {
    let hostname = url
        .host_str()
        .ok_or_else(|| "网络地址缺少主机名。".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "网络地址端口无效。".to_string())?;
    let mut unique = HashSet::new();
    let mut addresses = Vec::new();
    for address in (hostname, port)
        .to_socket_addrs()
        .map_err(|error| format!("目标域名解析失败：{error}"))?
    {
        if is_blocked_ip(address.ip()) {
            return Err("目标域名解析到了本机、私有或保留网络地址，已拒绝下载。".to_string());
        }
        if unique.insert(address) {
            addresses.push(address);
        }
    }
    if addresses.is_empty() {
        return Err("目标域名没有可用的公网地址。".to_string());
    }
    Ok(addresses)
}

fn media_type(mime_type: &str) -> Option<(&'static str, MediaKind, u64)> {
    match mime_type {
        "image/png" => Some((".png", MediaKind::Image, MAX_IMAGE_BYTES)),
        "image/jpeg" => Some((".jpg", MediaKind::Image, MAX_IMAGE_BYTES)),
        "image/webp" => Some((".webp", MediaKind::Image, MAX_IMAGE_BYTES)),
        "image/gif" => Some((".gif", MediaKind::Image, MAX_IMAGE_BYTES)),
        "image/avif" => Some((".avif", MediaKind::Image, MAX_IMAGE_BYTES)),
        "video/mp4" => Some((".mp4", MediaKind::Video, MAX_VIDEO_BYTES)),
        "video/webm" => Some((".webm", MediaKind::Video, MAX_VIDEO_BYTES)),
        "video/ogg" => Some((".ogv", MediaKind::Video, MAX_VIDEO_BYTES)),
        "video/quicktime" => Some((".mov", MediaKind::Video, MAX_VIDEO_BYTES)),
        _ => None,
    }
}

fn safe_file_name(name: &str, extension: &str) -> String {
    let mut safe = name
        .chars()
        .map(|character| {
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) || character.is_control()
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    safe = safe.trim().to_string();
    let stem = Path::new(&safe)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("remote-media");
    let maximum = 180usize.saturating_sub(extension.len()).max(1);
    let stem = stem.chars().take(maximum).collect::<String>();
    format!(
        "{}{}",
        if stem.is_empty() {
            "remote-media"
        } else {
            &stem
        },
        extension
    )
}

fn file_name_from_headers(url: &Url, disposition: Option<&str>, extension: &str) -> String {
    let encoded = disposition.and_then(|header| {
        let lower = header.to_ascii_lowercase();
        let index = lower.find("filename*=utf-8''")?;
        header[index + "filename*=utf-8''".len()..]
            .split(';')
            .next()
    });
    let simple = disposition.and_then(|header| {
        let lower = header.to_ascii_lowercase();
        let index = lower.find("filename=")?;
        Some(
            header[index + "filename=".len()..]
                .split(';')
                .next()?
                .trim_matches('"'),
        )
    });
    let url_name = url
        .path_segments()
        .and_then(Iterator::last)
        .filter(|name| !name.is_empty());
    let candidate = encoded
        .and_then(|name| percent_decode_str(name).decode_utf8().ok())
        .map(|name| name.into_owned())
        .or_else(|| simple.map(str::to_string))
        .or_else(|| url_name.map(str::to_string))
        .unwrap_or_else(|| format!("remote-media{extension}"));
    safe_file_name(&candidate, extension)
}

pub fn download_remote_media(
    value: &str,
    temporary_directory: &Path,
) -> Result<RemoteDownload, String> {
    let mut url = validate_remote_url(value)?;
    fs::create_dir_all(temporary_directory).map_err(|error| error.to_string())?;

    for redirects in 0..=MAX_REDIRECTS {
        let addresses = checked_lookup(&url)?;
        let hostname = url
            .host_str()
            .ok_or_else(|| "网络地址缺少主机名。".to_string())?;
        let client = reqwest::blocking::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .resolve_to_addrs(hostname, &addresses)
            .build()
            .map_err(|error| error.to_string())?;
        let mut response = client
            .get(url.clone())
            .header("User-Agent", "BackgroundStudioHost/0.1")
            .header("Accept", "image/*,video/*;q=0.9")
            .send()
            .map_err(|error| format!("下载连接失败：{error}"))?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| format!("下载失败，服务器返回 HTTP {}。", status.as_u16()))?;
            if redirects == MAX_REDIRECTS {
                return Err("网络媒体重定向次数过多。".to_string());
            }
            url = validate_remote_url(
                &url.join(location)
                    .map_err(|_| "服务器返回了无效的重定向地址。".to_string())?
                    .to_string(),
            )?;
            continue;
        }
        if !status.is_success() {
            return Err(format!("下载失败，服务器返回 HTTP {}。", status.as_u16()));
        }
        let mime_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let (extension, kind, limit) = media_type(&mime_type).ok_or_else(|| {
            format!(
                "不支持服务器返回的媒体类型：{}。",
                if mime_type.is_empty() {
                    "未知"
                } else {
                    &mime_type
                }
            )
        })?;
        if response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|size| size > limit)
        {
            return Err(format!("媒体文件超过 {} MB 上限。", limit / 1024 / 1024));
        }
        let disposition = response
            .headers()
            .get(CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok());
        let original_name = file_name_from_headers(&url, disposition, extension);
        let temporary_path =
            temporary_directory.join(format!("{}{extension}.download", Uuid::new_v4()));
        let mut output = File::create(&temporary_path).map_err(|error| error.to_string())?;
        let mut byte_size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        let copy_result = loop {
            match response.read(&mut buffer) {
                Ok(0) => break Ok(byte_size),
                Ok(read) => {
                    byte_size = byte_size.saturating_add(read as u64);
                    if byte_size > limit {
                        break Err(format!("媒体文件超过 {} MB 上限。", limit / 1024 / 1024));
                    }
                    if let Err(error) = output.write_all(&buffer[..read]) {
                        break Err(error.to_string());
                    }
                }
                Err(error) => break Err(format!("下载中断：{error}")),
            }
        };
        match copy_result {
            Ok(byte_size) => {
                return Ok(RemoteDownload {
                    temporary_path,
                    original_name,
                    mime_type,
                    kind,
                    byte_size,
                    source_url: url.to_string(),
                });
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary_path);
                return Err(error);
            }
        }
    }
    Err("网络媒体重定向次数过多。".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_private_and_reserved_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.1.2",
            "172.20.1.2",
            "192.168.1.2",
            "169.254.169.254",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_blocked_address(address), "{address}");
        }
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(!is_blocked_address(address), "{address}");
        }
    }

    #[test]
    fn validates_remote_urls() {
        assert!(validate_remote_url("https://images.example.com/background.webp").is_ok());
        for url in [
            "file:///C:/secret.txt",
            "http://localhost:3000/image.png",
            "http://127.0.0.1/image.png",
            "https://user:pass@example.com/image.png",
        ] {
            assert!(validate_remote_url(url).is_err(), "{url}");
        }
    }
}
