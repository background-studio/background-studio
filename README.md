# Background Studio

[![org](https://img.shields.io/badge/org-background--studio-0ea5e9)](https://github.com/background-studio)
[![release](https://img.shields.io/github/v/release/background-studio/background-studio)](https://github.com/background-studio/background-studio/releases)

统一壳：Windows 托盘只显示一个 **Background Studio**，集中管理媒体、设置并在后台安装、启停
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
- 共享媒体库、插件 Profile、轮播与统一插件详情页；插件不再各自维护 WebView 界面
- 启用后以 `--plugin` 启动纯 Rust worker（无窗口、WebView 或独立托盘），自动等待并接管之后启动的目标程序
- 启用前已经运行的目标不会被静默关闭，可从壳里手动“重启并接管”
- 协议 2 通过 Named Pipe 下发回环媒体与显示设置，并实时汇总等待 / 接管 / 活动状态
- 协议 1 插件可在升级期间继续启停和应用，但不提供统一媒体与设置能力
- 「检查更新」同时检查插件与壳自身；可下载并启动壳安装包
- 本机已安装并登录 [GitHub CLI](https://cli.github.com/)（`gh auth login`）时，查版优先走 `gh api`，不易撞匿名 API 限流；未登录则仍用匿名 HTTP
- 仅注册壳自己的开机启动
- 检测到独立版自启动时提示，避免双托盘（不自动卸载）

协议见 [docs/plugin-protocol.md](./docs/plugin-protocol.md)，迁移见
[docs/migration.md](./docs/migration.md)。

## 开发

要求 Rust stable（1.95+）和 MSVC C++ Build Tools。原生壳不依赖 Node.js 或 WebView2。

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo test --manifest-path src-tauri/Cargo.toml
cargo run --manifest-path src-tauri/Cargo.toml
```

打包：

```powershell
cargo install cargo-packager --version 0.11.8 --locked
cd src-tauri
cargo packager --release
```

## 发布

```powershell
git tag v0.1.3
git push origin v0.1.3
```

推送 `v*` 标签后，GitHub Actions 构建 NSIS 并创建 Release。
壳依赖插件 Release 中存在 `*-plugin.zip`（Codex ≥ 0.5.0 / Notion ≥ 0.2.0 / Multica ≥ 0.1.0）。
