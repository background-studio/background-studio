# Background Studio

[![org](https://img.shields.io/badge/org-background--studio-0ea5e9)](https://github.com/background-studio)
[![release](https://img.shields.io/github/v/release/background-studio/background-studio)](https://github.com/background-studio/background-studio/releases)

统一壳：Windows 托盘只显示一个 **Background Studio**，在后台安装、启停
[Codex](https://github.com/background-studio/codex_desktop_background) /
[Notion](https://github.com/background-studio/notion_desktop_background) /
[Multica](https://github.com/background-studio/multica_desktop_background) 背景插件。

插件仍从各自仓库 Release 下载 `*-plugin.zip`，壳不内嵌 CDP / 注入样式。

> 非 OpenAI / Notion / Multica 官方产品。

## 安装

### NSIS

从 [Releases](https://github.com/background-studio/background-studio/releases) 下载 `Background.Studio_*_x64-setup.exe`。

### Scoop

```powershell
scoop bucket add background-studio https://github.com/background-studio/scoop-bucket
scoop install background-studio
scoop update background-studio
```

## 功能

- 单托盘宿主机
- 从 GitHub Release 安装 / 更新 / 卸载插件（带下载进度）
- 启用后以 `--plugin` 启动 worker（无独立托盘）
- Named Pipe 汇总状态，并转发打开设置 / 应用 / 暂停 / 恢复
- 「检查更新」同时检查插件与壳自身；可下载并启动壳安装包
- 仅注册壳自己的开机启动
- 检测到独立版自启动时提示，避免双托盘（不自动卸载）

协议见 [docs/plugin-protocol.md](./docs/plugin-protocol.md)，迁移见
[docs/migration.md](./docs/migration.md)。

## 开发

要求 Node.js 22+、Rust stable、MSVC C++ Build Tools、WebView2。

```powershell
npm install
npm run check
npm run dev
```

打包：

```powershell
npm run package:win
```

## 发布

```powershell
git tag v0.1.3
git push origin v0.1.3
```

推送 `v*` 标签后，GitHub Actions 构建 NSIS 并创建 Release。
壳依赖插件 Release 中存在 `*-plugin.zip`（Codex ≥ 0.5.0 / Notion ≥ 0.2.0 / Multica ≥ 0.1.0）。
