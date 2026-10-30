mod catalog;
mod config;
mod core;
mod desktop;
mod host_update;
mod native_app;
mod plugins;
mod proxy;
mod registry;
mod single_instance;
mod thumbnails;
mod updates;

use std::{ffi::OsString, path::PathBuf};

const DIAGNOSTICS_USAGE: &str = "用法：--check-updates-json <outputPath> [--force-refresh] [--diagnostic-data-dir <dir> | --data-root <dir> --cache-dir <dir>]";

pub fn run() {
    // 诊断模式：`--check-updates-json <outputPath> [--force-refresh] [--data-root <dir>] [--cache-dir <dir>]`。
    // 走与「检查更新」相同的生产查更路径，但不取单实例锁、不启动 worker、
    // 不写 plugins.json / profile / 自启动。退出码：0=ok，3=degraded，1=failed，2=参数错误。
    let args: Vec<OsString> = std::env::args_os().collect();
    if args
        .iter()
        .any(|argument| argument == "--check-updates-json")
    {
        std::process::exit(run_diagnostics(&args));
    }
    if let Err(error) = native_app::run() {
        eprintln!("运行 Background Studio 失败：{error}");
    }
}

/// 取 `flag` 后紧跟的值；flag 不存在为 Ok(None)，缺值为 Err。
fn flag_value(args: &[OsString], flag: &str) -> Result<Option<PathBuf>, ()> {
    match args.iter().position(|argument| argument == flag) {
        None => Ok(None),
        Some(at) => args
            .get(at + 1)
            .filter(|value| !value.to_string_lossy().starts_with("--"))
            .map(|value| Some(PathBuf::from(value)))
            .ok_or(()),
    }
}

fn run_diagnostics(args: &[OsString]) -> i32 {
    let (Ok(Some(output)), Ok(data_root), Ok(cache_dir), Ok(isolated)) = (
        flag_value(args, "--check-updates-json"),
        flag_value(args, "--data-root"),
        flag_value(args, "--cache-dir"),
        flag_value(args, "--diagnostic-data-dir"),
    ) else {
        eprintln!("{DIAGNOSTICS_USAGE}");
        return 2;
    };
    let mut options = match isolated {
        Some(dir) => updates::DiagnosticsOptions::isolated(dir),
        None => updates::DiagnosticsOptions::default(),
    };
    options.force = args.iter().any(|argument| argument == "--force-refresh");
    if data_root.is_some() {
        options.data_root = data_root;
    }
    if cache_dir.is_some() {
        options.cache_dir = cache_dir;
    }
    match updates::run_check_updates_json_status(&output, &options) {
        Ok(status) if status == "ok" => 0,
        Ok(status) if status == "degraded" => 3,
        Ok(_) => 1,
        Err(error) => {
            eprintln!("写出诊断报告失败：{error}");
            1
        }
    }
}
