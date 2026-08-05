#[derive(Clone, Copy, Debug)]
pub struct PluginDef {
    pub id: &'static str,
    pub display_name: &'static str,
    pub owner: &'static str,
    pub repo: &'static str,
    pub asset_prefix: &'static str,
    pub exe_name: &'static str,
    pub pipe_name: &'static str,
}

pub const PLUGIN_PROTOCOL: u32 = 1;

pub const PLUGINS: &[PluginDef] = &[
    PluginDef {
        id: "codex",
        display_name: "Codex Background Studio",
        owner: "background-studio",
        repo: "codex_desktop_background",
        asset_prefix: "CodexBackgroundStudio-",
        exe_name: "Codex Background Studio.exe",
        pipe_name: r"\\.\pipe\background-studio-codex",
    },
    PluginDef {
        id: "notion",
        display_name: "Notion Background Studio",
        owner: "background-studio",
        repo: "notion_desktop_background",
        asset_prefix: "NotionBackgroundStudio-",
        exe_name: "Notion Background Studio.exe",
        pipe_name: r"\\.\pipe\background-studio-notion",
    },
];

pub fn find(id: &str) -> Option<&'static PluginDef> {
    PLUGINS.iter().find(|plugin| plugin.id == id)
}
