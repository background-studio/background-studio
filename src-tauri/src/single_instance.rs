use std::{
    io::Write,
    time::{Duration, Instant},
};

#[cfg(windows)]
use windows::{
    core::w,
    Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE},
        System::Threading::CreateMutexW,
    },
};

pub const ACTIVATION_PIPE: &str = r"\\.\pipe\background-studio-host-activate";

pub enum Instance {
    Primary(PrimaryInstance),
    Secondary,
}

pub struct PrimaryInstance {
    #[cfg(windows)]
    handle: HANDLE,
}

impl PrimaryInstance {
    pub fn acquire() -> Result<Instance, String> {
        #[cfg(windows)]
        {
            let handle = unsafe {
                CreateMutexW(None, true, w!("Local\\BackgroundStudioHost.SingleInstance"))
            }
            .map_err(|error| format!("创建单实例互斥量失败：{error}"))?;
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                let _ = unsafe { CloseHandle(handle) };
                return Ok(Instance::Secondary);
            }
            return Ok(Instance::Primary(Self { handle }));
        }
        #[cfg(not(windows))]
        {
            Ok(Instance::Primary(Self {}))
        }
    }
}

impl Drop for PrimaryInstance {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            let _ = unsafe { CloseHandle(self.handle) };
        }
    }
}

pub fn notify_primary() -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut last_error = None;
    while Instant::now() < deadline {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(ACTIVATION_PIPE)
        {
            Ok(mut pipe) => {
                pipe.write_all(b"activate\n")
                    .map_err(|error| error.to_string())?;
                pipe.flush().map_err(|error| error.to_string())?;
                return Ok(());
            }
            Err(error) => {
                last_error = Some(error);
                std::thread::sleep(Duration::from_millis(80));
            }
        }
    }
    Err(format!(
        "已有实例正在运行，但无法激活它：{}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "未知错误".to_string())
    ))
}

#[cfg(windows)]
pub async fn wait_for_activation() -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(ACTIVATION_PIPE)
        .or_else(|_| ServerOptions::new().create(ACTIVATION_PIPE))
        .map_err(|error| format!("创建实例激活管道失败：{error}"))?;
    server
        .connect()
        .await
        .map_err(|error| format!("等待实例激活失败：{error}"))?;
    let mut reader = BufReader::new(server);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|error| format!("读取实例激活请求失败：{error}"))?;
    if line.trim() == "activate" {
        Ok(())
    } else {
        Err("收到无效的实例激活请求。".to_string())
    }
}

#[cfg(not(windows))]
pub async fn wait_for_activation() -> Result<(), String> {
    std::future::pending::<()>().await;
    Ok(())
}
