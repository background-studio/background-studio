# Background Studio 插件协议

`pluginProtocol: 1`

壳仓库权威说明。Codex / Notion 插件仓各保留一份摘要。

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

## 命令（NDJSON）

请求：`{"id":"1","cmd":"status|open-ui|apply|pause|restore|quit-keep-target"}`

成功：`{"id":"1","ok":true,"result":{...}}`

失败：`{"id":"1","ok":false,"error":"..."}`

## Release 产物

- `CodexBackgroundStudio-<version>-plugin.zip`
- `NotionBackgroundStudio-<version>-plugin.zip`

壳安装到：

`%LOCALAPPDATA%\BackgroundStudio\plugins\<pluginId>\<version>\`
