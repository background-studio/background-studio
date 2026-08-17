# Background Studio 插件协议

`pluginProtocol: 1`

壳仓库权威说明。Codex / Notion / Multica 插件仓各保留一份摘要。

## 启动

```text
<Plugin>.exe --plugin
```

插件模式：无托盘、无自启动注册、窗口默认隐藏，通过 Named Pipe 接受命令。

新版插件在该模式下还会进入自动托管：

- 目标程序未运行时只等待，不会替用户打开程序。
- 用户之后正常启动目标程序时，插件会校验完整可执行路径，将这次普通启动重启为仅监听本机的调试会话，并自动应用上次背景。
- 插件启动前已经存在的普通目标进程不会被静默关闭；`status` 会提示需要手动执行 `apply` 来重启接管。
- 已有合法调试会话时直接重连；目标退出后重新等待下一次启动。
- `pause` / `restore` 会暂停本次插件进程内的自动接管，后续 `apply` 会重新启用托管。
- 壳停用插件时只停止后续托管，不强制关闭或重启当前目标程序。

该机制不会修改目标程序文件、签名、快捷方式或注册表启动入口。

## Pipe

| 插件 | Pipe |
|------|------|
| Codex | `\\.\pipe\background-studio-codex` |
| Notion | `\\.\pipe\background-studio-notion` |
| Multica | `\\.\pipe\background-studio-multica` |

## 命令（NDJSON）

请求：`{"id":"1","cmd":"status|open-ui|apply|pause|restore|quit-keep-target"}`

成功：`{"id":"1","ok":true,"result":{...}}`

失败：`{"id":"1","ok":false,"error":"..."}`

`status.result.phase` 是可扩展字符串。自动托管常见状态包括等待目标启动、已有普通进程等待手动接管、正在接管、活动、暂停和错误；壳应优先直接展示 `message`，不要把“插件进程存在”当成“背景已经注入”。

## Release 产物

- `CodexBackgroundStudio-<version>-plugin.zip`
- `NotionBackgroundStudio-<version>-plugin.zip`
- `MulticaBackgroundStudio-<version>-plugin.zip`

壳安装到：

`%LOCALAPPDATA%\BackgroundStudio\plugins\<pluginId>\<version>\`

## 动态插件目录（壳）

壳不把插件列表写死在 UI。内置一份 `resources/catalog.json`，启动时会复制/合并到：

`%LOCALAPPDATA%\BackgroundStudio\catalog.json`

要加新插件（例如本地开发中的 Multica）：

1. 编辑上面的 `catalog.json`，追加一项（`id` / `owner` / `repo` / `assetPrefix` / `exeName` / `pipeName` / `icon`）。
2. 把图标放到 `%LOCALAPPDATA%\BackgroundStudio\icons\<id>.png`（或壳数据目录下的 `icons\`）。
3. 在壳界面点「刷新列表」，或重启壳。

同 `id` 时本地覆盖内置项。Release 上有对应 `*-plugin.zip` 后即可安装。
