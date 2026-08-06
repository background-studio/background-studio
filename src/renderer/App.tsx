import { useEffect, useState, useTransition } from "react";
import {
  chooseDataDirectory,
  getSnapshot,
  installPlugin,
  onSnapshot,
  openDataDirectory,
  pluginCommand,
  pluginIconSrc,
  refreshReleases,
  reloadCatalog,
  setPluginEnabled,
  uninstallPlugin,
  updateHostSettings,
  type HostSnapshot,
  type PluginCard,
} from "./bridge";

const APP_VERSION = "0.1.2";

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
      <div className="loading">
        <img src="/brand.png" alt="Background Studio" />
        <h1>Background Studio</h1>
        <p>正在加载…</p>
        {error ? <p className="error">{error}</p> : null}
      </div>
    );
  }

  return (
    <div className="app">
      <aside className="brand-rail">
        <img className="brand-mark" src="/brand.png" alt="Background Studio" />
        <div className="brand-copy">
          <h1>Background Studio</h1>
          <p>一个托盘管理背景插件</p>
        </div>
        <div className="brand-foot">
          版本 {APP_VERSION}
          <br />
          插件列表来自 catalog，可动态扩展
        </div>
      </aside>

      <main className="workspace">
        <header className="topbar">
          <div>
            <h2>插件</h2>
            <p>从各自 Release 安装；列表不写死在界面里</p>
          </div>
          <div className="actions">
            <button
              className="btn"
              disabled={busyId !== null}
              onClick={() => run("catalog", reloadCatalog)}
            >
              刷新列表
            </button>
            <button
              className="btn primary"
              disabled={busyId !== null}
              onClick={() => run("refresh", refreshReleases)}
            >
              检查更新
            </button>
          </div>
        </header>

        {snapshot.warning ? <div className="warning">{snapshot.warning}</div> : null}

        <section className="list">
          {snapshot.plugins.map((plugin) => (
            <PluginCardView
              key={plugin.id}
              plugin={plugin}
              busy={busyId !== null}
              onInstall={() => run(plugin.id, () => installPlugin(plugin.id))}
              onUninstall={() => run(plugin.id, () => uninstallPlugin(plugin.id))}
              onToggle={(enabled) =>
                run(plugin.id, () => setPluginEnabled(plugin.id, enabled))
              }
              onCommand={(cmd) =>
                run(plugin.id, () => pluginCommand(plugin.id, cmd))
              }
            />
          ))}
        </section>

        <section className="settings">
          <div className="settings-row">
            <div className="path-wrap">
              <label className="title">数据目录</label>
              <div className="path" title={snapshot.dataDirectory}>
                {snapshot.dataDirectory}
              </div>
            </div>
            <div className="actions">
              <button
                className="btn primary"
                disabled={busyId !== null}
                onClick={() => run("data-dir", chooseDataDirectory)}
              >
                更改目录
              </button>
              <button
                className="btn"
                disabled={busyId !== null}
                onClick={() => openDataDirectory()}
              >
                打开
              </button>
            </div>
          </div>
          <div className="toggles">
            <label className="toggle">
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
              开机自启动
            </label>
            <label className="toggle">
              <input
                type="checkbox"
                checked={snapshot.startMinimized}
                disabled={busyId !== null}
                onChange={(event) =>
                  run("settings", () =>
                    updateHostSettings(
                      snapshot.autoStartWithWindows,
                      event.target.checked,
                    ),
                  )
                }
              />
              启动时进托盘
            </label>
          </div>
        </section>

        {error ? <p className="error">{error}</p> : null}
      </main>
    </div>
  );
}

function PluginCardView(props: {
  plugin: PluginCard;
  busy: boolean;
  onInstall: () => void;
  onUninstall: () => void;
  onToggle: (enabled: boolean) => void;
  onCommand: (cmd: string) => void;
}) {
  const { plugin, busy } = props;
  const installed = Boolean(plugin.installedVersion);
  const [brokenIcon, setBrokenIcon] = useState(false);
  const icon = pluginIconSrc(plugin);

  return (
    <article className="card">
      {brokenIcon ? (
        <div className="card-icon fallback" aria-hidden>
          {plugin.displayName.slice(0, 1)}
        </div>
      ) : (
        <img
          className="card-icon"
          src={icon}
          alt=""
          onError={() => setBrokenIcon(true)}
        />
      )}

      <div className="card-body">
        <h3>{plugin.displayName}</h3>
        <div className="meta">
          <span className={plugin.enabled && installed ? "ok" : undefined}>
            {plugin.statusMessage}
          </span>
          <span>
            {plugin.installedVersion
              ? `已装 v${plugin.installedVersion}`
              : plugin.latestVersion
                ? `最新 v${plugin.latestVersion}`
                : "暂无版本"}
          </span>
          <span>{plugin.targetHint}</span>
          {plugin.updateAvailable ? <span>可更新</span> : null}
        </div>
      </div>

      <div className="card-actions">
        {!installed ? (
          <button className="btn primary" disabled={busy} onClick={props.onInstall}>
            安装
          </button>
        ) : (
          <>
            <button
              className="btn primary"
              disabled={busy}
              onClick={() => props.onToggle(!plugin.enabled)}
            >
              {plugin.enabled ? "停用" : "启用"}
            </button>
            <button
              className="btn"
              disabled={busy}
              onClick={() => props.onCommand("open-ui")}
            >
              打开设置
            </button>
            <button
              className="btn"
              disabled={busy}
              onClick={() => props.onCommand("apply")}
            >
              应用
            </button>
            <button className="btn" disabled={busy} onClick={props.onInstall}>
              {plugin.updateAvailable ? "更新" : "重装"}
            </button>
            <button className="btn danger" disabled={busy} onClick={props.onUninstall}>
              卸载
            </button>
          </>
        )}
      </div>
    </article>
  );
}
