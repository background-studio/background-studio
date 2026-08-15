import { useEffect, useState, useTransition } from "react";
import {
  chooseDataDirectory,
  getSnapshot,
  installPlugin,
  onHostUpdateProgress,
  onInstallProgress,
  onSnapshot,
  openDataDirectory,
  pluginCommand,
  pluginIconSrc,
  refreshReleases,
  reloadCatalog,
  setPluginEnabled,
  uninstallPlugin,
  updateHost,
  updateHostSettings,
  updateProxySettings,
  type HostSnapshot,
  type HostUpdateProgress,
  type InstallProgress,
  type PluginCard,
  type ProxyMode,
} from "./bridge";

const PROXY_SCOPE_HINT =
  "仅影响壳自身的网络：检查更新、下载插件包、下载壳更新，以及壳调用 gh api 时的环境。不影响各插件内部（例如媒体下载），也不影响目标桌面应用。";

export function App() {
  const [snapshot, setSnapshot] = useState<HostSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [installProgress, setInstallProgress] = useState<InstallProgress | null>(null);
  const [hostProgress, setHostProgress] = useState<HostUpdateProgress | null>(null);
  const [draftProxyUrl, setDraftProxyUrl] = useState("");
  const [, startTransition] = useTransition();

  useEffect(() => {
    const unlisteners: Array<() => void> = [];
    (async () => {
      try {
        const initial = await getSnapshot();
        setSnapshot(initial);
        setDraftProxyUrl(initial.proxyUrl);
        unlisteners.push(
          await onSnapshot((next) => {
            startTransition(() => {
              setSnapshot(next);
              setDraftProxyUrl(next.proxyUrl);
            });
          }),
        );
        unlisteners.push(
          await onInstallProgress((progress) => {
            setInstallProgress(progress.phase === "done" ? null : progress);
          }),
        );
        unlisteners.push(
          await onHostUpdateProgress((progress) => {
            setHostProgress(progress);
          }),
        );
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      }
    })();
    return () => {
      for (const unlisten of unlisteners) {
        unlisten();
      }
    };
  }, []);

  async function run(label: string, action: () => Promise<HostSnapshot>) {
    setBusyId(label);
    setError(null);
    try {
      const next = await action();
      setSnapshot(next);
      setDraftProxyUrl(next.proxyUrl);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
      setInstallProgress(null);
    }
  }

  async function applyProxy(mode: ProxyMode, url = draftProxyUrl) {
    await run("proxy", () => updateProxySettings(mode, url));
  }

  async function runHostUpdate() {
    setBusyId("host-update");
    setError(null);
    setHostProgress({ phase: "download", percent: 0, message: "准备更新壳…" });
    try {
      await updateHost();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusyId(null);
      setHostProgress(null);
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
          版本 {snapshot.hostVersion}
          <br />
          插件列表来自 catalog，可动态扩展
          {snapshot.hostUpdateAvailable ? (
            <>
              <br />
              <span className="host-update-hint">
                可更新到 v{snapshot.hostLatestVersion}
              </span>
            </>
          ) : null}
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
            {snapshot.hostUpdateAvailable ? (
              <button
                className="btn primary"
                disabled={busyId !== null}
                onClick={() => void runHostUpdate()}
              >
                更新壳
              </button>
            ) : null}
          </div>
        </header>

        {snapshot.hostUpdateAvailable ? (
          <div className="update-banner">
            Background Studio 可更新到 v{snapshot.hostLatestVersion}。
            点「更新壳」下载并启动安装程序；若用 Scoop 安装，也可用{" "}
            <code>scoop update background-studio</code>。
            {hostProgress ? (
              <div className="progress-block">
                <div className="progress-label">{hostProgress.message}</div>
                <div className="progress-track">
                  <div
                    className="progress-fill"
                    style={{
                      width: `${Math.max(hostProgress.percent ?? 8, 8)}%`,
                    }}
                  />
                </div>
              </div>
            ) : null}
          </div>
        ) : null}

        {snapshot.warning ? <div className="warning">{snapshot.warning}</div> : null}

        <section className="list">
          {snapshot.plugins.map((plugin) => (
            <PluginCardView
              key={plugin.id}
              plugin={plugin}
              busy={busyId !== null}
              progress={
                installProgress?.id === plugin.id ? installProgress : null
              }
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
          <div className="proxy-settings">
            <div className="proxy-label-row">
              <span className="title">网络代理</span>
              <span
                className="proxy-hint"
                title={PROXY_SCOPE_HINT}
                aria-label={PROXY_SCOPE_HINT}
              >
                !
              </span>
            </div>
            <div className="proxy-modes" role="radiogroup" aria-label="网络代理">
              {(
                [
                  ["off", "不使用代理"],
                  ["system", "使用系统代理"],
                  ["custom", "使用自定义代理"],
                ] as const
              ).map(([mode, label]) => (
                <label key={mode} className="proxy-mode">
                  <input
                    type="radio"
                    name="proxy-mode"
                    value={mode}
                    checked={snapshot.proxyMode === mode}
                    disabled={busyId !== null}
                    onChange={() => void applyProxy(mode)}
                  />
                  {label}
                </label>
              ))}
            </div>
            {snapshot.proxyMode === "custom" ? (
              <div className="proxy-custom">
                <input
                  className="proxy-input"
                  type="text"
                  value={draftProxyUrl}
                  disabled={busyId !== null}
                  placeholder="例如 http://127.0.0.1:7890 或 socks5://127.0.0.1:1080"
                  onChange={(event) => setDraftProxyUrl(event.target.value)}
                  onBlur={() => {
                    if (draftProxyUrl.trim() !== snapshot.proxyUrl) {
                      void applyProxy("custom", draftProxyUrl);
                    }
                  }}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.currentTarget.blur();
                    }
                  }}
                />
              </div>
            ) : null}
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
  progress: InstallProgress | null;
  onInstall: () => void;
  onUninstall: () => void;
  onToggle: (enabled: boolean) => void;
  onCommand: (cmd: string) => void;
}) {
  const { plugin, busy, progress } = props;
  const installed = Boolean(plugin.installedVersion);
  const [brokenIcon, setBrokenIcon] = useState(false);
  const icon = pluginIconSrc(plugin);
  const installing = Boolean(progress);

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
            {progress?.message ?? plugin.statusMessage}
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
        {progress ? (
          <div className="progress-block">
            <div className="progress-track">
              <div
                className="progress-fill"
                style={{
                  width:
                    progress.percent == null
                      ? "35%"
                      : `${Math.max(progress.percent, 4)}%`,
                }}
              />
            </div>
          </div>
        ) : null}
      </div>

      <div className="card-actions">
        {!installed ? (
          <button
            className="btn primary"
            disabled={busy}
            onClick={props.onInstall}
          >
            {installing ? "安装中…" : "安装"}
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
              {installing
                ? "更新中…"
                : plugin.updateAvailable
                  ? "更新"
                  : "重装"}
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
