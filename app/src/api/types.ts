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
  /** Host id of the PC that indexes this device (this PC, or a linked PC for copies). */
  indexedByHost: string;
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

/** This PC or a linked PC. */
export interface PcSummary {
  hostId: string;
  name: string;
  online: boolean;
  /** Unix ms of the last update exchanged; 0 = never. */
  lastSyncMs: number;
  /** Came back with a different identity (reinstalled): must be linked again. */
  needsRelink: boolean;
}

/** A Seek PC on the local network that is not linked yet. */
export interface PcInfo {
  hostId: string;
  name: string;
  /** `ip:port` */
  addr: string;
}

export interface Peers {
  thisPc: PcSummary;
  peers: PcSummary[];
  /** "Share this PC's files with linked PCs". */
  sharing: boolean;
  /** Own devices not shared. */
  unshared: string[];
}

export interface PairingCode {
  code: string;
  expiresMs: number;
}

export type SeekEvent =
  | { type: "devicesChanged" }
  | { type: "peersChanged" }
  | { type: "resultsInvalidated" }
  | { type: "indexProgress"; deviceId: string; scanned: number; done: boolean }
  | { type: "serviceState"; state: ServiceState };

/** `smaller`: that many items too small to show, merged; `hidden`: other users' folders. */
export type SpaceKind = "folder" | "file" | "hidden" | { smaller: { count: number } };

export interface SpaceNode {
  name: string;
  kind: SpaceKind;
  bytes: number;
  files: number;
  modified: number;
  /** Holds cloud-only files (OneDrive / Google Drive placeholders). */
  cloud: boolean;
  children: SpaceNode[];
}

export type TypeCategory = "video" | "pictures" | "music" | "documents" | "archives" | "apps" | "system" | "other";

export interface TypeTotal {
  category: TypeCategory;
  bytes: number;
  files: number;
}

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
  startPairing(): Promise<PairingCode>;
  discoveredPcs(): Promise<PcInfo[]>;
  joinPairing(addr: string, code: string): Promise<void>;
  listPeers(): Promise<Peers>;
  unlink(hostId: string): Promise<void>;
  renamePeer(hostId: string, name: string): Promise<void>;
  syncNow(): Promise<void>;
  setSharing(enabled: boolean): Promise<void>;
  setDeviceShared(deviceId: string, shared: boolean): Promise<void>;
  /** What fills `path` (a folder's full path; a drive root is its mount point, e.g. `D:\`). */
  spaceTree(deviceId: string, path: string, depth: number): Promise<SpaceNode>;
  spaceLargest(deviceId: string, path: string, limit: number): Promise<ResultRow[]>;
  spaceTypes(deviceId: string, path: string): Promise<TypeTotal[]>;
  onEvent(cb: (e: SeekEvent) => void): () => void;
}
