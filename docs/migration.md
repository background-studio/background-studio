# 从独立 Studio 迁到 Background Studio

## 推荐用法

1. 安装 Background Studio 壳（NSIS Release，或 Scoop：
   `scoop bucket add background-studio https://github.com/background-studio/scoop-bucket` → `scoop install background-studio`）
2. 在壳里分别安装 Codex / Notion / Multica 插件（从各自仓库 Release 下载 `*-plugin.zip`）
3. Windows 托盘只保留 **Background Studio** 一个图标

壳更新：界面「检查更新」后点「更新壳」，或 `scoop update background-studio`。

## 独立版怎么办

- 已安装的 Codex / Notion / Multica NSIS **仍可单独使用**
- 不建议与壳插件版同时运行：会出现双托盘、重复注入
- 若注册表自启动里仍有 `Codex Background Studio` / `Notion Background Studio` / `Multica Background Studio`：
  1. 打开对应独立版，关掉「开机启动」
  2. 或在壳界面按提示处理
  3. 再只开 Background Studio

壳**不会**静默卸载你的独立版。

## 设置会丢吗

不会。插件模式仍读写各自目录：

- `%LOCALAPPDATA%\CodexBackgroundStudio`
- `%LOCALAPPDATA%\NotionBackgroundStudio`
- `%LOCALAPPDATA%\MulticaBackgroundStudio`

壳自己的状态在：

- `%LOCALAPPDATA%\BackgroundStudio`

## 仓库位置

组织：[background-studio](https://github.com/background-studio)

| 产品 | 仓库 |
|------|------|
| 壳 | [background-studio](https://github.com/background-studio/background-studio) |
| Codex 插件 | [codex_desktop_background](https://github.com/background-studio/codex_desktop_background) |
| Notion 插件 | [notion_desktop_background](https://github.com/background-studio/notion_desktop_background) |
| Multica 插件 | [multica_desktop_background](https://github.com/background-studio/multica_desktop_background) |
| Scoop bucket | [scoop-bucket](https://github.com/background-studio/scoop-bucket) |
