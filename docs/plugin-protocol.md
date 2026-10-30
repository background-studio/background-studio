# Background Studio 插件协议

当前协议：`pluginProtocol: 2`。壳在过渡期仍可启动协议 1 插件，但统一媒体库、壳内设置和热更新只对协议 2 开放。

## 安装包与 Manifest

插件 Release 只发布给壳安装的 zip：

- `CodexBackgroundStudio-<version>-plugin.zip`
- `NotionBackgroundStudio-<version>-plugin.zip`
- `MulticaBackgroundStudio-<version>-plugin.zip`
- `RebasedBackgroundStudio-<version>-plugin.zip`

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

Rebased 是 IntelliJ 平台应用，不走调试会话：worker 往 Rebased 的用户插件目录放一个桥接插件，
由它在 Rebased 进程内显示背景。桥接插件装好之前就在运行的 Rebased 需要 `apply` 重启一次；
细节见 [rebased_desktop_background 的协议文档](https://github.com/background-studio/rebased_desktop_background/blob/main/docs/plugin-protocol.md)。

## Named Pipe

| 插件 | Pipe |
|------|------|
| Codex | `\\.\pipe\background-studio-codex` |
| Notion | `\\.\pipe\background-studio-notion` |
| Multica | `\\.\pipe\background-studio-multica` |
| Rebased | `\\.\pipe\background-studio-rebased` |

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

#### 可选扩展：按窗口独立媒体

仅在运行中 worker 的 `hello.capabilities.perWindowMedia === true` 时协商启用；不能仅凭安装清单推断。未声明该能力的插件仍使用原来的同步轮播。

- 基础 `configure` 增加 `independentWindows: true`，用于配置并启动窗口发现；保留原来的 `media` 和 `display` 字段，但不将基础媒体覆盖到已分配的窗口。
- `status` 增加 `independentWindows`、`targetIds: string[]`、`configuredTargetIds: string[]`。只有实际通过 Agent 页面探测的窗口才能进入 `targetIds`，IDE 不包含在内。ID 为 1–64 个 ASCII 字母、数字、`-` 或 `_`。
- 单窗口 `configure` 在原字段之外增加 `independentWindows: true`、`targetId: "<id>"`。每次只传一个媒体，完整验证后原子替换该窗口的配置，不影响其他窗口。未建立独立模式时拒绝定向配置。
- 宿主保留各窗口的媒体与计时点；即使关闭轮播也为新窗口分配背景，关闭轮播时只使用当前激活源。开启轮播时使用播放列表（没有列表时使用激活源），展开文件夹但不推进共享预览游标。随机/顺序选择优先避开其他窗口占用的文件及自己的上一张；文件不足允许重复。
- 显示设置变更复用现有选择；新窗口不改变已有选择及其计时点。窗口关闭后释放对应配置下载槽；worker 丢失配置时重发该窗口原选择。暂停期间不下发或自动执行 `apply`。
- 每个 `(pluginId, targetId)` 使用独立、带令牌与版本校验的回环下载槽，避免其他窗口或预览换图改变正在下载的内容；不向 worker 传本机路径。

宿主与 Cursor Agent worker 均每 2 秒轮询；注入时延还包括页面加载和媒体传输。独立轮播需要同时更新两端，旧宿主搭配新 worker 仍保持原单媒体行为。

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

目录按 **内置 → 官方清单 → 本地** 三层合并（后者覆盖前者）：

1. 内置 `resources/catalog.json`：离线 bootstrap。
2. 官方静态清单 `https://raw.githubusercontent.com/background-studio/.github/main/registry/v1.json`（网络或上次成功缓存）。
3. 本地扩展 `%LOCALAPPDATA%\BackgroundStudio\catalog.json`：仅用于自定义插件。新装只写空模板；旧版本复制出的内置项（同 `id` 且 `owner` 为 `background-studio`）视为官方镜像并忽略，不会遮挡官方新元数据。同 `id` 但 `owner` 不同的项是用户替换的第三方源，照常覆盖，并按其自身仓库查版本。

目录项只负责发现仓库和 Release；运行时 exe、pipe、能力和设置 Schema 以已安装版本的 `plugin.json` 为准。

## 官方静态清单（registry v1）

一次 `GET` 同时提供官方目录、插件版本与壳版本；壳不依赖 `gh` / 登录，也不为官方插件或壳访问 GitHub REST API。

```jsonc
{
  "schemaVersion": 1,
  "generatedAt": "2026-05-01T08:00:00Z",          // RFC 3339
  "plugins": [{
    "id": "factory", "displayName": "...", "owner": "background-studio", "repo": "factory_desktop_background",
    "assetPrefix": "FactoryBackgroundStudio-", "exeName": "Factory Background Studio.exe",
    "pipeName": "\\\\.\\pipe\\background-studio-factory", "targetHint": "...", "icon": "factory.png",
    "releases": { "stable": Release | null, "prerelease": Release | null }
  }],
  "host": { "stable": Release | null, "prerelease": Release | null }
}
// Release
{ "version": "0.1.1", "tag": "v0.1.1", "assetName": "FactoryBackgroundStudio-0.1.1-plugin.zip",
  "downloadUrl": "https://github.com/background-studio/<repo>/releases/download/<tag>/<assetName>",
  "releaseUrl": "https://github.com/background-studio/<repo>/releases/tag/<tag>",
  "sha256": "<64 位 hex>" | null }
```

校验规则（任一项不合规则整份拒绝，回用上次成功缓存并在界面顶部警告）：

- 响应体 ≤ 1 MiB，单次请求 15 秒超时，不跟随重定向；未知字段忽略。
- `schemaVersion == 1`；插件 ≤ 256 项；`id` 为 `^[a-z0-9][a-z0-9-]{0,63}$`、唯一、不得为 `console` / `host`。
- `owner` 必须为 `background-studio`；`repo` / `assetPrefix` 仅含 `[A-Za-z0-9._-]`；`exeName` 为 `.exe` 纯文件名；`pipeName` 为 `\\.\pipe\` + `[A-Za-z0-9._-]+`；`icon` 为 null 或 `.png` 纯文件名。
- `releases` / `host` 必须同时含 `stable` 与 `prerelease` 键（无版本写 null）；stable 版本不得带预发布段，prerelease 必须带。
- `tag == "v" + version`；插件 `assetName == assetPrefix + version + "-plugin.zip"`，壳 `assetName == "Background.Studio_" + version + "_x64-setup.exe"`；`downloadUrl` / `releaseUrl` 必须精确等于由 owner/repo/tag/asset 拼出的 GitHub 地址。

客户端行为：

- 缓存：`%LOCALAPPDATA%\BackgroundStudio\cache\official-registry-v1.json`（原子写），TTL 5 分钟；过期后带 `If-None-Match` 重验，304 沿用缓存。合法但 `generatedAt` 更旧的响应不覆盖较新缓存（防 CDN 回滚），并给出警告。
- 失败：指数退避 30 秒起、上限 10 分钟；回用上次成功缓存并在界面明确警告；没有缓存时官方插件与壳暂不显示新版本。官方插件**绝不**回退 REST；清单未收录的内置插件也不会偷偷查 API。
- 手动「检查更新」跳过 TTL 与官方退避（仍带 ETag）；启动时的自动检查遵守 TTL 与退避。
- 渠道：壳版本带预发布段时插件取 `releases.prerelease`，否则取 `releases.stable`；prerelease 为 null 时不回落 stable。壳自身始终只看 `host.stable`。
- 安装仍由用户触发；`sha256` 非空时在解压与停止旧 worker 之前校验，壳安装包在启动安装程序前校验；为 null 时界面提示“无法校验完整性”。
- 图标：没有本地 / 内置图标的官方插件从 `registry/icons/<id>.png` 拉取（≤ 2 MiB PNG），缓存到数据目录 `icons/official/<id>.png`；仅在清单 `generatedAt` 变化且距上次检查 ≥ 6 小时时带 ETag 重验，每次最多 8 个。
- 一次查更的网络总预算 60 秒（持有核心锁期间有界）。

本地自定义插件（非官方镜像）仍走 GitHub REST（已登录 `gh` 时用 `gh api`，否则匿名），结果缓存于 `cache\github-api-cache-v1.json`，TTL 30 分钟，失败退避（手动刷新也遵守），版本没有 SHA-256。

## 查更诊断模式

```
"Background Studio.exe" --check-updates-json <outputPath> [--force-refresh]
                        [--diagnostic-data-dir <dir> | --data-root <dir> --cache-dir <dir>]
```

与界面「检查更新」走同一生产路径，但不取单实例锁、不启动 worker、不写 `plugins.json` / profile / 自启动；会更新查更缓存与官方图标缓存。`--diagnostic-data-dir` 把数据目录、本地 catalog 与缓存全部放到该目录，完全隔离真实配置。报告包含数据源、缓存状态、官方清单 GET 与图标 GET 次数、REST 调用次数（本次与进程级实测计数）、各插件与壳的选中版本及 `sha256`；代理地址中的账号密码会被打码。`status` 为 `ok` / `degraded`（回用缓存、保留较新缓存、部分失败等）/ `failed`，退出码分别为 0 / 3 / 1，参数错误为 2。
