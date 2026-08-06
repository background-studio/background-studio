import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type PluginCard = {
  id: string;
  displayName: string;
  targetHint: string;
  enabled: boolean;
  installedVersion: string | null;
  latestVersion: string | null;
  latestAssetName: string | null;
  running: boolean;
  statusMessage: string;
  phase: string;
  pluginProtocol: number;
  updateAvailable: boolean;
  iconPath: string | null;
  iconWeb: string;
};

export type HostSnapshot = {
  plugins: PluginCard[];
  autoStartWithWindows: boolean;
  startMinimized: boolean;
  dataDirectory: string;
  warning: string | null;
  hostVersion: string;
  hostLatestVersion: string | null;
  hostUpdateAvailable: boolean;
  hostReleaseUrl: string | null;
};

export type InstallProgress = {
  id: string;
  phase: string;
  percent: number | null;
  message: string;
};

export type HostUpdateProgress = {
  phase: string;
  percent: number | null;
  message: string;
};

const SNAPSHOT_EVENT = "host:snapshot-changed";
const INSTALL_PROGRESS_EVENT = "host:install-progress";
const HOST_UPDATE_PROGRESS_EVENT = "host:host-update-progress";

export function pluginIconSrc(plugin: PluginCard): string {
  if (plugin.iconPath) {
    try {
      return convertFileSrc(plugin.iconPath);
    } catch {
      // fall through
    }
  }
  return plugin.iconWeb || `/plugins/${plugin.id}.png`;
}

export async function getSnapshot(): Promise<HostSnapshot> {
  return invoke("get_snapshot");
}

export async function refreshReleases(): Promise<HostSnapshot> {
  return invoke("refresh_releases");
}

export async function reloadCatalog(): Promise<HostSnapshot> {
  return invoke("reload_catalog");
}

export async function installPlugin(id: string): Promise<HostSnapshot> {
  return invoke("install_plugin", { id });
}

export async function uninstallPlugin(id: string): Promise<HostSnapshot> {
  return invoke("uninstall_plugin", { id });
}

export async function setPluginEnabled(id: string, enabled: boolean): Promise<HostSnapshot> {
  return invoke("set_plugin_enabled", { id, enabled });
}

export async function pluginCommand(id: string, cmd: string): Promise<HostSnapshot> {
  return invoke("plugin_command", { id, cmd });
}

export async function updateHostSettings(
  autoStartWithWindows: boolean,
  startMinimized: boolean,
): Promise<HostSnapshot> {
  return invoke("update_host_settings", {
    autoStartWithWindows,
    startMinimized,
  });
}

export async function openDataDirectory(): Promise<void> {
  return invoke("open_data_directory");
}

export async function chooseDataDirectory(): Promise<HostSnapshot> {
  return invoke("choose_data_directory");
}

export async function updateHost(): Promise<void> {
  return invoke("update_host");
}

export async function onSnapshot(
  handler: (snapshot: HostSnapshot) => void,
): Promise<UnlistenFn> {
  return listen<HostSnapshot>(SNAPSHOT_EVENT, (event) => handler(event.payload));
}

export async function onInstallProgress(
  handler: (progress: InstallProgress) => void,
): Promise<UnlistenFn> {
  return listen<InstallProgress>(INSTALL_PROGRESS_EVENT, (event) => handler(event.payload));
}

export async function onHostUpdateProgress(
  handler: (progress: HostUpdateProgress) => void,
): Promise<UnlistenFn> {
  return listen<HostUpdateProgress>(HOST_UPDATE_PROGRESS_EVENT, (event) =>
    handler(event.payload),
  );
}
