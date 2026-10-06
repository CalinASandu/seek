// Mirrors the camelCase JSON in crates/seek-ipc/src/protocol.rs and crates/seek-core.
export type SortKey = "name" | "path" | "size" | "modified" | "device";
export type DeviceState = "online" | "offline" | "scanning" | "error";
export type DeviceKind = "fixed" | "removable" | "network";

export interface Device {
  id: string;
  displayName: string;
  label: string;
  filesystem: string;
  kind: DeviceKind;
  totalBytes: number;
  freeBytes: number;
  lastMountPoint: string;
  firstSeen: number;
  lastSeen: number;
  indexUpdatedAt: number;
  state: DeviceState;
  entryCount: number;
  error: string | null;
}

export interface ResultRow {
  name: string;
  folder: string;
  fullPath: string;
  size: number;
  modified: number;
  created: number;
  isDir: boolean;
  attrs: number;
  deviceId: string;
  deviceName: string;
  online: boolean;
}

export type SearchOutcome =
  | { ok: true; searchId: number; total: number; elapsedMs: number }
  | { ok: false; message: string; position: number };

export interface Status {
  serviceVersion: string;
  totalEntries: number;
  devicesOnline: number;
  devicesOffline: number;
  indexing: { deviceId: string; scanned: number; done: boolean }[];
}

export type ServiceState = "connected" | "connecting" | "notInstalled" | "error";

export interface Settings {
  startWithWindows: boolean;
  theme: "system" | "light" | "dark";
  excludeFolders: string[];
}

export type SeekEvent =
  | { type: "devicesChanged" }
  | { type: "resultsInvalidated" }
  | { type: "indexProgress"; deviceId: string; scanned: number; done: boolean }
  | { type: "serviceState"; state: ServiceState };

export interface SeekApi {
  search(query: string, sort: SortKey, descending: boolean): Promise<SearchOutcome>;
  fetchRows(searchId: number, offset: number, limit: number): Promise<ResultRow[]>;
  listDevices(): Promise<Device[]>;
  renameDevice(id: string, name: string): Promise<void>;
  forgetDevice(id: string): Promise<void>;
  rescanDevice(id: string): Promise<void>;
  addNetworkShare(unc: string): Promise<Device>;
  status(): Promise<Status>;
  openPath(path: string): Promise<void>;
  revealPath(path: string): Promise<void>;
  serviceState(): Promise<ServiceState>;
  installService(): Promise<void>;
  getSettings(): Promise<Settings>;
  setSettings(s: Settings): Promise<void>;
  onEvent(cb: (e: SeekEvent) => void): () => void;
}
