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
  /** Files and folders Cleanup never touches ("Always protect"); Rust: `protected_paths`, optional on the wire. */
  protectedPaths: string[];
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

/** This PC's link to a self-hosted hub. */
export interface HubStatus {
  name: string;
  /** The address the PC was linked with, e.g. `tls://seek-hub.your-tailnet.ts.net`. */
  addr: string;
  online: boolean;
  /** Unix ms of the last update exchanged with the hub; 0 = never. */
  lastSyncMs: number;
  /** Why the last attempt failed, or what the hub did (*The hub unlinked this PC*). */
  lastError: string | null;
  /** The other PCs of the hub. */
  pcs: PcSummary[];
  /** The hub removed this PC's link: only the notice is left (until dismissed), the PC is back in LAN mode. */
  unlinked?: boolean;
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

// ---- Decimator (Cleanup): mirrors crates/seek-clean/src/model.rs --------------------------------------------

/** Why an item is up for removal. */
export type Why = "duplicate" | "leftover" | "uninstall" | "staleData";

/** Why an item cannot be removed. */
export type Lock =
  | "systemFolder"
  | "programFiles"
  | "otherUser"
  | "appData"
  | "toolFolder"
  | "noRecycleBin"
  | "binFull"
  | "pathTooLong"
  | "cloudPlaceholder"
  | "protected"
  | "linkedPc"
  | "unsupportedPath"
  | "reparsePoint"
  | "claimed";

/** How sure Seek is that a leftover folder is dead. */
export type Level = "veryLikely" | "likely" | "notSure" | "probablyInUse";

/** One line of the "why we think so" list; positive points point to "left over". */
export interface Reason {
  text: string;
  points: number;
}

export interface DupCopy {
  path: string;
  deviceId: string;
  size: number;
  modified: number;
  lock: Lock | null;
  suggestedKeep: boolean;
}
export interface DupGroup {
  id: number;
  name: string;
  size: number;
  copies: DupCopy[];
  /** Space that can really be freed: removable copies, leaving one copy if none is locked. */
  freeable: number;
}
export interface OtherPcMatch {
  name: string;
  size: number;
  pcName: string;
  path: string;
}
export interface DupScanState {
  running: boolean;
  found: DupGroup[];
  otherPcs: OtherPcMatch[];
  checkedBytes: number;
  totalBytes: number;
  current: string | null;
  /** Something the scan left out or couldn't read, in plain words; kept after the scan ends. */
  notice?: string | null;
}
export type CleanEvent =
  | { type: "group"; group: DupGroup }
  | { type: "progress"; checkedBytes: number; totalBytes: number; current: string | null }
  | { type: "done" }
  /** The backend let go of the finished results after a long absence: nothing is left to show. */
  | { type: "dropped" }
  /** The whole notice text, sent as soon as it is raised. */
  | { type: "notice"; text: string };

export interface LeftoverRow {
  path: string;
  parent: string;
  size: number;
  modified: number;
  needsAdmin: boolean;
  level: Level;
  reasons: Reason[];
  contents: string[];
}
export interface AppRow {
  id: string;
  name: string;
  publisher: string;
  version: string;
  size: number;
  installDate: number | null;
  icon: string | null;
  folder: string | null;
  /** True when the folder did not come from the app's InstallLocation: Seek's best guess. */
  folderGuessed: boolean;
  /** False when Seek can't tell what to run to uninstall this app. */
  canUninstall: boolean;
  note: string | null;
  running: boolean;
}

export type PrepareRequest =
  | { kind: "duplicates"; groups: { path: string; deviceId: string }[][] }
  | { kind: "folders"; why: Why; folders: { path: string; level: Level | null; reasons: Reason[] }[]; appId?: string }
  | { kind: "stale"; files: string[] };

export interface ReviewItem {
  id: number;
  path: string;
  size: number;
  modifiedMs: number;
  isDir: boolean;
  files: number;
  why: Why;
  lock: Lock | null;
  needsAdmin: boolean;
  group: number | null;
  level: Level | null;
  reasons: Reason[];
}
export interface Prepared {
  token: string;
  items: ReviewItem[];
}

/** Tagged by `kind`, like the Rust enum. */
export type Outcome = { kind: "moved"; binPath: string } | { kind: "left"; reason: string };
export interface ItemOutcome {
  id: number;
  path: string;
  size: number;
  outcome: Outcome;
}
export interface CommitResult {
  items: ItemOutcome[];
  movedBytes: number;
  stoppedEarly: string | null;
}

/** One past removal (Task 16 owns the Rust side; this is the shape the UI needs). */
export interface HistoryEntry {
  id: string;
  timeMs: number;
  originalPath: string;
  size: number;
  why: Why;
  /** Where it went in the Recycle Bin; null when it was left in place. */
  binPath: string | null;
  /** Why an item was not moved; null when it was. */
  leftReason: string | null;
}
export interface PutBackResult {
  id: string;
  restored: boolean;
  /** Plain-words reason when it could not be put back. */
  reason: string | null;
}

export type UninstallState = "running" | "finished" | "unresolved";

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
  /** Rejects when the settings could not be saved (for instance an unreadable settings file that must not be overwritten). */
  setSettings(s: Settings): Promise<void>;
  /** Why the saved settings, and so the protected list, can't be used; null when they can. */
  settingsProblem(): Promise<string | null>;
  startPairing(): Promise<PairingCode>;
  discoveredPcs(): Promise<PcInfo[]>;
  joinPairing(addr: string, code: string): Promise<void>;
  /** Links to the hub at `addr` showing `code` (8 digits); resolves with the hub's name. */
  linkHub(addr: string, code: string): Promise<string>;
  /** Unlinks from the hub, or dismisses the *hub unlinked this PC* notice. */
  unlinkHub(): Promise<void>;
  /** `null` when this PC is not linked to a hub. */
  hubStatus(): Promise<HubStatus | null>;
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

  // ---- Decimator (Cleanup) ----
  cleanScanStart(deviceIds: string[]): Promise<void>;
  cleanScanCancel(): Promise<void>;
  cleanScanState(): Promise<DupScanState>;
  onCleanEvent(cb: (e: CleanEvent) => void): () => void;
  cleanLeftovers(): Promise<LeftoverRow[]>;
  cleanApps(): Promise<AppRow[]>;
  cleanPrepare(req: PrepareRequest): Promise<Prepared>;
  cleanCommit(token: string, ids: number[]): Promise<CommitResult>;
  cleanHistory(): Promise<HistoryEntry[]>;
  cleanPutBack(entryIds: string[]): Promise<PutBackResult[]>;
  uninstallStart(appId: string): Promise<void>;
  uninstallState(appId: string): Promise<UninstallState>;
  uninstallLeftovers(appId: string): Promise<LeftoverRow[]>;
  openRecycleBin(): Promise<void>;
}
