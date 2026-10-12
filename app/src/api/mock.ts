import type {
  Device,
  PairingCode,
  PcInfo,
  PcSummary,
  Peers,
  ResultRow,
  SearchOutcome,
  SeekApi,
  SeekEvent,
  ServiceState,
  Settings,
  SortKey,
  Status,
} from "./types";

export interface MockOptions {
  /** Number of synthetic rows (default 300k). */
  rowCount?: number;
  /** Artificial latency range in ms (default 5..20). */
  latency?: [number, number];
  /** Initial service state; defaults to the `?state=` URL param or "connected". */
  state?: ServiceState;
  /** Emit fake indexing progress for the scanning device. */
  simulate?: boolean;
}

const DAY = 86_400_000; // timestamps are unix epoch milliseconds, like the service
export const MOCK_THIS_PC = "host-desktop";
export const MOCK_LAPTOP = "host-laptop";
/** The code the mock's other PCs "show"; any other code is wrong. */
export const MOCK_PAIRING_CODE = "482913";
const GB = 1024 ** 3;

function mulberry32(seed: number): () => number {
  let a = seed;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const WORDS = [
  "report", "invoice", "budget", "summary", "notes", "draft", "final", "backup", "photo", "holiday",
  "meeting", "project", "design", "proposal", "contract", "resume", "thesis", "lecture", "slides", "assignment",
  "screenshot", "wallpaper", "recording", "podcast", "album", "setup", "installer", "config", "readme", "changelog",
  "archive", "export", "import", "data", "analysis", "survey", "results", "schedule", "timeline", "roadmap",
  "tax", "receipt", "statement", "portfolio", "sketch", "mockup", "icon", "logo", "banner", "template",
];
const SUFFIX = ["", "", "", "-v2", "-final", "_old", " (1)", " copy", "-2024", "-2025", "_draft", "-new"];
const EXT_POOL: [string, number][] = [
  ["pdf", 14], ["docx", 9], ["xlsx", 5], ["pptx", 3], ["txt", 6], ["md", 3], ["jpg", 14], ["png", 10],
  ["mp4", 3], ["mp3", 4], ["zip", 3], ["7z", 1], ["exe", 2], ["dll", 6], ["ts", 5], ["tsx", 3], ["rs", 4],
  ["json", 4], ["csv", 3], ["log", 2], ["iso", 1], ["", 3],
];
const FOLDERS: Record<string, string[]> = {
  c: [
    "Users\\Calin\\Documents", "Users\\Calin\\Downloads", "Users\\Calin\\Desktop", "Users\\Calin\\Pictures\\Screenshots",
    "Program Files\\Common Files", "Windows\\System32", "Users\\Calin\\AppData\\Local\\Temp",
    "Users\\Calin\\Documents\\Work\\Clients",
  ],
  d: [
    "Uni\\Sem2\\Lectures", "Uni\\Sem1", "Programming\\seek\\docs", "Programming\\web\\src\\components", "Games\\Saves",
    "Projects\\Archive\\2023", "Media\\Music\\Albums", "Uni\\Thesis\\Drafts",
  ],
  backup: ["Old\\Documents", "Old\\Pictures\\2019", "Laptop-2021\\Desktop", "Laptop-2021\\Documents\\Taxes", "Archive\\Finance"],
  usb: ["DCIM\\100CANON", "DCIM\\101CANON", "Exports\\Wedding", "Camera Roll"],
  nas: ["media\\Movies", "media\\Music", "media\\Photos\\2024", "shared\\Documents", "backups\\phone"],
  laptop: ["Users\\Calin\\Documents\\Uni", "Users\\Calin\\Downloads", "Users\\Calin\\Desktop", "Projects\\travel"],
};

interface DeviceSpec {
  device: Device;
  folderKey: string;
  weight: number;
  mount: string;
}

function makeDevices(now: number): DeviceSpec[] {
  const base = { firstSeen: now - 400 * DAY, error: null, indexedByHost: MOCK_THIS_PC };
  const laptop = { ...base, indexedByHost: MOCK_LAPTOP };
  return [
    {
      folderKey: "c", weight: 0.26, mount: "C:",
      device: {
        ...base, id: "dev-c", displayName: "System (C:)", label: "System", filesystem: "NTFS", kind: "fixed",
        totalBytes: 476 * GB, freeBytes: 118 * GB, lastMountPoint: "C:\\", lastSeen: now, indexUpdatedAt: now - 120_000,
        state: "online", entryCount: 0,
      },
    },
    {
      folderKey: "d", weight: 0.3, mount: "D:",
      device: {
        ...base, id: "dev-d", displayName: "Data (D:)", label: "Data", filesystem: "NTFS", kind: "fixed",
        totalBytes: 1863 * GB, freeBytes: 412 * GB, lastMountPoint: "D:\\", lastSeen: now, indexUpdatedAt: now - 30_000,
        state: "scanning", entryCount: 0,
      },
    },
    {
      folderKey: "backup", weight: 0.2, mount: "E:",
      device: {
        ...base, id: "dev-backup", displayName: "Backup 2TB", label: "Backup", filesystem: "exFAT", kind: "removable",
        totalBytes: 1863 * GB, freeBytes: 301 * GB, lastMountPoint: "E:\\", lastSeen: now - 3 * DAY,
        indexUpdatedAt: now - 3 * DAY, state: "offline", entryCount: 0,
      },
    },
    {
      folderKey: "usb", weight: 0.04, mount: "F:",
      device: {
        ...base, id: "dev-usb", displayName: "Camera SD", label: "CANON", filesystem: "FAT32", kind: "removable",
        totalBytes: 59 * GB, freeBytes: 8 * GB, lastMountPoint: "F:\\", lastSeen: now - 41 * DAY,
        indexUpdatedAt: now - 41 * DAY, state: "offline", entryCount: 0,
      },
    },
    {
      folderKey: "nas", weight: 0.08, mount: "\\\\nas\\media",
      device: {
        ...base, id: "dev-nas", displayName: "NAS media", label: "media", filesystem: "SMB", kind: "network",
        totalBytes: 7452 * GB, freeBytes: 3120 * GB, lastMountPoint: "\\\\nas\\media", lastSeen: now,
        indexUpdatedAt: now - 900_000, state: "online", entryCount: 0,
      },
    },
    {
      folderKey: "laptop", weight: 0.08, mount: "C:",
      device: {
        ...laptop, id: "dev-laptop-c", displayName: "Laptop SSD (C:)", label: "OS", filesystem: "NTFS", kind: "fixed",
        totalBytes: 953 * GB, freeBytes: 210 * GB, lastMountPoint: "C:\\", lastSeen: now, indexUpdatedAt: now - 20_000,
        state: "online", entryCount: 0,
      },
    },
    {
      folderKey: "laptop", weight: 0.04, mount: "D:",
      device: {
        ...laptop, id: "dev-laptop-usb", displayName: "Travel stick", label: "TRAVEL", filesystem: "exFAT",
        kind: "removable", totalBytes: 119 * GB, freeBytes: 40 * GB, lastMountPoint: "D:\\", lastSeen: now - 9 * DAY,
        indexUpdatedAt: now - 9 * DAY, state: "offline", entryCount: 0,
      },
    },
  ];
}

interface Store {
  deviceIdx: Uint8Array;
  names: string[];
  lower: string[];
  exts: string[];
  folders: string[];
  isDir: Uint8Array;
  sizes: Float64Array;
  modified: Float64Array;
}

function generate(count: number, specs: DeviceSpec[], now: number): Store {
  const rnd = mulberry32(0x5eec);
  const total = EXT_POOL.reduce((s, [, w]) => s + w, 0);
  const pickExt = (): string => {
    let r = rnd() * total;
    for (const [e, w] of EXT_POOL) {
      r -= w;
      if (r <= 0) return e;
    }
    return "";
  };
  const store: Store = {
    deviceIdx: new Uint8Array(count),
    names: new Array<string>(count),
    lower: new Array<string>(count),
    exts: new Array<string>(count),
    folders: new Array<string>(count),
    isDir: new Uint8Array(count),
    sizes: new Float64Array(count),
    modified: new Float64Array(count),
  };
  const counts = specs.map(() => 0);
  for (let i = 0; i < count; i++) {
    let r = rnd();
    let d = 0;
    for (; d < specs.length - 1; d++) {
      const w = specs[d]!.weight;
      if (r < w) break;
      r -= w;
    }
    counts[d]!++;
    const spec = specs[d]!;
    const dir = rnd() < 0.08;
    const w1 = WORDS[(rnd() * WORDS.length) | 0]!;
    const w2 = WORDS[(rnd() * WORDS.length) | 0]!;
    const ext = dir ? "" : pickExt();
    const stem = rnd() < 0.5 ? w1 : `${w1}-${w2}`;
    const name = `${stem}${SUFFIX[(rnd() * SUFFIX.length) | 0]!}${ext ? "." + ext : ""}`;
    const tails = FOLDERS[spec.folderKey]!;
    const folderTail = tails[(rnd() * tails.length) | 0]!;
    store.deviceIdx[i] = d;
    store.names[i] = name;
    store.lower[i] = name.toLowerCase();
    store.exts[i] = ext;
    store.folders[i] = `${spec.mount}\\${folderTail}`;
    store.isDir[i] = dir ? 1 : 0;
    store.sizes[i] = dir ? 0 : Math.floor(Math.exp(rnd() * 16) * 40);
    const recent = rnd() < 0.12;
    store.modified[i] = Math.floor(now - (recent ? rnd() * 6 * DAY : 7 * DAY + rnd() * 1000 * DAY));
  }
  specs.forEach((s, i) => (s.device.entryCount = counts[i]!));
  return store;
}

// ---- tiny query matcher ---------------------------------------------------

interface ParsedQuery {
  terms: string[];
  exts: Set<string> | null;
  devices: string[];
  drives: string[];
  pcs: string[];
  kind: "file" | "folder" | null;
  online: boolean | null;
  regex: RegExp | null;
}

type Parsed = { ok: true; q: ParsedQuery } | { ok: false; message: string; position: number };

function parseQuery(query: string): Parsed {
  const q: ParsedQuery = { terms: [], exts: null, devices: [], drives: [], pcs: [], kind: null, online: null, regex: null };
  let i = 0;
  while (i < query.length) {
    while (i < query.length && /\s/.test(query[i]!)) i++;
    if (i >= query.length) break;
    const start = i;
    let tok = "";
    while (i < query.length && !/\s/.test(query[i]!)) {
      if (query[i] === '"') {
        const close = query.indexOf('"', i + 1);
        if (close < 0) return { ok: false, message: "Unterminated quote", position: i };
        tok += query.slice(i + 1, close);
        i = close + 1;
      } else tok += query[i++];
    }
    const m = /^([a-z]+):(.*)$/i.exec(tok);
    const key = m?.[1]?.toLowerCase();
    const val = m?.[2] ?? "";
    if (key === "ext") q.exts = new Set(val.toLowerCase().split(";").filter(Boolean));
    else if (key === "device") q.devices.push(val.toLowerCase());
    else if (key === "drive") q.drives.push(val.toLowerCase());
    else if (key === "pc") {
      if (!val) return { ok: false, message: "pc: needs a name", position: start };
      q.pcs.push(val.toLowerCase());
    }
    else if (key === "file") q.kind = "file";
    else if (key === "folder") q.kind = "folder";
    else if (key === "online") q.online = true;
    else if (key === "offline") q.online = false;
    else if (key === "regex") {
      try {
        q.regex = new RegExp(val, "i");
      } catch {
        return { ok: false, message: "Invalid regular expression", position: start + 6 };
      }
    } else if (key && !["size", "dm", "path"].includes(key)) {
      return { ok: false, message: `Unknown filter "${key}:"`, position: start };
    } else if (tok) q.terms.push(tok.toLowerCase());
  }
  return { ok: true, q };
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

export function createMockApi(opts: MockOptions = {}): SeekApi {
  const now = Date.now();
  const specs = makeDevices(now);
  const store = generate(opts.rowCount ?? 300_000, specs, now);
  const [latMin, latMax] = opts.latency ?? [5, 20];
  const delay = () => (latMax > 0 ? sleep(latMin + Math.random() * (latMax - latMin)) : Promise.resolve());

  const urlState =
    typeof location !== "undefined" ? (new URLSearchParams(location.search).get("state") as ServiceState | null) : null;
  let serviceState: ServiceState = opts.state ?? urlState ?? "connected";
  let settings: Settings = {
    startWithWindows: true,
    theme: "system",
    excludeFolders: ["C:\\Windows\\WinSxS", "C:\\$Recycle.Bin"],
  };
  const listeners = new Set<(e: SeekEvent) => void>();
  const emit = (e: SeekEvent) => listeners.forEach((l) => l(e));

  let nextId = 1;
  const searches = new Map<number, Int32Array>();
  let scanned = 48_210;
  let timer: ReturnType<typeof setInterval> | null = null;

  const specOf = (id: string) => specs.find((s) => s.device.id === id);

  // ---- linked PCs ----
  const thisPc: PcSummary = { hostId: MOCK_THIS_PC, name: "DESKTOP", online: true, lastSyncMs: now - 20_000, needsRelink: false };
  let peers: PcSummary[] = [
    { hostId: MOCK_LAPTOP, name: "Laptop", online: true, lastSyncMs: now - 20_000, needsRelink: false },
  ];
  let sharing = true;
  let unshared: string[] = [];
  const discovered: PcInfo[] = [{ hostId: "host-office", name: "Office-PC", addr: "192.168.1.23:47321" }];
  /** Devices of PCs that were unlinked (rows stay in the store, so devices are hidden rather than spliced). */
  const gone = new Set<string>();
  const visible = () => specs.filter((s) => !gone.has(s.device.id));
  const pcName = (host: string) => (host === MOCK_THIS_PC ? thisPc.name : peers.find((p) => p.hostId === host)?.name ?? "");

  function ensureTimer() {
    if (!opts.simulate || timer) return;
    timer = setInterval(() => {
      scanned += 1380;
      emit({ type: "indexProgress", deviceId: "dev-d", scanned, done: false });
    }, 500);
  }

  function rowAt(i: number): ResultRow {
    const spec = specs[store.deviceIdx[i]!]!;
    const folder = store.folders[i]!;
    const name = store.names[i]!;
    return {
      name,
      folder,
      fullPath: `${folder}\\${name}`,
      size: store.sizes[i]!,
      modified: store.modified[i]!,
      created: store.modified[i]! - 3 * DAY,
      isDir: store.isDir[i] === 1,
      attrs: 0,
      deviceId: spec.device.id,
      deviceName: spec.device.displayName,
      online: spec.device.state !== "offline",
    };
  }

  const cmpStr = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);
  const cmp = (sort: SortKey): ((a: number, b: number) => number) => {
    switch (sort) {
      case "name": return (a, b) => cmpStr(store.lower[a]!, store.lower[b]!);
      case "path": return (a, b) => cmpStr(store.folders[a]!, store.folders[b]!);
      case "size": return (a, b) => store.sizes[a]! - store.sizes[b]!;
      case "modified": return (a, b) => store.modified[a]! - store.modified[b]!;
      case "device": return (a, b) => store.deviceIdx[a]! - store.deviceIdx[b]!;
    }
  };

  return {
    async search(query, sort, descending): Promise<SearchOutcome> {
      await delay();
      const t0 = performance.now();
      const parsed = parseQuery(query);
      if (!parsed.ok) return parsed;
      const q = parsed.q;
      const deviceOk = specs.map((s) => {
        if (gone.has(s.device.id)) return false;
        if (q.online !== null && (s.device.state !== "offline") !== q.online) return false;
        const pc = pcName(s.device.indexedByHost).toLowerCase();
        const dev = s.device.displayName.toLowerCase();
        if (q.devices.length && !q.devices.some((d) => dev.includes(d) || pc.includes(d))) return false;
        if (q.drives.length && !q.drives.some((d) => dev.includes(d))) return false;
        if (q.pcs.length && !q.pcs.some((p) => pc.includes(p))) return false;
        return true;
      });
      const hits: number[] = [];
      const n = store.names.length;
      for (let i = 0; i < n; i++) {
        if (!deviceOk[store.deviceIdx[i]!]) continue;
        if (q.kind && (store.isDir[i] === 1) !== (q.kind === "folder")) continue;
        if (q.exts && !q.exts.has(store.exts[i]!)) continue;
        const lower = store.lower[i]!;
        let ok = true;
        for (const t of q.terms) {
          if (!lower.includes(t)) {
            ok = false;
            break;
          }
        }
        if (ok && q.regex && !q.regex.test(store.names[i]!)) ok = false;
        if (ok) hits.push(i);
      }
      const order = Int32Array.from(hits);
      order.sort(cmp(sort));
      if (descending) order.reverse();
      const searchId = nextId++;
      searches.set(searchId, order);
      searches.delete(searchId - 6);
      return { ok: true, searchId, total: order.length, elapsedMs: Math.max(0.1, +(performance.now() - t0).toFixed(1)) };
    },
    async fetchRows(searchId, offset, limit) {
      await delay();
      const order = searches.get(searchId);
      if (!order) return [];
      return Array.from(order.subarray(offset, offset + limit), rowAt);
    },
    async listDevices() {
      await delay();
      return visible().map((s) => ({ ...s.device }));
    },
    async renameDevice(id, name) {
      await delay();
      const s = specOf(id);
      if (s) s.device.displayName = name;
      emit({ type: "devicesChanged" });
    },
    async forgetDevice(id) {
      await delay();
      const i = specs.findIndex((s) => s.device.id === id);
      if (i >= 0) specs.splice(i, 1);
      emit({ type: "devicesChanged" });
    },
    async rescanDevice(id) {
      await delay();
      const s = specOf(id);
      if (s && s.device.state !== "offline") s.device.state = "scanning";
      emit({ type: "devicesChanged" });
    },
    async addNetworkShare(unc) {
      await delay();
      const parts = unc.split("\\").filter(Boolean);
      const device: Device = {
        id: `dev-${Date.now()}`, displayName: parts.join(" / "), label: parts[1] ?? "", filesystem: "SMB", kind: "network",
        indexedByHost: MOCK_THIS_PC,
        totalBytes: 2000 * GB, freeBytes: 1500 * GB, lastMountPoint: unc, firstSeen: now, lastSeen: now,
        indexUpdatedAt: now, state: "scanning", entryCount: 0, error: null,
      };
      specs.push({ device, folderKey: "nas", weight: 0, mount: unc });
      emit({ type: "devicesChanged" });
      return { ...device };
    },
    async status(): Promise<Status> {
      await delay();
      const shown = visible();
      const offline = shown.filter((s) => s.device.state === "offline").length;
      return {
        serviceVersion: "0.1.0",
        totalEntries: shown.reduce((n, s) => n + s.device.entryCount, 0),
        devicesOnline: shown.length - offline,
        devicesOffline: offline,
        indexing: specs
          .filter((s) => s.device.state === "scanning")
          .map((s) => ({ deviceId: s.device.id, scanned, done: false })),
      };
    },
    async openPath() {
      await delay();
    },
    async revealPath() {
      await delay();
    },
    async serviceState() {
      await delay();
      return serviceState;
    },
    async installService() {
      const fast = latMax === 0;
      await sleep(fast ? 0 : 900);
      serviceState = "connecting";
      emit({ type: "serviceState", state: "connecting" });
      await sleep(fast ? 0 : 1400);
      serviceState = "connected";
      emit({ type: "serviceState", state: "connected" });
    },
    async getSettings() {
      await delay();
      return { ...settings, excludeFolders: [...settings.excludeFolders] };
    },
    async setSettings(s) {
      await delay();
      settings = s;
    },
    async startPairing(): Promise<PairingCode> {
      await delay();
      return { code: MOCK_PAIRING_CODE, expiresMs: Date.now() + 120_000 };
    },
    async discoveredPcs() {
      await delay();
      return discovered.filter((d) => !peers.some((p) => p.hostId === d.hostId)).map((d) => ({ ...d }));
    },
    async joinPairing(addr, code) {
      await delay();
      if (code.replace(/\D/g, "") !== MOCK_PAIRING_CODE) throw new Error("wrong code");
      if (peers.length + 1 >= 5) throw new Error("that device's group is full (5 devices)");
      const found = discovered.find((d) => d.addr === addr.trim());
      const hostId = found?.hostId ?? `host-${Date.now()}`;
      peers = [...peers, { hostId, name: found?.name ?? addr.split(":")[0]!, online: true, lastSyncMs: Date.now(), needsRelink: false }];
      emit({ type: "peersChanged" });
    },
    async listPeers(): Promise<Peers> {
      await delay();
      return { thisPc: { ...thisPc }, peers: peers.map((p) => ({ ...p })), sharing, unshared: [...unshared] };
    },
    async unlink(hostId) {
      await delay();
      peers = peers.filter((p) => p.hostId !== hostId);
      specs.filter((s) => s.device.indexedByHost === hostId).forEach((s) => gone.add(s.device.id));
      emit({ type: "peersChanged" });
      emit({ type: "devicesChanged" });
      emit({ type: "resultsInvalidated" });
    },
    async renamePeer(hostId, name) {
      await delay();
      peers = peers.map((p) => (p.hostId === hostId ? { ...p, name } : p));
      emit({ type: "peersChanged" });
    },
    async syncNow() {
      await delay();
      const t = Date.now();
      thisPc.lastSyncMs = t;
      peers = peers.map((p) => (p.online ? { ...p, lastSyncMs: t } : p));
      emit({ type: "peersChanged" });
    },
    async setSharing(enabled) {
      await delay();
      sharing = enabled;
      emit({ type: "peersChanged" });
    },
    async setDeviceShared(deviceId, shared) {
      await delay();
      unshared = shared ? unshared.filter((d) => d !== deviceId) : [...unshared.filter((d) => d !== deviceId), deviceId];
      emit({ type: "peersChanged" });
    },
    onEvent(cb) {
      listeners.add(cb);
      ensureTimer();
      return () => {
        listeners.delete(cb);
        if (listeners.size === 0 && timer) {
          clearInterval(timer);
          timer = null;
        }
      };
    },
  };
}
