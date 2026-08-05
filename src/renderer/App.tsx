import { useEffect, useState, useTransition } from "react";
import {
  getSnapshot,
  installPlugin,
  onSnapshot,
  openDataDirectory,
  pluginCommand,
  refreshReleases,
  setPluginEnabled,
  uninstallPlugin,
  updateHostSettings,
  type HostSnapshot,
  type PluginCard,
} from "./bridge";

export function App() {
  const [snapshot, setSnapshot] = useState<HostSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [, startTransition] = useTransition();

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    (async () => {
      try {
        setSnapshot(await getSnapshot());
        unlisten = await onSnapshot((next) => {
          startTransition(() => setSnapshot(next));
        });
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      }
    })();
    return () => {
      unlisten?.();
    };
  }, []);

  async function run(label: string, action: () => Promise<HostSnapshot>) {
    setBusyId(label);
    setError(null);
    try {
      setSnapshot(await action());
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
    }
  }

  if (!snapshot) {
    return (
      <div className="app">
        <h1>Background Studio</h1>
        <p className="meta">正在加载插件状态…</p>
        {error ? <p className="error">{error}</p> : null}
      </div>
    );
  }

  return (
    <div className="app">
      <header className="hero">
        <div>
          <h1>Background Studio</h1>
          <p>
            一个托盘管理 Codex / Notion 背景插件。插件仍从各自 GitHub Release
            安装，注入逻辑不进壳。
          </p>
        </div>
        <div className="actions">
          <button
            className="primary"
            disabled={busyId !== null}
            onClick={() => run("refresh", refreshReleases)}
          >
            检查更新
          </button>
          <button disabled={busyId !== null} onClick={() => openDataDirectory()}>
            打开数据目录
          </button>
        </div>
      </header>

      {snapshot.warning ? <div className="warning">{snapshot.warning}</div> : null}

      <div className="grid">
        {snapshot.plugins.map((plugin) => (
          <PluginPanel
            key={plugin.id}
            plugin={plugin}
            busy={busyId !== null}
            onInstall={() => run(plugin.id, () => installPlugin(plugin.id))}
            onUninstall={() => run(plugin.id, () => uninstallPlugin(plugin.id))}
            onToggle={(enabled) =>
              run(plugin.id, () => setPluginEnabled(plugin.id, enabled))
            }
            onCommand={(cmd) => run(plugin.id, () => pluginCommand(plugin.id, cmd))}
          />
        ))}
      </div>

      <section className="settings">
        <label>
          <input
            type="checkbox"
            checked={snapshot.autoStartWithWindows}
            disabled={busyId !== null}
            onChange={(event) =>
              run("settings", () =>
                updateHostSettings(event.target.checked, snapshot.startMinimized),
              )
            }
          />
          开机启动 Background Studio
        </label>
        <label>
          <input
            type="checkbox"
            checked={snapshot.startMinimized}
            disabled={busyId !== null}
            onChange={(event) =>
              run("settings", () =>
                updateHostSettings(snapshot.autoStartWithWindows, event.target.checked),
              )
            }
          />
          启动时最小化到托盘
        </label>
        <span className="meta">数据目录：{snapshot.dataDirectory}</span>
      </section>

      {error ? <p className="error">{error}</p> : null}
    </div>
  );
}

function PluginPanel(props: {
  plugin: PluginCard;
  busy: boolean;
  onInstall: () => void;
  onUninstall: () => void;
  onToggle: (enabled: boolean) => void;
  onCommand: (cmd: string) => void;
}) {
  const { plugin, busy } = props;
  const installed = Boolean(plugin.installedVersion);

  return (
    <article className="card">
      <div>
        <h2>{plugin.displayName}</h2>
        <p className="meta">目标：{plugin.targetHint}</p>
      </div>
      <div className="row">
        <span className={`badge ${plugin.running ? "ok" : ""}`}>
          {plugin.statusMessage}
        </span>
        {plugin.installedVersion ? (
          <span className="badge">已装 v{plugin.installedVersion}</span>
        ) : null}
        {plugin.latestVersion ? (
          <span className="badge">最新 v{plugin.latestVersion}</span>
        ) : (
          <span className="badge warn">暂无 plugin.zip Release</span>
        )}
        {plugin.updateAvailable ? <span className="badge warn">可更新</span> : null}
      </div>
      <p className="meta">
        相位：{plugin.phase} · 协议 v{plugin.pluginProtocol}
      </p>
      <div className="row">
        {!installed ? (
          <button className="primary" disabled={busy} onClick={props.onInstall}>
            从 Release 安装
          </button>
        ) : (
          <>
            <button className="primary" disabled={busy} onClick={props.onInstall}>
              {plugin.updateAvailable ? "更新插件" : "重新安装"}
            </button>
            <button
              disabled={busy}
              onClick={() => props.onToggle(!plugin.enabled)}
            >
              {plugin.enabled ? "停用" : "启用"}
            </button>
            <button disabled={busy} onClick={() => props.onCommand("open-ui")}>
              打开设置
            </button>
            <button disabled={busy} onClick={() => props.onCommand("apply")}>
              应用背景
            </button>
            <button disabled={busy} onClick={() => props.onCommand("pause")}>
              暂停
            </button>
            <button disabled={busy} onClick={() => props.onCommand("restore")}>
              恢复官方
            </button>
            <button className="danger" disabled={busy} onClick={props.onUninstall}>
              卸载
            </button>
          </>
        )}
      </div>
    </article>
  );
}
