mod catalog;
mod config;
mod core;
mod desktop;
mod host_update;
mod native_app;
mod plugins;
mod proxy;
mod single_instance;
mod thumbnails;

pub fn run() {
    if let Err(error) = native_app::run() {
        eprintln!("运行 Background Studio 失败：{error}");
    }
}
