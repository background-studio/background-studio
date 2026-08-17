use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

fn timeout_for(cmd: &str) -> std::time::Duration {
    match cmd {
        "status" => std::time::Duration::from_millis(800),
        "open-ui" | "quit-keep-target" => std::time::Duration::from_secs(5),
        "pause" => std::time::Duration::from_secs(15),
        "apply" | "restore" => std::time::Duration::from_secs(75),
        _ => std::time::Duration::from_secs(15),
    }
}

pub async fn request(pipe_name: &str, cmd: &str) -> Result<Value, String> {
    #[cfg(windows)]
    {
        tokio::time::timeout(timeout_for(cmd), request_windows(pipe_name, cmd))
            .await
            .map_err(|_| format!("插件命令 {cmd} 超时。"))?
    }
    #[cfg(not(windows))]
    {
        let _ = (pipe_name, cmd);
        Err("插件 IPC 仅支持 Windows。".to_string())
    }
}

#[cfg(windows)]
async fn request_windows(pipe_name: &str, cmd: &str) -> Result<Value, String> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let client = ClientOptions::new()
        .open(pipe_name)
        .map_err(|error| format!("无法连接插件管道（插件是否已启动？）：{error}"))?;
    let (reader, mut writer) = tokio::io::split(client);
    let id = Uuid::new_v4().to_string();
    let payload = json!({ "id": id, "cmd": cmd });
    let mut line = serde_json::to_string(&payload).map_err(|error| error.to_string())?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    writer.flush().await.map_err(|error| error.to_string())?;

    let mut lines = BufReader::new(reader).lines();
    let response_line = lines
        .next_line()
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "插件没有返回响应。".to_string())?;
    let value: Value = serde_json::from_str(&response_line).map_err(|error| error.to_string())?;
    if value.get("ok").and_then(|ok| ok.as_bool()) == Some(true) {
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(value
            .get("error")
            .and_then(|error| error.as_str())
            .unwrap_or("插件命令失败。")
            .to_string())
    }
}
