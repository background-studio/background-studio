use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    path::PathBuf,
    sync::{mpsc, Arc},
    time::Duration,
};

use eframe::egui::{
    self, Align, Color32, FontData, FontDefinitions, FontFamily, Frame, Layout, RichText, Stroke,
    Vec2,
};
use serde_json::{Map, Value};
use tokio::{runtime::Runtime, sync::Mutex};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use crate::{
    config,
    core::{
        HostCore, PluginDetail, ProfilePatch, SharedMediaKind, SlideshowOrder, SlideshowPatch,
        SlideshowSettings, CONSOLE_PROFILE_ID,
    },
    desktop,
    plugins::{ConsoleBackground, HostSnapshot, InstallProgress},
    proxy::{ProxyMode, ProxySettings},
    single_instance::{self, Instance, PrimaryInstance},
    thumbnails::{self, ThumbnailPixels, TEXTURE_CACHE_BYTES},
};

const CANVAS: Color32 = Color32::from_rgb(14, 20, 27);
const RAIL: Color32 = Color32::from_rgb(9, 14, 20);
const PAPER: Color32 = Color32::from_rgb(23, 31, 41);
const PAPER_RAISED: Color32 = Color32::from_rgb(29, 39, 51);
const INK: Color32 = Color32::from_rgb(235, 242, 247);
const MUTED: Color32 = Color32::from_rgb(143, 158, 171);
const LINE: Color32 = Color32::from_rgb(45, 58, 72);
const CYAN: Color32 = Color32::from_rgb(79, 209, 197);
const CYAN_DIM: Color32 = Color32::from_rgb(42, 106, 101);
const BRASS: Color32 = Color32::from_rgb(201, 168, 106);
const GREEN: Color32 = Color32::from_rgb(67, 190, 134);
const AMBER: Color32 = Color32::from_rgb(224, 164, 72);
const RED: Color32 = Color32::from_rgb(224, 92, 101);

#[derive(Clone)]
enum Command {
    Refresh,
    Install(String),
    Uninstall(String),
    SetEnabled(String, bool),
    PluginAction(String, String),
    LoadDetail(String),
    ImportFiles(String, Vec<PathBuf>),
    ImportFolder(String, PathBuf),
    ImportRemote(String, String, bool),
    RemoveMedia(String, String),
    SetActiveMedia(String, Option<String>),
    UpdateProfile(String, ProfilePatch),
    RefreshMedia(String, String),
    UpdateHostSettings(bool, bool),
    UpdateProxy(ProxySettings),
    RelocateDataDirectory(PathBuf),
    InstallHostUpdate,
}

enum UiEvent {
    Snapshot(HostSnapshot),
    Detail(String, PluginDetail),
    Progress(InstallProgress),
    Thumbnail(ThumbnailPixels),
    ThumbnailFailed(String),
    ShowWindow,
    Quit,
    Error(String),
    Idle,
}

pub fn run() -> Result<(), String> {
    let instance = match PrimaryInstance::acquire()? {
        Instance::Primary(instance) => instance,
        Instance::Secondary => {
            single_instance::notify_primary()?;
            return Ok(());
        }
    };
    let data_dir = config::resolve_data_directory()?;
    let mut core = HostCore::load(data_dir.clone())?;
    let auto_start = core.state().auto_start_with_windows;
    let start_minimized = core.state().start_minimized;
    desktop::sync_autostart(auto_start, start_minimized)?;
    core.start_enabled();

    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(4)
            .enable_all()
            .thread_name("background-studio")
            .build()
            .map_err(|error| error.to_string())?,
    );
    let core = Arc::new(Mutex::new(core));
    let viewport = egui::ViewportBuilder::default()
        .with_title("Background Studio")
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([960.0, 620.0])
        .with_icon(load_window_icon());
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Background Studio",
        options,
        Box::new(move |creation| {
            StudioApp::new(
                creation,
                Arc::clone(&core),
                Arc::clone(&runtime),
                start_minimized,
                data_dir,
                instance,
            )
            .map(|app| Box::new(app) as Box<dyn eframe::App>)
            .map_err(|error| std::io::Error::other(error).into())
        }),
    )
    .map_err(|error| error.to_string())
}

fn load_window_icon() -> Arc<egui::IconData> {
    let bytes = include_bytes!("../icons/128x128.png");
    match image::load_from_memory(bytes) {
        Ok(image) => {
            let rgba = image.into_rgba8();
            let (width, height) = rgba.dimensions();
            Arc::new(egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            })
        }
        Err(_) => Arc::new(egui::IconData::default()),
    }
}

struct StudioApp {
    core: Arc<Mutex<HostCore>>,
    runtime: Arc<Runtime>,
    events_tx: mpsc::Sender<UiEvent>,
    events_rx: mpsc::Receiver<UiEvent>,
    snapshot: Option<HostSnapshot>,
    selected_plugin: Option<String>,
    detail: Option<PluginDetail>,
    display_draft: Value,
    remote_url: String,
    remote_dynamic: bool,
    search: String,
    busy_count: usize,
    progress: Option<InstallProgress>,
    notice: Option<(String, bool)>,
    thumbnail_cache: lru::LruCache<String, CachedThumbnail>,
    thumbnail_loading: HashSet<String>,
    thumbnail_bytes: usize,
    thumbnail_directory: PathBuf,
    tray: TrayUi,
    logo: Option<egui::TextureHandle>,
    plugin_icons: HashMap<String, Option<egui::TextureHandle>>,
    console_bg_texture: Option<(String, egui::TextureHandle)>,
    console_bg_active: bool,
    proxy_draft: Option<ProxySettings>,
    host_draft: Option<(bool, bool)>,
    enabled_draft: HashMap<String, bool>,
    slideshow_draft: Option<(String, SlideshowSettings)>,
    _instance: PrimaryInstance,
    quitting: bool,
}

struct CachedThumbnail {
    texture: egui::TextureHandle,
    bytes: usize,
}

struct TrayUi {
    icon: TrayIcon,
    status: MenuItem,
}

impl StudioApp {
    fn new(
        creation: &eframe::CreationContext<'_>,
        core: Arc<Mutex<HostCore>>,
        runtime: Arc<Runtime>,
        start_minimized: bool,
        data_dir: PathBuf,
        instance: PrimaryInstance,
    ) -> Result<Self, String> {
        configure_fonts(&creation.egui_ctx);
        configure_style(&creation.egui_ctx);
        if start_minimized || std::env::args().any(|argument| argument == "--hidden") {
            creation
                .egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        let (events_tx, events_rx) = mpsc::channel();
        let tray = setup_tray(events_tx.clone(), creation.egui_ctx.clone())?;
        start_snapshot_worker(
            Arc::clone(&core),
            Arc::clone(&runtime),
            events_tx.clone(),
            creation.egui_ctx.clone(),
        );
        start_activation_worker(
            Arc::clone(&runtime),
            events_tx.clone(),
            creation.egui_ctx.clone(),
        );
        let mut app = Self {
            core,
            runtime,
            events_tx,
            events_rx,
            snapshot: None,
            selected_plugin: None,
            detail: None,
            display_draft: Value::Object(Map::new()),
            remote_url: String::new(),
            remote_dynamic: false,
            search: String::new(),
            busy_count: 0,
            progress: None,
            notice: None,
            thumbnail_cache: lru::LruCache::new(
                NonZeroUsize::new(512).expect("thumbnail cache capacity"),
            ),
            thumbnail_loading: HashSet::new(),
            thumbnail_bytes: 0,
            thumbnail_directory: thumbnails::cache_directory(&data_dir),
            tray,
            logo: load_sidebar_logo(&creation.egui_ctx),
            plugin_icons: HashMap::new(),
            console_bg_texture: None,
            console_bg_active: false,
            proxy_draft: None,
            host_draft: None,
            enabled_draft: HashMap::new(),
            slideshow_draft: None,
            _instance: instance,
            quitting: false,
        };
        app.dispatch(Command::Refresh);
        Ok(app)
    }

    fn dispatch(&mut self, command: Command) {
        self.busy_count = self.busy_count.saturating_add(1);
        let core = Arc::clone(&self.core);
        let tx = self.events_tx.clone();
        self.runtime.spawn(async move {
            let result = execute_command(&core, &tx, command).await;
            if let Err(error) = result {
                let _ = tx.send(UiEvent::Error(error));
            }
            let snapshot = {
                let mut core = core.lock().await;
                core.snapshot().await
            };
            let _ = tx.send(UiEvent::Snapshot(snapshot));
            let _ = tx.send(UiEvent::Idle);
        });
    }

    fn drain_events(&mut self, context: &egui::Context) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                UiEvent::Snapshot(snapshot) => {
                    let installed = snapshot
                        .plugins
                        .iter()
                        .filter(|plugin| plugin.installed_version.is_some())
                        .count();
                    let running = snapshot
                        .plugins
                        .iter()
                        .filter(|plugin| plugin.running)
                        .count();
                    let summary = format!("{installed} 已装 / {running} 运行中");
                    self.tray.status.set_text(format!("状态：{summary}"));
                    let _ = self
                        .tray
                        .icon
                        .set_tooltip(Some(format!("Background Studio · {summary}")));
                    let thumbnail_directory =
                        thumbnails::cache_directory(std::path::Path::new(&snapshot.data_directory));
                    if thumbnail_directory != self.thumbnail_directory {
                        self.thumbnail_directory = thumbnail_directory;
                        self.thumbnail_cache.clear();
                        self.thumbnail_loading.clear();
                        self.thumbnail_bytes = 0;
                    }
                    if self.proxy_draft.as_ref().is_some_and(|draft| {
                        draft.mode == snapshot.proxy_mode && draft.url == snapshot.proxy_url
                    }) {
                        self.proxy_draft = None;
                    }
                    if self.host_draft
                        == Some((snapshot.auto_start_with_windows, snapshot.start_minimized))
                    {
                        self.host_draft = None;
                    }
                    self.enabled_draft.retain(|id, enabled| {
                        snapshot
                            .plugins
                            .iter()
                            .find(|plugin| plugin.id == *id)
                            .is_some_and(|plugin| plugin.enabled != *enabled)
                    });
                    self.snapshot = Some(snapshot);
                }
                UiEvent::Detail(id, detail) => {
                    if self.selected_plugin.as_deref() == Some(id.as_str()) {
                        let previous_saved = self
                            .detail
                            .as_ref()
                            .map(|item| item.profile.display.clone());
                        let draft_dirty = previous_saved
                            .as_ref()
                            .is_some_and(|saved| *saved != self.display_draft);
                        if !draft_dirty {
                            self.display_draft = detail.profile.display.clone();
                        }
                        if self
                            .slideshow_draft
                            .as_ref()
                            .is_some_and(|(plugin, settings)| {
                                plugin == &id && *settings == detail.profile.slideshow
                            })
                        {
                            self.slideshow_draft = None;
                        }
                        self.detail = Some(detail);
                    }
                }
                UiEvent::Progress(progress) => self.progress = Some(progress),
                UiEvent::Thumbnail(pixels) => {
                    self.thumbnail_loading.remove(&pixels.key);
                    let bytes = pixels.rgba.len();
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [pixels.width, pixels.height],
                        &pixels.rgba,
                    );
                    let texture = context.load_texture(
                        format!("thumbnail-{}", pixels.key),
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    if let Some(previous) = self
                        .thumbnail_cache
                        .put(pixels.key, CachedThumbnail { texture, bytes })
                    {
                        self.thumbnail_bytes = self.thumbnail_bytes.saturating_sub(previous.bytes);
                    }
                    self.thumbnail_bytes = self.thumbnail_bytes.saturating_add(bytes);
                    while self.thumbnail_bytes > TEXTURE_CACHE_BYTES {
                        let Some((_key, evicted)) = self.thumbnail_cache.pop_lru() else {
                            break;
                        };
                        self.thumbnail_bytes = self.thumbnail_bytes.saturating_sub(evicted.bytes);
                    }
                }
                UiEvent::ThumbnailFailed(key) => {
                    self.thumbnail_loading.remove(&key);
                }
                UiEvent::ShowWindow => {
                    context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    context.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                UiEvent::Quit => {
                    self.quitting = true;
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                UiEvent::Error(error) => self.notice = Some((error, true)),
                UiEvent::Idle => {
                    self.busy_count = self.busy_count.saturating_sub(1);
                    self.progress = None;
                }
            }
            context.request_repaint();
        }
    }

    fn open_plugin(&mut self, id: String) {
        if self.selected_plugin.as_deref() != Some(id.as_str()) {
            self.slideshow_draft = None;
        }
        self.selected_plugin = Some(id.clone());
        self.detail = None;
        self.dispatch(Command::LoadDetail(id));
    }

    fn render_thumbnail(
        &mut self,
        ui: &mut egui::Ui,
        source: Option<&crate::core::ThumbnailSource>,
        fallback: &str,
    ) {
        let size = Vec2::new(86.0, 58.0);
        let Some(source) = source else {
            thumbnail_placeholder(ui, size, fallback);
            return;
        };
        let key = thumbnails::cache_key(source);
        if let Some(cached) = self.thumbnail_cache.get(&key) {
            ui.add(
                egui::Image::new(&cached.texture)
                    .fit_to_exact_size(size)
                    .corner_radius(9.0),
            );
            return;
        }
        thumbnail_placeholder(ui, size, fallback);
        if self.thumbnail_loading.insert(key.clone()) {
            let source = source.clone();
            let directory = self.thumbnail_directory.clone();
            let tx = self.events_tx.clone();
            let context = ui.ctx().clone();
            std::thread::spawn(move || {
                let event = match thumbnails::generate(&source, &directory) {
                    Ok(pixels) => UiEvent::Thumbnail(pixels),
                    Err(_) => UiEvent::ThumbnailFailed(key),
                };
                let _ = tx.send(event);
                context.request_repaint();
            });
        }
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("navigation")
            .exact_size(272.0)
            .frame(
                Frame::new()
                    .fill(self.rail_fill())
                    .stroke(Stroke::new(1.0, LINE))
                    .inner_margin(egui::Margin::symmetric(18, 22)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(logo) = self.logo.clone() {
                        ui.add(
                            egui::Image::new(&logo)
                                .fit_to_exact_size(Vec2::splat(44.0))
                                .corner_radius(12.0),
                        );
                    } else {
                        let (rect, _) =
                            ui.allocate_exact_size(Vec2::splat(44.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 12.0, PAPER_RAISED);
                    }
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Background Studio")
                                .size(17.0)
                                .strong()
                                .color(INK),
                        );
                        ui.label(RichText::new("统一背景控制台").size(11.0).color(MUTED));
                    });
                });
                ui.add_space(28.0);
                let overview_selected = self.selected_plugin.is_none();
                if navigation_button(ui, "控制台", None, overview_selected).clicked() {
                    self.selected_plugin = None;
                    self.detail = None;
                }
                let console_selected =
                    self.selected_plugin.as_deref() == Some(CONSOLE_PROFILE_ID);
                if navigation_button(ui, "控制台背景", None, console_selected).clicked() {
                    self.open_plugin(CONSOLE_PROFILE_ID.to_string());
                }
                ui.add_space(22.0);
                ui.label(
                    RichText::new("插件")
                        .size(11.0)
                        .strong()
                        .color(Color32::from_rgb(112, 128, 142)),
                );
                ui.add_space(8.0);
                let plugins = self
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.plugins.clone())
                    .unwrap_or_default();
                for plugin in plugins {
                    let selected = self.selected_plugin.as_deref() == Some(plugin.id.as_str());
                    let label = format!(
                        "{}  {}",
                        phase_dot(&plugin.phase, plugin.running),
                        plugin.display_name
                    );
                    let version = plugin_version_label(&plugin);
                    if navigation_button(ui, &label, Some(&version), selected).clicked() {
                        self.open_plugin(plugin.id);
                    }
                }
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.add_space(18.0);
                    let version = self
                        .snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.host_version.as_str())
                        .unwrap_or("加载中");
                    ui.label(
                        RichText::new(format!("Native host · {version}"))
                            .size(11.0)
                            .color(Color32::from_rgb(102, 118, 132)),
                    );
                });
            });
    }

    fn render_overview(&mut self, ui: &mut egui::Ui) {
        page_heading(ui, "控制台", "插件、媒体与接管状态都在这里。");
        if let Some(snapshot) = self.snapshot.clone() {
            if let Some(warning) = snapshot.warning.as_deref() {
                notice_frame(ui, warning, AMBER);
                ui.add_space(12.0);
            }
            self.render_takeover_bus(ui, &snapshot);
            ui.add_space(16.0);
            let metric_fill = self.paper_fill();
            ui.horizontal_wrapped(|ui| {
                metric(
                    ui,
                    "已安装",
                    snapshot
                        .plugins
                        .iter()
                        .filter(|plugin| plugin.installed_version.is_some())
                        .count(),
                    metric_fill,
                );
                metric(
                    ui,
                    "运行中",
                    snapshot
                        .plugins
                        .iter()
                        .filter(|plugin| plugin.running)
                        .count(),
                    metric_fill,
                );
                metric(
                    ui,
                    "需要更新",
                    snapshot
                        .plugins
                        .iter()
                        .filter(|plugin| plugin.update_available)
                        .count(),
                    metric_fill,
                );
            });
            ui.add_space(18.0);
            for plugin in snapshot.plugins {
                self.render_plugin_card(ui, plugin);
                ui.add_space(12.0);
            }
            ui.add_space(16.0);
            self.render_host_settings(ui);
        } else {
            ui.spinner();
            ui.label(RichText::new("正在加载宿主状态…").color(MUTED));
        }
    }

    /// 当前生效的控制台背景：快照为准；正在控制台背景页编辑强度时用草稿实时预览。
    fn console_background_settings(&self) -> Option<ConsoleBackground> {
        let mut view = self.snapshot.as_ref()?.console_background.clone();
        if self.selected_plugin.as_deref() == Some(CONSOLE_PROFILE_ID) {
            if let Some(intensity) = self.display_draft.get("intensity").and_then(Value::as_f64) {
                view.intensity = intensity as f32;
            }
        }
        Some(view)
    }

    /// 在所有面板之前把背景图铺满窗口；面板改用半透明填充让它透出来。
    fn paint_console_background(&mut self, ui: &egui::Ui) {
        self.console_bg_active = false;
        let Some(settings) = self.console_background_settings() else {
            return;
        };
        let Some(path) = settings.path.filter(|path| !path.is_empty()) else {
            self.console_bg_texture = None;
            return;
        };
        let stale = self
            .console_bg_texture
            .as_ref()
            .is_none_or(|(cached, _)| cached != &path);
        if stale {
            let texture = image::open(&path).ok().map(|source| {
                let rgba = source.thumbnail(2048, 2048).into_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                ui.ctx()
                    .load_texture("console-background", color, egui::TextureOptions::LINEAR)
            });
            match texture {
                Some(texture) => self.console_bg_texture = Some((path, texture)),
                None => {
                    self.console_bg_texture = None;
                    return;
                }
            }
        }
        let Some((_, texture)) = &self.console_bg_texture else {
            return;
        };
        let alpha = (settings.intensity.clamp(0.0, 1.0) * 255.0).round() as u8;
        if alpha == 0 {
            return;
        }
        // 此时面板尚未布局，根 ui 的范围即整个窗口。
        let screen = ui.max_rect();
        let size = texture.size_vec2();
        if size.x <= 0.0 || size.y <= 0.0 {
            return;
        }
        // cover 填充：等比放大到盖满整个窗口。
        let scale = (screen.width() / size.x).max(screen.height() / size.y);
        let rect = egui::Rect::from_center_size(screen.center(), size * scale);
        ui.painter().image(
            texture.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::from_rgba_unmultiplied(255, 255, 255, alpha),
        );
        self.console_bg_active = true;
    }

    fn rail_fill(&self) -> Color32 {
        if self.console_bg_active {
            glass(RAIL, 216)
        } else {
            RAIL
        }
    }

    fn canvas_fill(&self) -> Color32 {
        if self.console_bg_active {
            glass(CANVAS, 176)
        } else {
            CANVAS
        }
    }

    fn paper_fill(&self) -> Color32 {
        if self.console_bg_active {
            glass(PAPER, 212)
        } else {
            PAPER
        }
    }

    /// 按 icon_path 懒加载插件 logo 纹理；加载失败也缓存，避免每帧重试。
    fn plugin_icon_texture(
        &mut self,
        context: &egui::Context,
        plugin: &crate::plugins::PluginCard,
    ) -> Option<egui::TextureHandle> {
        let path = plugin.icon_path.as_deref()?;
        if let Some(cached) = self.plugin_icons.get(path) {
            return cached.clone();
        }
        let texture = image::open(path).ok().map(|source| {
            let rgba = source.thumbnail(96, 96).into_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            context.load_texture(
                format!("plugin-icon-{}", plugin.id),
                color,
                egui::TextureOptions::LINEAR,
            )
        });
        self.plugin_icons.insert(path.to_string(), texture.clone());
        texture
    }

    fn render_takeover_bus(&mut self, ui: &mut egui::Ui, snapshot: &HostSnapshot) {
        Frame::new()
            .fill(self.paper_fill())
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(16.0)
            .inner_margin(egui::Margin::symmetric(20, 16))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("接管总线").size(12.0).strong().color(BRASS));
                        ui.label(
                            RichText::new("壳 → worker → 目标程序")
                                .size(11.0)
                                .color(MUTED),
                        );
                    });
                    ui.add_space(18.0);
                    let available = ui.available_width();
                    let count = snapshot.plugins.len().max(1) as f32;
                    let node_width = ((available - 24.0 * (count - 1.0)) / count).max(120.0);
                    for (index, plugin) in snapshot.plugins.iter().enumerate() {
                        let color = phase_color(&plugin.phase, plugin.running);
                        let icon = self.plugin_icon_texture(ui.ctx(), plugin);
                        let (rect, response) = ui
                            .allocate_exact_size(Vec2::new(node_width, 58.0), egui::Sense::click());
                        let fill = if self.selected_plugin.as_deref() == Some(plugin.id.as_str()) {
                            PAPER_RAISED
                        } else {
                            Color32::from_rgb(19, 27, 36)
                        };
                        ui.painter().rect_filled(rect, 12.0, fill);
                        ui.painter().rect_stroke(
                            rect,
                            12.0,
                            Stroke::new(1.0, color.gamma_multiply(0.55)),
                            egui::StrokeKind::Inside,
                        );
                        let text_x = if let Some(icon) = &icon {
                            let icon_rect = egui::Rect::from_center_size(
                                rect.left_center() + egui::vec2(22.0, 0.0),
                                Vec2::splat(28.0),
                            );
                            paint_icon(ui, icon, icon_rect);
                            let dot = icon_rect.right_bottom() + egui::vec2(-3.0, -3.0);
                            ui.painter().circle_stroke(dot, 5.0, Stroke::new(2.0, fill));
                            ui.painter().circle_filled(dot, 4.0, color);
                            44.0
                        } else {
                            let center = rect.left_center() + egui::vec2(18.0, 0.0);
                            ui.painter().circle_filled(center, 6.0, color);
                            ui.painter().circle_stroke(
                                center,
                                10.0,
                                Stroke::new(1.0, color.gamma_multiply(0.35)),
                            );
                            36.0
                        };
                        ui.painter().text(
                            rect.left_center() + egui::vec2(text_x, -10.0),
                            egui::Align2::LEFT_CENTER,
                            &plugin.display_name,
                            egui::FontId::proportional(13.0),
                            INK,
                        );
                        let status = if plugin.running {
                            "worker 在线"
                        } else if plugin.installed_version.is_some() {
                            "已安装"
                        } else {
                            "待安装"
                        };
                        ui.painter().text(
                            rect.left_center() + egui::vec2(text_x, 10.0),
                            egui::Align2::LEFT_CENTER,
                            format!("{status} · {}", plugin_version_label(plugin)),
                            egui::FontId::proportional(10.0),
                            MUTED,
                        );
                        if response.clicked() {
                            self.open_plugin(plugin.id.clone());
                        }
                        if index + 1 < snapshot.plugins.len() {
                            let start = rect.right_center() + egui::vec2(4.0, 0.0);
                            let end = start + egui::vec2(16.0, 0.0);
                            ui.painter().line_segment(
                                [start, end],
                                Stroke::new(1.5, CYAN.gamma_multiply(0.7)),
                            );
                        }
                    }
                });
            });
    }

    fn render_plugin_card(&mut self, ui: &mut egui::Ui, plugin: crate::plugins::PluginCard) {
        let icon = self.plugin_icon_texture(ui.ctx(), &plugin);
        Frame::new()
            .fill(self.paper_fill())
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(16.0)
            .inner_margin(egui::Margin::symmetric(20, 16))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let color = phase_color(&plugin.phase, plugin.running);
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(46.0), egui::Sense::hover());
                    if let Some(icon) = &icon {
                        paint_icon(ui, icon, rect);
                        ui.painter().rect_stroke(
                            rect,
                            14.0,
                            Stroke::new(1.0, color.gamma_multiply(0.35)),
                            egui::StrokeKind::Inside,
                        );
                        let dot = rect.right_bottom() + egui::vec2(-4.0, -4.0);
                        ui.painter()
                            .circle_stroke(dot, 6.0, Stroke::new(2.0, PAPER));
                        ui.painter().circle_filled(dot, 5.0, color);
                    } else {
                        ui.painter()
                            .rect_filled(rect, 14.0, color.gamma_multiply(0.12));
                        ui.painter().rect_stroke(
                            rect,
                            14.0,
                            Stroke::new(1.0, color.gamma_multiply(0.35)),
                            egui::StrokeKind::Inside,
                        );
                        ui.painter().circle_filled(rect.center(), 7.0, color);
                    }
                    ui.add_space(14.0);
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(&plugin.display_name)
                                    .size(17.0)
                                    .strong()
                                    .color(INK),
                            );
                            ui.label(
                                RichText::new(plugin_version_label(&plugin))
                                    .size(12.0)
                                    .color(if plugin.update_available {
                                        AMBER
                                    } else {
                                        BRASS
                                    }),
                            );
                        });
                        ui.label(RichText::new(&plugin.target_hint).size(12.0).color(MUTED));
                        ui.label(
                            RichText::new(&plugin.status_message)
                                .size(12.0)
                                .color(color),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("详情").clicked() {
                            self.open_plugin(plugin.id.clone());
                        }
                        if plugin.installed_version.is_none() {
                            if ui
                                .add_enabled(
                                    self.busy_count == 0,
                                    egui::Button::new(RichText::new("安装").color(INK))
                                        .fill(CYAN_DIM),
                                )
                                .clicked()
                            {
                                self.dispatch(Command::Install(plugin.id.clone()));
                            }
                        } else {
                            let mut enabled = self
                                .enabled_draft
                                .get(&plugin.id)
                                .copied()
                                .unwrap_or(plugin.enabled);
                            if ui
                                .add_enabled(
                                    self.busy_count == 0,
                                    egui::Checkbox::new(&mut enabled, "启用"),
                                )
                                .changed()
                            {
                                self.enabled_draft.insert(plugin.id.clone(), enabled);
                                self.dispatch(Command::SetEnabled(plugin.id.clone(), enabled));
                            }
                            if plugin.update_available
                                && ui
                                    .add_enabled(
                                        self.busy_count == 0,
                                        egui::Button::new(RichText::new("更新").color(INK))
                                            .fill(CYAN_DIM),
                                    )
                                    .clicked()
                            {
                                self.dispatch(Command::Install(plugin.id.clone()));
                            }
                        }
                    });
                });
            });
    }

    fn render_host_settings(&mut self, ui: &mut egui::Ui) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        Frame::new()
            .fill(self.paper_fill())
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(16.0)
            .inner_margin(egui::Margin::symmetric(20, 16))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("宿主设置").size(16.0).strong().color(INK));
                        ui.label(
                            RichText::new("启动、数据目录、代理与壳更新")
                                .size(11.0)
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if snapshot.host_update_available {
                            let version =
                                snapshot.host_latest_version.as_deref().unwrap_or("最新版");
                            if ui
                                .add_enabled(
                                    self.busy_count == 0,
                                    egui::Button::new(
                                        RichText::new(format!("更新到 {version}")).color(INK),
                                    )
                                    .fill(CYAN_DIM),
                                )
                                .clicked()
                            {
                                self.dispatch(Command::InstallHostUpdate);
                            }
                        }
                        if ui
                            .add_enabled(self.busy_count == 0, egui::Button::new("检查更新"))
                            .clicked()
                        {
                            self.dispatch(Command::Refresh);
                        }
                    });
                });
                ui.add_space(10.0);
                let (mut auto_start, mut minimized) = self
                    .host_draft
                    .unwrap_or((snapshot.auto_start_with_windows, snapshot.start_minimized));
                let auto_changed = ui.checkbox(&mut auto_start, "随 Windows 启动").changed();
                let minimized_changed = ui.checkbox(&mut minimized, "启动后最小化").changed();
                if auto_changed || minimized_changed {
                    self.host_draft = Some((auto_start, minimized));
                    self.dispatch(Command::UpdateHostSettings(auto_start, minimized));
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("控制台背景").color(MUTED));
                    let hint = if snapshot.console_background.path.is_some() {
                        "已启用"
                    } else {
                        "未设置"
                    };
                    ui.label(RichText::new(hint).size(11.0).color(MUTED));
                    if ui.button("管理媒体与轮播").clicked() {
                        self.open_plugin(CONSOLE_PROFILE_ID.to_string());
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("数据目录").color(MUTED));
                    ui.add(
                        egui::Label::new(
                            RichText::new(&snapshot.data_directory)
                                .monospace()
                                .color(INK),
                        )
                        .truncate(),
                    );
                    if ui.button("打开").clicked() {
                        match desktop::open_data_directory(std::path::Path::new(
                            &snapshot.data_directory,
                        )) {
                            Ok(()) => {}
                            Err(error) => self.notice = Some((error, true)),
                        }
                    }
                    if ui.button("迁移").clicked() {
                        if let Some(folder) = rfd::FileDialog::new()
                            .set_directory(&snapshot.data_directory)
                            .pick_folder()
                        {
                            self.dispatch(Command::RelocateDataDirectory(folder));
                        }
                    }
                });
                ui.separator();
                let mut settings = self.proxy_draft.clone().unwrap_or(ProxySettings {
                    mode: snapshot.proxy_mode.clone(),
                    url: snapshot.proxy_url.clone(),
                });
                let before = settings.clone();
                let mut url_committed = false;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("代理").color(MUTED));
                    proxy_mode_chip(ui, &mut settings.mode, ProxyMode::Off, "关闭");
                    proxy_mode_chip(ui, &mut settings.mode, ProxyMode::System, "系统");
                    proxy_mode_chip(ui, &mut settings.mode, ProxyMode::Custom, "自定义");
                });
                if settings.mode == ProxyMode::Custom {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("地址").color(MUTED));
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut settings.url)
                                .desired_width(280.0)
                                .hint_text("http://127.0.0.1:7890"),
                        );
                        url_committed = edit.lost_focus();
                    });
                }
                if settings != before {
                    self.proxy_draft = Some(settings.clone());
                }
                if settings.mode != before.mode || (url_committed && settings.url != before.url) {
                    self.dispatch(Command::UpdateProxy(settings));
                }
            });
    }

    fn render_detail(&mut self, ui: &mut egui::Ui, plugin_id: &str) {
        let Some(detail) = self.detail.clone() else {
            page_heading(ui, "插件详情", "正在读取媒体与设置…");
            ui.spinner();
            return;
        };
        if detail.plugin.id != plugin_id {
            return;
        }
        // 控制台背景页：没有 worker，只有媒体库 + 显示/轮播设置。
        if plugin_id == CONSOLE_PROFILE_ID {
            page_heading(
                ui,
                "控制台背景",
                &format!(
                    "壳自身的背景 · 与插件共用媒体库与轮播 · {}",
                    detail.plugin.status_message
                ),
            );
            ui.columns(2, |columns| {
                columns[0].set_min_width(460.0);
                self.render_media_library(&mut columns[0], plugin_id, &detail);
                self.render_display_settings(&mut columns[1], plugin_id, &detail);
            });
            return;
        }
        let icon = self.plugin_icon_texture(ui.ctx(), &detail.plugin);
        let subtitle = format!(
            "{} · {} · 协议 {} · {}",
            detail.plugin.target_hint,
            plugin_version_label(&detail.plugin),
            detail.plugin_protocol,
            detail.plugin.status_message
        );
        if let Some(icon) = &icon {
            ui.horizontal(|ui| {
                ui.add(
                    egui::Image::new(icon)
                        .fit_to_exact_size(Vec2::splat(52.0))
                        .corner_radius(14.0),
                );
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&detail.plugin.display_name)
                            .size(30.0)
                            .strong()
                            .color(INK),
                    );
                    ui.add_space(2.0);
                    ui.label(RichText::new(&subtitle).size(13.0).color(MUTED));
                });
            });
            ui.add_space(20.0);
        } else {
            page_heading(ui, &detail.plugin.display_name, &subtitle);
        }
        Frame::new()
            .fill(self.paper_fill())
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(16.0)
            .inner_margin(egui::Margin::symmetric(18, 14))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            detail.plugin.enabled && self.busy_count == 0,
                            egui::Button::new(RichText::new("应用背景").color(INK)).fill(CYAN_DIM),
                        )
                        .clicked()
                    {
                        self.dispatch(Command::PluginAction(
                            plugin_id.to_string(),
                            "apply".to_string(),
                        ));
                    }
                    if ui
                        .add_enabled(
                            detail.plugin.running && self.busy_count == 0,
                            egui::Button::new("暂停"),
                        )
                        .clicked()
                    {
                        self.dispatch(Command::PluginAction(
                            plugin_id.to_string(),
                            "pause".to_string(),
                        ));
                    }
                    if ui
                        .add_enabled(
                            detail.plugin.running && self.busy_count == 0,
                            egui::Button::new("恢复官方外观"),
                        )
                        .clicked()
                    {
                        self.dispatch(Command::PluginAction(
                            plugin_id.to_string(),
                            "restore".to_string(),
                        ));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} 个媒体 · {} 个轮播项",
                                detail.library.len(),
                                detail.profile.playlist_ids.len()
                            ))
                            .color(MUTED),
                        );
                    });
                });
            });
        ui.add_space(16.0);

        ui.columns(2, |columns| {
            columns[0].set_min_width(460.0);
            self.render_media_library(&mut columns[0], plugin_id, &detail);
            self.render_display_settings(&mut columns[1], plugin_id, &detail);
        });
    }

    fn render_media_library(&mut self, ui: &mut egui::Ui, plugin_id: &str, detail: &PluginDetail) {
        Frame::new()
            .fill(self.paper_fill())
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(16.0)
            .inner_margin(egui::Margin::symmetric(18, 16))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("共享媒体库").size(16.0).strong().color(INK));
                        ui.label(
                            RichText::new("静态缩略图只在壳里预览，动态播放仍交给目标程序")
                                .size(11.0)
                                .color(MUTED),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.search)
                                .hint_text("搜索")
                                .desired_width(150.0),
                        );
                    });
                });
                ui.add_space(10.0);
                let dropped: Vec<PathBuf> = ui.ctx().input(|input| {
                    input
                        .raw
                        .dropped_files
                        .iter()
                        .map(|file| file.path().to_path_buf())
                        .collect()
                });
                if !dropped.is_empty() {
                    self.dispatch(Command::ImportFiles(plugin_id.to_string(), dropped));
                }
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("选择文件").color(INK)).fill(CYAN_DIM))
                        .clicked()
                    {
                        if let Some(paths) = rfd::FileDialog::new()
                            .add_filter(
                                "媒体",
                                &["png", "jpg", "jpeg", "webp", "gif", "mp4", "webm"],
                            )
                            .pick_files()
                        {
                            self.dispatch(Command::ImportFiles(plugin_id.to_string(), paths));
                        }
                    }
                    if ui.button("选择文件夹").clicked() {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            self.dispatch(Command::ImportFolder(plugin_id.to_string(), folder));
                        }
                    }
                    ui.label(RichText::new("也可以直接拖入窗口").size(11.0).color(MUTED));
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.remote_url)
                            .hint_text("HTTPS 图片或视频 URL")
                            .desired_width(250.0),
                    );
                    ui.checkbox(&mut self.remote_dynamic, "随机 API");
                    if ui.button("导入 URL").clicked() && !self.remote_url.trim().is_empty() {
                        self.dispatch(Command::ImportRemote(
                            plugin_id.to_string(),
                            self.remote_url.trim().to_string(),
                            self.remote_dynamic,
                        ));
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("media-library")
                    .max_height(430.0)
                    .show(ui, |ui| {
                        let query = self.search.trim().to_ascii_lowercase();
                        for item in detail.library.iter().filter(|item| {
                            query.is_empty() || item.name.to_ascii_lowercase().contains(&query)
                        }) {
                            let thumbnail_source = detail
                                .thumbnail_sources
                                .iter()
                                .find(|source| source.media_id == item.id)
                                .cloned();
                            let active =
                                detail.profile.active_media_id.as_deref() == Some(item.id.as_str());
                            let in_playlist =
                                detail.profile.playlist_ids.iter().any(|id| id == &item.id);
                            Frame::new()
                                .fill(if active {
                                    CYAN_DIM.gamma_multiply(0.28)
                                } else {
                                    Color32::from_rgb(19, 27, 36)
                                })
                                .stroke(Stroke::new(1.0, if active { CYAN } else { LINE }))
                                .corner_radius(12.0)
                                .inner_margin(10.0)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let kind = match item.kind {
                                            SharedMediaKind::Image => "IMG",
                                            SharedMediaKind::Video => "VID",
                                        };
                                        self.render_thumbnail(ui, thumbnail_source.as_ref(), kind);
                                        ui.vertical(|ui| {
                                            ui.label(RichText::new(&item.name).strong().color(INK));
                                            ui.label(
                                                RichText::new(format_bytes(item.byte_size))
                                                    .size(11.0)
                                                    .color(MUTED),
                                            );
                                        });
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if ui.small_button("删除").clicked() {
                                                    self.dispatch(Command::RemoveMedia(
                                                        plugin_id.to_string(),
                                                        item.id.clone(),
                                                    ));
                                                }
                                                if item.source_url.is_some()
                                                    && ui.small_button("刷新").clicked()
                                                {
                                                    self.dispatch(Command::RefreshMedia(
                                                        plugin_id.to_string(),
                                                        item.id.clone(),
                                                    ));
                                                }
                                                if ui
                                                    .small_button(if in_playlist {
                                                        "移出轮播"
                                                    } else {
                                                        "加入轮播"
                                                    })
                                                    .clicked()
                                                {
                                                    let mut playlist =
                                                        detail.profile.playlist_ids.clone();
                                                    if in_playlist {
                                                        playlist.retain(|id| id != &item.id);
                                                    } else {
                                                        playlist.push(item.id.clone());
                                                    }
                                                    self.dispatch(Command::UpdateProfile(
                                                        plugin_id.to_string(),
                                                        ProfilePatch {
                                                            playlist_ids: Some(playlist),
                                                            ..ProfilePatch::default()
                                                        },
                                                    ));
                                                }
                                                let active_button = if active {
                                                    egui::Button::new(
                                                        RichText::new("已选").color(INK),
                                                    )
                                                    .fill(CYAN_DIM)
                                                } else {
                                                    egui::Button::new("设为背景")
                                                };
                                                if ui.add_enabled(!active, active_button).clicked()
                                                {
                                                    self.dispatch(Command::SetActiveMedia(
                                                        plugin_id.to_string(),
                                                        Some(item.id.clone()),
                                                    ));
                                                }
                                            },
                                        );
                                    });
                                });
                            ui.add_space(3.0);
                        }
                    });
            });
    }

    fn render_display_settings(
        &mut self,
        ui: &mut egui::Ui,
        plugin_id: &str,
        detail: &PluginDetail,
    ) {
        Frame::new()
            .fill(self.paper_fill())
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(16.0)
            .inner_margin(egui::Margin::symmetric(18, 16))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("显示设置").size(16.0).strong().color(INK));
                    let hint = if plugin_id == CONSOLE_PROFILE_ID {
                        "背景强度拖动时实时预览，保存后长期生效"
                    } else {
                        "由插件 Manifest 生成，保存后通过协议 2 热更新"
                    };
                    ui.label(RichText::new(hint).size(11.0).color(MUTED));
                });
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .id_salt("settings")
                    .max_height(470.0)
                    .show(ui, |ui| {
                        render_schema_fields(
                            ui,
                            detail
                                .settings_schema
                                .get("properties")
                                .and_then(Value::as_object),
                            &mut self.display_draft,
                        );
                    });
                ui.add_space(8.0);
                if ui
                    .add_enabled(
                        self.display_draft != detail.profile.display && self.busy_count == 0,
                        egui::Button::new(RichText::new("保存并热更新").color(INK)).fill(CYAN_DIM),
                    )
                    .clicked()
                {
                    self.dispatch(Command::UpdateProfile(
                        plugin_id.to_string(),
                        ProfilePatch {
                            display: Some(self.display_draft.clone()),
                            ..ProfilePatch::default()
                        },
                    ));
                }
                ui.separator();
                ui.label(RichText::new("轮播").strong().color(INK));
                let mut slideshow = self
                    .slideshow_draft
                    .as_ref()
                    .filter(|(id, _)| id == plugin_id)
                    .map(|(_, settings)| settings.clone())
                    .unwrap_or_else(|| detail.profile.slideshow.clone());
                let before = slideshow.clone();
                let saved = detail.profile.slideshow.clone();
                let enabled_changed = ui.checkbox(&mut slideshow.enabled, "启用轮播").changed();
                let mut interval_response = None;
                ui.horizontal(|ui| {
                    ui.label("间隔");
                    interval_response = Some(
                        ui.add(
                            egui::DragValue::new(&mut slideshow.interval_seconds)
                                .range(10..=86_400)
                                .suffix(" 秒"),
                        ),
                    );
                    slideshow_order_chip(
                        ui,
                        &mut slideshow.order,
                        SlideshowOrder::Sequential,
                        "顺序",
                    );
                    slideshow_order_chip(ui, &mut slideshow.order, SlideshowOrder::Random, "随机");
                });
                let interval_response =
                    interval_response.expect("interval drag value is always created");
                if slideshow != before {
                    self.slideshow_draft = Some((plugin_id.to_string(), slideshow.clone()));
                }
                let interval_stepped = slideshow.interval_seconds != before.interval_seconds
                    && !interval_response.dragged();
                let interval_committed = slideshow.interval_seconds != saved.interval_seconds
                    && (interval_response.lost_focus() || interval_response.drag_stopped());
                if enabled_changed
                    || slideshow.order != before.order
                    || interval_stepped
                    || interval_committed
                {
                    self.dispatch(Command::UpdateProfile(
                        plugin_id.to_string(),
                        ProfilePatch {
                            slideshow: Some(SlideshowPatch {
                                enabled: Some(slideshow.enabled),
                                interval_seconds: Some(slideshow.interval_seconds as f64),
                                order: Some(slideshow.order),
                            }),
                            ..ProfilePatch::default()
                        },
                    ));
                }
                if detail.plugin.installed_version.is_some()
                    && ui
                        .add_enabled(
                            self.busy_count == 0,
                            egui::Button::new(RichText::new("卸载插件").color(RED)),
                        )
                        .clicked()
                {
                    self.dispatch(Command::Uninstall(plugin_id.to_string()));
                }
            });
    }
}

impl eframe::App for StudioApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(context);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        if context.input(|input| input.viewport().close_requested()) && !self.quitting {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        self.drain_events(&context);
        context.request_repaint_after(Duration::from_millis(500));
        self.paint_console_background(ui);
        self.render_sidebar(ui);
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(self.canvas_fill())
                    .inner_margin(egui::Margin::symmetric(28, 24)),
            )
            .show(ui, |ui| {
                if let Some((message, error)) = self.notice.clone() {
                    notice_frame(ui, &message, if error { RED } else { CYAN });
                    if ui.small_button("关闭").clicked() {
                        self.notice = None;
                    }
                    ui.add_space(12.0);
                }
                if let Some(progress) = &self.progress {
                    let text = match progress.percent {
                        Some(percent) => format!("{} · {:.0}%", progress.message, percent),
                        None => progress.message.clone(),
                    };
                    notice_frame(ui, &text, CYAN);
                    ui.add_space(12.0);
                }
                egui::ScrollArea::vertical().id_salt("page").show(ui, |ui| {
                    match self.selected_plugin.clone() {
                        Some(plugin_id) => self.render_detail(ui, &plugin_id),
                        None => self.render_overview(ui),
                    }
                });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let core = Arc::clone(&self.core);
        self.runtime.block_on(async move {
            let mut core = core.lock().await;
            core.quit_all_keep_targets();
        });
    }
}

async fn execute_command(
    core: &Arc<Mutex<HostCore>>,
    tx: &mpsc::Sender<UiEvent>,
    command: Command,
) -> Result<(), String> {
    let mut core = core.lock().await;
    match command {
        Command::Refresh => {
            core.reload_catalog()?;
            core.refresh_latest()?;
            core.handshake_enabled().await?;
        }
        Command::Install(id) => {
            let progress_tx = tx.clone();
            let progress_id = id.clone();
            core.install(&id, move |phase, percent, message| {
                let _ = progress_tx.send(UiEvent::Progress(InstallProgress {
                    id: progress_id.clone(),
                    phase: phase.to_string(),
                    percent,
                    message: message.to_string(),
                }));
            })?;
            core.handshake(&id).await?;
            let detail = core.plugin_detail(&id).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::Uninstall(id) => {
            core.uninstall(&id)?;
        }
        Command::SetEnabled(id, enabled) => {
            core.set_enabled(&id, enabled)?;
            if enabled {
                core.handshake(&id).await?;
            }
        }
        Command::PluginAction(id, action) => {
            core.plugin_command(&id, &action).await?;
            let detail = core.plugin_detail(&id).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::LoadDetail(id) => {
            let detail = core.plugin_detail(&id).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::ImportFiles(id, paths) => {
            let result = core.import_files(&paths);
            if result.added.is_empty() && !result.skipped.is_empty() {
                return Err(result
                    .skipped
                    .iter()
                    .map(|item| format!("{}：{}", item.path, item.reason))
                    .collect::<Vec<_>>()
                    .join("\n"));
            }
            let detail = core.plugin_detail(&id).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::ImportFolder(id, folder) => {
            let result = core.import_folder(&folder);
            if result.added.is_empty() && !result.skipped.is_empty() {
                return Err(result
                    .skipped
                    .iter()
                    .map(|item| format!("{}：{}", item.path, item.reason))
                    .collect::<Vec<_>>()
                    .join("\n"));
            }
            let detail = core.plugin_detail(&id).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::ImportRemote(id, url, dynamic) => {
            let result = core.import_remote(&url, dynamic);
            if result.added.is_empty() {
                return Err(result
                    .skipped
                    .first()
                    .map(|item| item.reason.clone())
                    .unwrap_or_else(|| "远程媒体没有返回可导入内容。".to_string()));
            }
            let detail = core.plugin_detail(&id).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::RemoveMedia(id, media_id) => {
            let detail = core.remove_media(&id, &media_id).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::SetActiveMedia(id, media_id) => {
            let detail = core.set_active_media(&id, media_id).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::UpdateProfile(id, patch) => {
            let detail = core.update_profile(&id, patch).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::RefreshMedia(id, media_id) => {
            let detail = core.refresh_media(&id, &media_id).await?;
            let _ = tx.send(UiEvent::Detail(id, detail));
        }
        Command::UpdateHostSettings(auto_start, minimized) => {
            core.set_autostart(auto_start, minimized)?;
            desktop::sync_autostart(auto_start, minimized)?;
        }
        Command::UpdateProxy(proxy) => core.set_proxy(proxy)?,
        Command::RelocateDataDirectory(folder) => {
            core.relocate_data_directory(folder)?;
        }
        Command::InstallHostUpdate => {
            let release = core.host_release().clone();
            let asset_name = release
                .asset_name
                .ok_or_else(|| "请先检查更新。".to_string())?;
            let download_url = release
                .download_url
                .ok_or_else(|| "最新版没有可下载的安装包。".to_string())?;
            let path = crate::host_update::installer_temp_path(&asset_name);
            let settings = core.proxy_settings();
            let progress_tx = tx.clone();
            crate::host_update::download_with_progress(
                &download_url,
                &path,
                &settings,
                move |downloaded, total| {
                    let percent = total
                        .filter(|total| *total > 0)
                        .map(|total| downloaded as f64 / total as f64 * 100.0);
                    let _ = progress_tx.send(UiEvent::Progress(InstallProgress {
                        id: "host".to_string(),
                        phase: "download".to_string(),
                        percent,
                        message: "正在下载宿主更新…".to_string(),
                    }));
                },
            )?;
            crate::host_update::launch_installer(&path)?;
            let _ = tx.send(UiEvent::Quit);
        }
    }
    Ok(())
}

fn start_snapshot_worker(
    core: Arc<Mutex<HostCore>>,
    runtime: Arc<Runtime>,
    tx: mpsc::Sender<UiEvent>,
    context: egui::Context,
) {
    runtime.spawn(async move {
        loop {
            {
                let mut core = core.lock().await;
                if let Err(error) = core.tick_slideshow().await {
                    let _ = tx.send(UiEvent::Error(error));
                }
                let snapshot = core.snapshot().await;
                let _ = tx.send(UiEvent::Snapshot(snapshot));
            }
            context.request_repaint();
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

fn start_activation_worker(
    runtime: Arc<Runtime>,
    tx: mpsc::Sender<UiEvent>,
    context: egui::Context,
) {
    runtime.spawn(async move {
        loop {
            match single_instance::wait_for_activation().await {
                Ok(()) => {
                    let _ = tx.send(UiEvent::ShowWindow);
                    context.request_repaint();
                }
                Err(error) => {
                    let _ = tx.send(UiEvent::Error(error));
                    context.request_repaint();
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
}

fn setup_tray(tx: mpsc::Sender<UiEvent>, context: egui::Context) -> Result<TrayUi, String> {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "打开 Background Studio", true, None);
    let status = MenuItem::new("状态：正在读取…", false, None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    let separator = PredefinedMenuItem::separator();
    menu.append_items(&[&show, &status, &separator, &quit])
        .map_err(|error| error.to_string())?;

    let menu_tx = tx.clone();
    let menu_context = context.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = match event.id.as_ref() {
            "show" => Some(UiEvent::ShowWindow),
            "quit" => Some(UiEvent::Quit),
            _ => None,
        };
        if let Some(action) = action {
            let _ = menu_tx.send(action);
            menu_context.request_repaint();
        }
    }));

    let tray_tx = tx;
    let tray_context = context;
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let should_show = matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                ..
            } | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            }
        );
        if should_show {
            let _ = tray_tx.send(UiEvent::ShowWindow);
            tray_context.request_repaint();
        }
    }));

    let icon = load_tray_icon()?;
    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Background Studio")
        .with_icon(icon)
        .build()
        .map_err(|error| error.to_string())?;
    Ok(TrayUi { icon, status })
}

fn load_sidebar_logo(context: &egui::Context) -> Option<egui::TextureHandle> {
    let image = image::load_from_memory(include_bytes!("../icons/128x128.png"))
        .ok()?
        .into_rgba8();
    let (width, height) = image.dimensions();
    let color =
        egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], image.as_raw());
    Some(context.load_texture("host-logo", color, egui::TextureOptions::LINEAR))
}

fn proxy_mode_chip(ui: &mut egui::Ui, current: &mut ProxyMode, mode: ProxyMode, label: &str) {
    let selected = *current == mode;
    if ui
        .add(
            egui::Button::new(RichText::new(label).color(if selected { INK } else { MUTED }))
                .fill(if selected { CYAN_DIM } else { PAPER_RAISED }),
        )
        .clicked()
    {
        *current = mode;
    }
}

fn slideshow_order_chip(
    ui: &mut egui::Ui,
    current: &mut SlideshowOrder,
    order: SlideshowOrder,
    label: &str,
) {
    let selected = *current == order;
    if ui
        .add(
            egui::Button::new(RichText::new(label).color(if selected { INK } else { MUTED }))
                .fill(if selected { CYAN_DIM } else { PAPER_RAISED }),
        )
        .clicked()
    {
        *current = order;
    }
}

fn load_tray_icon() -> Result<Icon, String> {
    let image = image::load_from_memory(include_bytes!("../icons/icon.png"))
        .map_err(|error| error.to_string())?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).map_err(|error| error.to_string())
}

fn configure_fonts(context: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    for path in [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "background-studio-cjk".to_string(),
                FontData::from_owned(bytes).into(),
            );
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "background-studio-cjk".to_string());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .push("background-studio-cjk".to_string());
            break;
        }
    }
    context.set_fonts(fonts);
}

fn configure_style(context: &egui::Context) {
    let mut style = (*context.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(13.0, 8.0);
    style.spacing.indent = 18.0;
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = CANVAS;
    style.visuals.window_fill = PAPER;
    style.visuals.extreme_bg_color = RAIL;
    style.visuals.faint_bg_color = PAPER_RAISED;
    style.visuals.widgets.noninteractive.bg_fill = PAPER;
    style.visuals.widgets.noninteractive.weak_bg_fill = PAPER;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    style.visuals.widgets.inactive.bg_fill = PAPER_RAISED;
    style.visuals.widgets.inactive.weak_bg_fill = PAPER_RAISED;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    style.visuals.widgets.inactive.corner_radius = 9.0.into();
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(36, 49, 63);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(36, 49, 63);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, CYAN_DIM);
    style.visuals.widgets.hovered.corner_radius = 9.0.into();
    style.visuals.widgets.active.bg_fill = CYAN_DIM;
    style.visuals.widgets.active.weak_bg_fill = CYAN_DIM;
    style.visuals.widgets.active.corner_radius = 9.0.into();
    style.visuals.selection.bg_fill = CYAN_DIM;
    style.visuals.selection.stroke = Stroke::new(1.0, CYAN);
    style.visuals.hyperlink_color = CYAN;
    context.set_style_of(egui::Theme::Dark, style);
}

fn plugin_version_label(plugin: &crate::plugins::PluginCard) -> String {
    match (
        plugin.installed_version.as_deref(),
        plugin.latest_version.as_deref(),
        plugin.update_available,
    ) {
        (Some(installed), Some(latest), true) => {
            format!("{} → {}", strip_v(installed), strip_v(latest))
        }
        (Some(installed), _, _) => strip_v(installed).to_string(),
        (None, Some(latest), _) => format!("未安装 · {}", strip_v(latest)),
        (None, None, _) => "未安装".to_string(),
    }
}

fn strip_v(version: &str) -> &str {
    version.trim_start_matches('v')
}

fn navigation_button(
    ui: &mut egui::Ui,
    text: &str,
    subtitle: Option<&str>,
    selected: bool,
) -> egui::Response {
    let height = if subtitle.is_some() { 54.0 } else { 42.0 };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(238.0, height), egui::Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 10.0, PAPER_RAISED);
        let accent = egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height()));
        ui.painter().rect_filled(accent, 2.0, CYAN);
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, 10.0, Color32::from_rgb(18, 27, 36));
    }
    if let Some(subtitle) = subtitle {
        ui.painter().text(
            egui::pos2(rect.min.x + 18.0, rect.center().y - 8.0),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(14.0),
            if selected { INK } else { MUTED },
        );
        ui.painter().text(
            egui::pos2(rect.min.x + 18.0, rect.center().y + 10.0),
            egui::Align2::LEFT_CENTER,
            subtitle,
            egui::FontId::proportional(11.0),
            if selected {
                BRASS
            } else {
                Color32::from_rgb(102, 118, 132)
            },
        );
    } else {
        ui.painter().text(
            egui::pos2(rect.min.x + 18.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(14.0),
            if selected { INK } else { MUTED },
        );
    }
    response
}

/// 给基色附加透明度，让控制台背景图能透出来。
fn glass(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// 直接用画笔按等比缩放绘制插件 logo；不用 ui.put，避免额外占用布局空间。
fn paint_icon(ui: &egui::Ui, icon: &egui::TextureHandle, rect: egui::Rect) {
    let size = icon.size_vec2();
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    let fitted = egui::Rect::from_center_size(rect.center(), size * scale);
    ui.painter().image(
        icon.id(),
        fitted,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

fn page_heading(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(30.0).strong().color(INK));
    ui.add_space(2.0);
    ui.label(RichText::new(subtitle).size(13.0).color(MUTED));
    ui.add_space(20.0);
}

fn notice_frame(ui: &mut egui::Ui, message: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
        .corner_radius(10.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.label(RichText::new(message).color(color));
        });
}

fn metric(ui: &mut egui::Ui, label: &str, value: usize, fill: Color32) {
    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(14.0)
        .inner_margin(16.0)
        .show(ui, |ui| {
            ui.set_min_width(136.0);
            ui.label(
                RichText::new(value.to_string())
                    .size(26.0)
                    .strong()
                    .color(CYAN),
            );
            ui.label(RichText::new(label).size(11.0).color(MUTED));
        });
}

fn phase_color(phase: &str, running: bool) -> Color32 {
    if !running {
        return MUTED;
    }
    match phase {
        "active" => GREEN,
        "error" => RED,
        "paused" | "waiting_manual" => AMBER,
        "starting" | "takeover" | "attaching" => CYAN,
        _ => MUTED,
    }
}

fn phase_dot(phase: &str, running: bool) -> &'static str {
    match phase_color(phase, running) {
        GREEN => "●",
        CYAN => "◆",
        AMBER => "▲",
        RED => "■",
        _ => "○",
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / 1024.0 / 1024.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn thumbnail_placeholder(ui: &mut egui::Ui, size: Vec2, label: &str) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect_filled(rect, 9.0, PAPER_RAISED);
    ui.painter()
        .rect_stroke(rect, 9.0, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::monospace(11.0),
        CYAN,
    );
}

fn render_schema_fields(ui: &mut egui::Ui, fields: Option<&Map<String, Value>>, draft: &mut Value) {
    let Some(fields) = fields else {
        ui.label(RichText::new("这个插件没有显示设置。").color(MUTED));
        return;
    };
    if !draft.is_object() {
        *draft = Value::Object(Map::new());
    }
    let display = draft.as_object_mut().expect("display object");
    for (key, schema) in fields {
        let title = schema.get("title").and_then(Value::as_str).unwrap_or(key);
        match schema.get("type").and_then(Value::as_str) {
            Some("boolean") => {
                let mut value = display.get(key).and_then(Value::as_bool).unwrap_or(false);
                if ui.checkbox(&mut value, title).changed() {
                    display.insert(key.clone(), Value::Bool(value));
                }
            }
            Some("number") | Some("integer") => {
                let minimum = schema.get("minimum").and_then(Value::as_f64).unwrap_or(0.0);
                let maximum = schema
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .unwrap_or(100.0);
                let step = schema.get("step").and_then(Value::as_f64).unwrap_or(0.01);
                let mut value = display.get(key).and_then(Value::as_f64).unwrap_or(minimum);
                ui.label(RichText::new(title).size(12.0).color(INK));
                if ui
                    .add(
                        egui::Slider::new(&mut value, minimum..=maximum)
                            .step_by(step)
                            .show_value(true),
                    )
                    .changed()
                {
                    if let Some(number) = serde_json::Number::from_f64(value) {
                        display.insert(key.clone(), Value::Number(number));
                    }
                }
            }
            Some("string") => {
                if let Some(values) = schema.get("enum").and_then(Value::as_array) {
                    let labels = schema.get("enumLabels").and_then(Value::as_array);
                    let mut current = display
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    egui::ComboBox::from_label(title)
                        .selected_text(&current)
                        .show_ui(ui, |ui| {
                            for (index, option) in
                                values.iter().filter_map(Value::as_str).enumerate()
                            {
                                let label = labels
                                    .and_then(|labels| labels.get(index))
                                    .and_then(Value::as_str)
                                    .unwrap_or(option);
                                ui.selectable_value(&mut current, option.to_string(), label);
                            }
                        });
                    display.insert(key.clone(), Value::String(current));
                } else {
                    let mut value = display
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    ui.label(RichText::new(title).size(12.0).color(INK));
                    if ui.text_edit_singleline(&mut value).changed() {
                        display.insert(key.clone(), Value::String(value));
                    }
                }
            }
            _ => {}
        }
        ui.add_space(4.0);
    }
}
