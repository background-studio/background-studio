use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

use super::protocol::{decode_response_for, encode_request, MAX_IPC_LINE_BYTES};

fn timeout_for(cmd: &str) -> std::time::Duration {
    match cmd {
        "status" => std::time::Duration::from_millis(800),
        "hello" => std::time::Duration::from_secs(5),
        "open-ui" | "quit-keep-target" | "shutdown" => std::time::Duration::from_secs(5),
        "configure" => std::time::Duration::from_secs(15),
        "pause" => std::time::Duration::from_secs(15),
        "apply" | "restore" => std::time::Duration::from_secs(75),
        _ => std::time::Duration::from_secs(15),
    }
}

pub async fn request(pipe_name: &str, cmd: &str) -> Result<Value, String> {
    request_with_params(pipe_name, cmd, None).await
}

pub async fn request_with_params(
    pipe_name: &str,
    cmd: &str,
    params: Option<&Value>,
) -> Result<Value, String> {
    #[cfg(windows)]
    {
        tokio::time::timeout(timeout_for(cmd), request_windows(pipe_name, cmd, params))
            .await
            .map_err(|_| format!("插件命令 {cmd} 超时。"))?
    }
    #[cfg(not(windows))]
    {
        let _ = (pipe_name, cmd, params);
        Err("插件 IPC 仅支持 Windows。".to_string())
    }
}

#[cfg(windows)]
async fn request_windows(
    pipe_name: &str,
    cmd: &str,
    params: Option<&Value>,
) -> Result<Value, String> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let client = ClientOptions::new()
        .open(pipe_name)
        .map_err(|error| format!("无法连接插件管道（插件是否已启动？）：{error}"))?;
    let (reader, mut writer) = tokio::io::split(client);
    let id = Uuid::new_v4().to_string();
    let line = encode_request(&id, cmd, params)?;
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    writer.flush().await.map_err(|error| error.to_string())?;

    let mut limited = BufReader::new(reader.take((MAX_IPC_LINE_BYTES + 1) as u64));
    let mut response = Vec::new();
    let read = limited
        .read_until(b'\n', &mut response)
        .await
        .map_err(|error| error.to_string())?;
    if read == 0 {
        return Err("插件没有返回响应。".to_string());
    }
    if response.len() > MAX_IPC_LINE_BYTES {
        return Err(format!("IPC 响应超过 {MAX_IPC_LINE_BYTES} 字节上限。"));
    }
    let response_line =
        String::from_utf8(response).map_err(|_| "插件响应不是有效 UTF-8。".to_string())?;
    decode_response_for(&response_line, Some(&id))
}
