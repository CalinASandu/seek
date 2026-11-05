import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { CleanEvent, SeekApi, SeekEvent } from "./types";

export const EVENT_CHANNEL = "seek://event";
/** Duplicate-scan events of the Decimator (the Rust side arrives in a later task). */
export const CLEAN_EVENT_CHANNEL = "seek://clean";

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
    linkHub: (addr, code) => invoke("link_hub", { addr, code }),
    unlinkHub: () => invoke("unlink_hub"),
    hubStatus: () => invoke("hub_status"),
    listPeers: () => invoke("list_peers"),
    unlink: (hostId) => invoke("unlink", { hostId }),
    renamePeer: (hostId, name) => invoke("rename_peer", { hostId, name }),
    syncNow: () => invoke("sync_now"),
    setSharing: (enabled) => invoke("set_sharing", { enabled }),
    setDeviceShared: (deviceId, shared) => invoke("set_device_shared", { deviceId, shared }),
    spaceTree: (deviceId, path, depth) => invoke("space_tree", { deviceId, path, depth }),
    spaceLargest: (deviceId, path, limit) => invoke("space_largest", { deviceId, path, limit }),
    spaceTypes: (deviceId, path) => invoke("space_types", { deviceId, path }),
    // Decimator: the Rust commands arrive in later tasks. Until then each call rejects ("command not found"),
    // which the view shows as a plain error; nothing here runs at load.
    cleanScanStart: (deviceIds) => invoke("clean_scan_start", { deviceIds }),
    cleanScanCancel: () => invoke("clean_scan_cancel"),
    cleanScanState: () => invoke("clean_scan_state"),
    cleanLeftovers: () => invoke("clean_leftovers"),
    cleanApps: () => invoke("clean_apps"),
    cleanPrepare: (req) => invoke("clean_prepare", { req }),
    cleanCommit: (token, ids) => invoke("clean_commit", { token, ids }),
    cleanHistory: () => invoke("clean_history"),
    cleanPutBack: (entryIds) => invoke("clean_put_back", { entryIds }),
    uninstallStart: (appId) => invoke("uninstall_start", { appId }),
    uninstallState: (appId) => invoke("uninstall_state", { appId }),
    uninstallLeftovers: (appId) => invoke("uninstall_leftovers", { appId }),
    openRecycleBin: () => invoke("open_recycle_bin"),
    onCleanEvent(cb) {
      let disposed = false;
      let unlisten: (() => void) | null = null;
      void listen<CleanEvent>(CLEAN_EVENT_CHANNEL, (e) => cb(e.payload)).then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      });
      return () => {
        disposed = true;
        unlisten?.();
      };
    },
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
