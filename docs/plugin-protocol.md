# Background Studio 插件协议

`pluginProtocol: 1`

壳仓库权威说明。Codex / Notion / Multica 插件仓各保留一份摘要。

## 启动

```text
<Plugin>.exe --plugin
```

插件模式：无托盘、无自启动注册、窗口默认隐藏，通过 Named Pipe 接受命令。

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
