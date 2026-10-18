import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { SeekApi, SeekEvent } from "./types";

export const EVENT_CHANNEL = "seek://event";

export function createTauriApi(): SeekApi {
  return {
    search: (query, sort, descending) => invoke("search", { query, sort, descending }),
    fetchRows: (searchId, offset, limit) => invoke("fetch_rows", { searchId, offset, limit }),
    listDevices: () => invoke("list_devices"),
    renameDevice: (id, name) => invoke("rename_device", { id, name }),
    forgetDevice: (id) => invoke("forget_device", { id }),
    rescanDevice: (id) => invoke("rescan_device", { id }),
    addNetworkShare: (unc) => invoke("add_network_share", { unc }),
    status: () => invoke("status"),
    openPath: (path) => invoke("open_path", { path }),
    revealPath: (path) => invoke("reveal_path", { path }),
    serviceState: () => invoke("service_state"),
    installService: () => invoke("install_service"),
    getSettings: () => invoke("get_settings"),
    setSettings: (settings) => invoke("set_settings", { settings }),
    startPairing: () => invoke("start_pairing"),
    discoveredPcs: () => invoke("discovered_pcs"),
    joinPairing: (addr, code) => invoke("join_pairing", { addr, code }),
    listPeers: () => invoke("list_peers"),
    unlink: (hostId) => invoke("unlink", { hostId }),
    renamePeer: (hostId, name) => invoke("rename_peer", { hostId, name }),
    syncNow: () => invoke("sync_now"),
    setSharing: (enabled) => invoke("set_sharing", { enabled }),
    setDeviceShared: (deviceId, shared) => invoke("set_device_shared", { deviceId, shared }),
    spaceTree: (deviceId, path, depth) => invoke("space_tree", { deviceId, path, depth }),
    spaceLargest: (deviceId, path, limit) => invoke("space_largest", { deviceId, path, limit }),
    spaceTypes: (deviceId, path) => invoke("space_types", { deviceId, path }),
    onEvent(cb) {
      let disposed = false;
      let unlisten: (() => void) | null = null;
      void listen<SeekEvent>(EVENT_CHANNEL, (e) => cb(e.payload)).then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      });
      return () => {
        disposed = true;
        unlisten?.();
      };
    },
  };
}
