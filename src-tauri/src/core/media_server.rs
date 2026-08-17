use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use sha2::{Digest, Sha256};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use uuid::Uuid;

use super::{
    media::{MediaLibrary, ResolvedMedia},
    models::SlideshowOrder,
};

#[derive(Clone)]
struct ServedMedia {
    path: PathBuf,
    mime_type: String,
    byte_size: u64,
    revision: String,
}

fn preview_revision(media: &ResolvedMedia) -> String {
    let mut hasher = Sha256::new();
    hasher.update(media.path.to_string_lossy().as_bytes());
    hasher.update(media.byte_size.to_le_bytes());
    hasher.update(media.sha256.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn header(name: &str, value: impl AsRef<str>) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_ref().as_bytes())
        .expect("static HTTP header is valid")
}

fn common_headers(mime_type: &str) -> Vec<Header> {
    vec![
        header("Content-Type", mime_type),
        header("Accept-Ranges", "bytes"),
        header("Cache-Control", "private, max-age=3600"),
        header("Cross-Origin-Resource-Policy", "cross-origin"),
        header("Access-Control-Allow-Origin", "*"),
        header("X-Content-Type-Options", "nosniff"),
    ]
}

fn plain(request: Request, status: u16) {
    let response = Response::from_string("")
        .with_status_code(StatusCode(status))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("Access-Control-Allow-Origin", "*"));
    let _ = request.respond(response);
}

fn parse_range(request: &Request, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(value) = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Range"))
        .map(|header| header.value.as_str())
    else {
        return Ok(None);
    };
    parse_range_value(value, size).map(Some)
}

fn parse_range_value(value: &str, size: u64) -> Result<(u64, u64), ()> {
    let Some(range) = value.strip_prefix("bytes=") else {
        return Err(());
    };
    if range.contains(',') {
        return Err(());
    }
    let Some((start, end)) = range.split_once('-') else {
        return Err(());
    };
    if start.is_empty() {
        let suffix_length = end.parse::<u64>().map_err(|_| ())?;
        if size == 0 || suffix_length == 0 {
            return Err(());
        }
        let start = size.saturating_sub(suffix_length);
        return Ok((start, size - 1));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    let requested_end = if end.is_empty() {
        size.saturating_sub(1)
    } else {
        end.parse::<u64>().map_err(|_| ())?
    };
    if size == 0 || start >= size || requested_end < start {
        return Err(());
    }
    Ok((start, requested_end.min(size - 1)))
}

pub fn is_safe_media_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains("..")
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

pub fn extract_media_id(url_path: &str, token: &str) -> Option<String> {
    let path = url_path.split('?').next().unwrap_or(url_path);
    if token.is_empty() || token.contains('/') || token.contains("..") {
        return None;
    }
    let prefix = format!("/{token}/media/");
    let id = path.strip_prefix(&prefix)?;
    if !is_safe_media_id(id) {
        return None;
    }
    Some(id.to_string())
}

fn serve(request: Request, token: &str, media: &Arc<RwLock<HashMap<String, ServedMedia>>>) {
    if !matches!(request.method(), Method::Get | Method::Head) {
        plain(request, 405);
        return;
    }
    let Some(id) = extract_media_id(request.url(), token) else {
        plain(request, 404);
        return;
    };
    let Some(item) = media.read().ok().and_then(|items| items.get(&id).cloned()) else {
        plain(request, 404);
        return;
    };
    let canonical = match item.path.canonicalize() {
        Ok(path) => path,
        Err(_) => {
            plain(request, 404);
            return;
        }
    };
    if canonical != item.path {
        plain(request, 404);
        return;
    }
    let Ok(mut file) = File::open(&canonical) else {
        plain(request, 404);
        return;
    };
    let size = file
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(item.byte_size);
    let range = match parse_range(&request, size) {
        Ok(range) => range,
        Err(()) => {
            let response = Response::empty(StatusCode(416))
                .with_header(header("Content-Range", format!("bytes */{size}")));
            let _ = request.respond(response);
            return;
        }
    };
    let is_head = request.method() == &Method::Head;
    let mut headers = common_headers(&item.mime_type);
    match range {
        Some((start, end)) => {
            let length = end - start + 1;
            headers.push(header(
                "Content-Range",
                format!("bytes {start}-{end}/{size}"),
            ));
            if is_head {
                headers.push(header("Content-Length", length.to_string()));
                let mut response = Response::empty(StatusCode(206));
                for header in headers {
                    response.add_header(header);
                }
                let _ = request.respond(response);
                return;
            }
            if file.seek(SeekFrom::Start(start)).is_err() {
                plain(request, 404);
                return;
            }
            let response = Response::new(
                StatusCode(206),
                headers,
                file.take(length),
                Some(length as usize),
                None,
            );
            let _ = request.respond(response);
        }
        None => {
            if is_head {
                headers.push(header("Content-Length", size.to_string()));
                let mut response = Response::empty(StatusCode(200));
                for header in headers {
                    response.add_header(header);
                }
                let _ = request.respond(response);
                return;
            }
            let response = Response::new(StatusCode(200), headers, file, Some(size as usize), None);
            let _ = request.respond(response);
        }
    }
}

pub struct MediaServer {
    token: String,
    origin: String,
    media: Arc<RwLock<HashMap<String, ServedMedia>>>,
    server: Arc<Server>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MediaServer {
    pub fn start() -> Result<Self, String> {
        let server = Arc::new(Server::http("127.0.0.1:0").map_err(|error| error.to_string())?);
        let port = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| "媒体服务端口分配失败。".to_string())?
            .port();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let media = Arc::new(RwLock::new(HashMap::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_server = Arc::clone(&server);
        let thread_media = Arc::clone(&media);
        let thread_stopping = Arc::clone(&stopping);
        let thread_token = token.clone();
        let thread = thread::Builder::new()
            .name("host-media".to_string())
            .spawn(move || {
                while !thread_stopping.load(Ordering::Relaxed) {
                    match thread_server.recv_timeout(Duration::from_millis(500)) {
                        Ok(Some(request)) => {
                            let request_token = thread_token.clone();
                            let request_media = Arc::clone(&thread_media);
                            if thread::Builder::new()
                                .name("host-media-request".to_string())
                                .spawn(move || serve(request, &request_token, &request_media))
                                .is_err()
                            {
                                eprintln!("媒体服务无法创建请求线程。");
                            }
                        }
                        Ok(None) => {}
                        Err(_) if thread_stopping.load(Ordering::Relaxed) => break,
                        Err(_) => {}
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            token,
            origin: format!("http://127.0.0.1:{port}"),
            media,
            server,
            stopping,
            thread: Some(thread),
        })
    }

    #[cfg(test)]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    #[cfg(test)]
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn sync(&self, library: &mut MediaLibrary, items: &[(String, SlideshowOrder)]) {
        let mut served = HashMap::new();
        for (id, order) in items {
            let Some(item) = library.get_by_id(id) else {
                continue;
            };
            let Ok(resolved) = library.resolve_playback(&item, *order, false) else {
                continue;
            };
            let revision = preview_revision(&resolved);
            served.insert(
                id.clone(),
                ServedMedia {
                    path: resolved.path,
                    mime_type: resolved.mime_type,
                    byte_size: resolved.byte_size,
                    revision,
                },
            );
        }
        if let Ok(mut media) = self.media.write() {
            *media = served;
        }
    }

    pub fn url_for(&self, id: &str) -> Option<String> {
        if !is_safe_media_id(id) {
            return None;
        }
        let revision = self
            .media
            .read()
            .ok()
            .and_then(|items| items.get(id).map(|item| item.revision.clone()))?;
        Some(format!(
            "{}/{}/media/{}?v={revision}",
            self.origin, self.token, id
        ))
    }

    #[cfg(test)]
    pub fn register_for_test(&self, id: &str, path: PathBuf, mime_type: &str, byte_size: u64) {
        if let Ok(mut media) = self.media.write() {
            media.insert(
                id.to_string(),
                ServedMedia {
                    path,
                    mime_type: mime_type.to_string(),
                    byte_size,
                    revision: "test".to_string(),
                },
            );
        }
    }
}

impl Drop for MediaServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::media::minimal_png;
    use std::{fs, fs::File, io::Write};

    #[test]
    fn rejects_path_traversal_and_wrong_token() {
        let token = "a".repeat(64);
        assert!(extract_media_id(&format!("/{token}/media/abc-def"), &token).is_some());
        assert!(extract_media_id(&format!("/{token}/media/../secret"), &token).is_none());
        assert!(extract_media_id(&format!("/{token}/media/foo/bar"), &token).is_none());
        assert!(extract_media_id(&format!("/{token}/media/%2e%2e/secret"), &token).is_none());
        assert!(extract_media_id("/wrong/media/abc-def", &token).is_none());
        assert!(!is_safe_media_id("../x"));
        assert!(!is_safe_media_id("a/b"));
    }

    #[test]
    fn parses_standard_and_suffix_ranges() {
        assert_eq!(parse_range_value("bytes=10-19", 100), Ok((10, 19)));
        assert_eq!(parse_range_value("bytes=-10", 100), Ok((90, 99)));
        assert!(parse_range_value("bytes=-0", 100).is_err());
    }

    #[test]
    fn serves_range_and_rejects_unknown_id() {
        let root = std::env::temp_dir().join(format!("host-media-http-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("ok.png");
        let bytes = minimal_png();
        File::create(&file).unwrap().write_all(&bytes).unwrap();
        let server = MediaServer::start().unwrap();
        let id = "11111111-1111-1111-1111-111111111111";
        server.register_for_test(
            id,
            file.canonicalize().unwrap(),
            "image/png",
            bytes.len() as u64,
        );
        let url = format!("{}/{}/media/{id}", server.origin(), server.token());
        let response = reqwest::blocking::get(&url).unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(response.bytes().unwrap().len(), bytes.len());

        let traversal = format!("{}/{}/media/../{}", server.origin(), server.token(), id);
        let denied = reqwest::blocking::get(traversal).unwrap();
        assert_eq!(denied.status().as_u16(), 404);

        let wrong = format!("{}/deadbeef/media/{id}", server.origin());
        let denied = reqwest::blocking::get(wrong).unwrap();
        assert_eq!(denied.status().as_u16(), 404);
        let _ = fs::remove_dir_all(root);
    }
}
