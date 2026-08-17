# Background Studio 插件协议

当前协议：`pluginProtocol: 2`。壳在过渡期仍可启动协议 1 插件，但统一媒体库、壳内设置和热更新只对协议 2 开放。

## 安装包与 Manifest

插件 Release 只发布给壳安装的 zip：

- `CodexBackgroundStudio-<version>-plugin.zip`
- `NotionBackgroundStudio-<version>-plugin.zip`
- `MulticaBackgroundStudio-<version>-plugin.zip`

zip 根目录必须包含 worker exe 和 `plugin.json`。壳会限制压缩包文件数量、单文件大小和总解压大小，先解到暂存目录，校验 Manifest 与 exe 后再原子替换旧版本。

`plugin.json` 至少声明：

```json
{
  "schemaVersion": 1,
  "pluginProtocol": 2,
  "id": "codex",
  "displayName": "Codex Background Studio",
  "exeName": "Codex Background Studio.exe",
  "pipeName": "\\\\.\\pipe\\background-studio-codex",
  "capabilities": {
    "mediaKinds": ["image", "video"],
    "maxMediaBytes": 67108864
  },
  "settingsSchema": {
    "type": "object",
    "additionalProperties": false,
    "properties": {}
  }
}
```

`settingsSchema.properties` 只描述插件自己的 `display` 字段。壳用它生成设置控件、填充默认值并过滤未知字段；它不是 `configure` 请求本身的 JSON Schema。

安装位置：

`%LOCALAPPDATA%\BackgroundStudio\plugins\<pluginId>\<version>\`

## 启动与生命周期

```text
<Plugin>.exe --plugin
```

协议 2 插件是纯 Rust 无界面 worker：不创建 WebView、窗口、托盘或自启动项。壳启动 worker 后先轮询 `status`，再发送 `hello`，校验协议、插件 ID 与能力；有激活媒体时随后发送 `configure`。

自动托管规则：

- 没有收到有效 `configure` 时只报告“尚未配置背景”，不会接管目标程序。
- 目标程序未运行时只等待，不替用户打开程序。
- 用户之后正常启动目标程序时，worker 校验完整可执行路径，将普通启动切换成仅监听本机的调试会话，再自动注入背景。
- worker 启动前已存在的普通目标进程不会被静默关闭；`status` 提示用户手动执行 `apply`。
- 已有合法调试会话时直接重连；目标退出后重新等待。
- `pause` / `restore` 暂停当前 worker 的自动接管，后续 `apply` 会重新武装 watcher。
- `shutdown` 只退出 worker，保留目标程序和当前已注入效果。

该机制不修改目标程序文件、签名、快捷方式或注册表启动入口。

## Named Pipe

| 插件 | Pipe |
|------|------|
| Codex | `\\.\pipe\background-studio-codex` |
| Notion | `\\.\pipe\background-studio-notion` |
| Multica | `\\.\pipe\background-studio-multica` |

传输格式为一行一条 UTF-8 JSON（NDJSON）。请求与响应必须使用同一个非空 `id`：

```json
{"id":"1","cmd":"status"}
{"id":"1","ok":true,"result":{}}
{"id":"1","ok":false,"error":"可读错误信息"}
```

请求和响应都有大小上限；未知命令、字段类型错误、超长字符串和不匹配的响应 ID 必须失败，不能静默兜底。

## 命令

### `hello`

返回：

```json
{
  "pluginProtocol": 2,
  "pluginId": "codex",
  "version": "0.5.4-beta.2",
  "capabilities": {
    "commands": ["hello", "configure", "status", "apply", "pause", "restore", "shutdown"],
    "mediaKinds": ["image", "video"],
    "hotUpdate": true,
    "autoTakeover": true,
    "loopbackMediaOnly": true,
    "keepsTargetOnShutdown": true,
    "maxMediaBytes": 67108864
  }
}
```

能力可以增加插件专有字段，但 `pluginProtocol`、`pluginId`、`version` 和 `capabilities` 必须存在。

### `configure`

壳拥有媒体库。worker 不接收本机文件路径，也不访问公网；它只从壳生成的临时回环 URL 读取媒体：

```json
{
  "id": "2",
  "cmd": "configure",
  "params": {
    "schemaVersion": 1,
    "revision": "<配置摘要>",
    "media": {
      "url": "http://127.0.0.1:<port>/media/<id>?token=<token>",
      "kind": "image",
      "mimeType": "image/png",
      "sha256": "<64 位十六进制>",
      "byteSize": 12345
    },
    "display": {
      "fit": "cover",
      "opacity": 0.72
    }
  }
}
```

worker 必须拒绝 HTTPS、公网或局域网地址、userinfo、重定向、非法端口、超限媒体，以及不匹配的 `Content-Type`、大小和 SHA-256。下载使用 `no_proxy`，并在流式读取过程中执行大小限制。

有效配置先构建完整注入 payload，再替换内存中的上一份配置。目标已处于 `active` 时可热更新；否则只保存配置并继续等待。

### `status`

至少返回：

```json
{
  "pluginProtocol": 2,
  "pluginId": "codex",
  "version": "0.5.4-beta.2",
  "phase": "idle",
  "message": "等待 Codex 启动",
  "activeTargets": 0,
  "paused": false,
  "configured": true,
  "revision": "<配置摘要>"
}
```

`phase` 是可扩展字符串。壳优先直接展示 `message`，不能把“worker 进程存在”当成“背景已注入”。

### `apply` / `pause` / `restore` / `shutdown`

- `apply`：用最近一次有效配置手动接管，并重新武装 watcher；未配置时失败。
- `pause`：暂停 watcher，并暂停当前注入效果。
- `restore`：暂停 watcher，恢复目标程序官方外观。
- `shutdown`：退出 worker，不恢复或关闭目标程序；成功结果包含 `shutdown: true` 与 `keptTarget: true`。

## 协议 1 过渡兼容

没有 `plugin.json` 或 Manifest 声明协议 1 时，壳不发送 `hello` / `configure`，仍可发送旧版 `status`、`apply`、`pause`、`restore`。旧插件界面不再从壳打开，媒体和设置继续由旧插件自身管理。

协议 2 安装成功后，壳会使用 Manifest 中的 pipe/exe，停用或升级时先发送 `shutdown`；协议 1 插件则按旧流程结束进程。这个兼容层只用于升级过渡，不再扩展新能力。

## 动态插件目录

壳内置 `resources/catalog.json`，并与下面的本地扩展目录合并：

`%LOCALAPPDATA%\BackgroundStudio\catalog.json`

目录项只负责发现仓库和 Release；运行时 exe、pipe、能力和设置 Schema 以已安装版本的 `plugin.json` 为准。同 `id` 时本地目录项覆盖内置项。
