// Realistic folder trees for the mock API's Space view (browser preview and tests).
// Mirrors the service's rules: crates/seek-core/src/space.rs.
import type { ResultRow, SpaceNode, TypeCategory, TypeTotal } from "./types";

const GB = 1024 ** 3;
const DAY = 86_400_000;
const MAX_LISTED = 500;
const MIN_SHARE = 0.005;

/** A number is a folder's content in GB (or one file's size when the name has an extension). */
type Spec = { [name: string]: number | Spec };

const SPECS: Record<string, Spec> = {
  "dev-c": {
    Windows: { System32: 9.2, WinSxS: 10, Installer: 6.3, SysWOW64: 3.1, Fonts: 0.6, Temp: 1.4 },
    "Program Files": {
      Adobe: { "Adobe Photoshop 2025": 5.4, "Adobe Premiere Pro 2025": 6.1 },
      "Microsoft Office": 4.2,
      JetBrains: { "RustRover 2025.2": 3.1, "WebStorm 2025.2": 2.8 },
      "NVIDIA Corporation": 2.4,
      "Common Files": 1.2,
    },
    "Program Files (x86)": {
      Steam: {
        steamapps: {
          common: { "Baldur's Gate 3": 56, "Cyberpunk 2077": 44, "Counter-Strike 2": 24, "Hades II": 9.6 },
          shadercache: 3.4,
        },
      },
      Microsoft: 2.1,
    },
    Users: {
      Calin: {
        Documents: { Work: 6.2, Uni: 3.4, Scans: 1.1 },
        Downloads: 17,
        Desktop: 3.9,
        Pictures: { Screenshots: 2.2, Camera: 9.8 },
        Videos: { Captures: 8 },
        Music: 7.4,
        AppData: {
          Local: { Temp: 7.1, Packages: 9.3, Google: { Chrome: 4.8 }, Docker: { wsl: { "ext4.vhdx": 7 } } },
          Roaming: { Discord: 1.1, Code: 0.9 },
        },
        OneDrive: { Documents: 3.1, Pictures: 2.7 },
      },
      Public: 0.4,
    },
    ProgramData: { Microsoft: 3.6, NVIDIA: 1.2, Package_Cache: 1.6 },
    "pagefile.sys": 16,
    "hiberfil.sys": 12.8,
    "swapfile.sys": 0.25,
  },
  "dev-d": {
    Media: {
      Movies: { "4K": 260, HD: 92 },
      Series: { "Severance S02": 38, "The Bear S03": 22 },
      Music: { Albums: 61, Lossless: 48 },
      Photos: { "2023": 64, "2024": 92, "2025": 51 },
    },
    Games: { SteamLibrary: { steamapps: { common: { "Starfield": 128, "Forza Horizon 5": 103 } } }, Saves: 1.4 },
    VMs: { "Ubuntu-24.04.vhdx": 64, "Win11-dev.vhdx": 80 },
    Backups: { "Phone 2024": 128, "Phone 2025": 74 },
    Programming: { seek: { target: 18, app: 1.2, docs: 0.1 }, web: 4.3, "rust-playground": 2.2 },
    Projects: { Video: 31, Design: 8.2, Archive: 12 },
    Uni: { Sem1: 2.8, Sem2: 6.1, Thesis: 1.9 },
  },
  "dev-backup": {
    Old: { Documents: 60, Pictures: { "2018": 140, "2019": 260 } },
    "Laptop-2021": { Desktop: 22, Documents: 81, Downloads: 197 },
    Archive: { Finance: 4.1, Mail: 9.4 },
    "Photos-2019.zip": 48,
  },
  "dev-usb": { DCIM: { "100CANON": 18.2, "101CANON": 21.7 }, Exports: { Wedding: 9.1 }, "Camera Roll": 1.6 },
  "dev-nas": {
    Movies: 2200,
    Music: 540,
    Photos: { "2022": 280, "2023": 310, "2024": 330 },
    shared: { Documents: 90, Software: 30 },
    backups: { phone: 300 },
  },
  "dev-laptop-c": {
    Windows: { System32: 8.1, WinSxS: 11.4, Installer: 4.2 },
    "Program Files": { "Microsoft Office": 4.1, Zoom: 0.6, Spotify: 0.8 },
    Users: {
      Calin: {
        Documents: { Uni: 12.3, CV: 0.2 },
        Downloads: 30,
        Desktop: 2.9,
        AppData: { Local: 22, Roaming: 4.1 },
        Pictures: 18,
      },
    },
    Projects: { travel: 8.2 },
    "pagefile.sys": 8,
  },
  "dev-laptop-usb": { Photos: { Japan: 21, Italy: 17 }, Docs: 2.1, "installers.zip": 3.4 },
};

/** Other users' folders the caller may not see, by device: (parent path, bytes). */
const HIDDEN: Record<string, [string, number][]> = { "dev-c": [["Users", Math.round(9 * GB)]] };

const EXT_BY_HINT: [RegExp, string[]][] = [
  [/movie|series|video|captures|4k|^hd$/i, ["mkv", "mp4", "mov"]],
  [/photo|picture|camera|dcim|canon|screenshot|japan|italy|20\d\d|wedding/i, ["jpg", "heic", "png", "cr3"]],
  [/music|album|lossless/i, ["flac", "mp3", "m4a"]],
  [/steam|game|common|starfield|forza|baldur|cyberpunk|counter|hades/i, ["pak", "dll", "exe", "bin"]],
  [/program|adobe|office|jetbrains|nvidia|microsoft|zoom|spotify|rustrover|webstorm|premiere|photoshop/i, ["dll", "exe", "pak"]],
  [/system32|syswow|winsxs|fonts|installer/i, ["dll", "sys", "mui", "msi", "ttf"]],
  [/temp|cache|local|packages|chrome|discord|code|roaming/i, ["tmp", "log", "db", "dat"]],
  [/backup|phone|mail|archive|finance/i, ["zip", "7z", "tar", "pst"]],
  [/target/i, ["rlib", "rmeta", "pdb", "exe"]],
];
const DEFAULT_EXTS = ["pdf", "docx", "xlsx", "txt", "md", "zip", "jpg", "png"];
const WORDS = ["report", "notes", "draft", "final", "backup", "export", "data", "assets", "build", "chunk", "pack",
  "scene", "clip", "track", "shot", "frame", "level", "texture", "bundle", "save"];

interface MockNode {
  name: string;
  dir: boolean;
  bytes: number;
  files: number;
  modified: number;
  hidden?: boolean;
  children: MockNode[];
}

function rng(seed: number): () => number {
  let a = seed;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function seedOf(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
  return h >>> 0;
}

function folder(name: string, children: MockNode[], modified: number): MockNode {
  return {
    name,
    dir: true,
    bytes: children.reduce((s, c) => s + c.bytes, 0),
    files: children.reduce((s, c) => s + c.files, 0),
    modified,
    children,
  };
}

/** Files (and for big folders a couple of subfolders) filling `bytes`. */
function fill(path: string, name: string, bytes: number, now: number, depth = 0): MockNode {
  const rnd = rng(seedOf(path));
  const exts = EXT_BY_HINT.find(([re]) => re.test(path))?.[1] ?? DEFAULT_EXTS;
  const children: MockNode[] = [];
  let left = bytes;
  if (bytes > 4 * GB && depth < 2) {
    for (let i = 0; i < 2; i++) {
      const share = left * (0.18 + rnd() * 0.17);
      const sub = WORDS[(rnd() * WORDS.length) | 0]! + (i ? "s" : "");
      children.push(fill(`${path}\\${sub}`, sub, share, now, depth + 1));
      left -= share;
    }
  }
  const count = 5 + Math.floor(rnd() * Math.min(40, 6 + Math.log2(1 + bytes / 1e6)));
  const weights = Array.from({ length: count }, () => Math.exp(rnd() * 4));
  const total = weights.reduce((s, w) => s + w, 0);
  let given = 0;
  weights.forEach((w, i) => {
    const size = i === count - 1 ? Math.round(left) - given : Math.round((left * w) / total);
    given += size;
    const ext = exts[(rnd() * exts.length) | 0]!;
    const word = WORDS[(rnd() * WORDS.length) | 0]!;
    children.push({
      name: `${word}-${String(i + 1).padStart(3, "0")}.${ext}`,
      dir: false,
      bytes: Math.max(0, size),
      files: 1,
      modified: Math.floor(now - rnd() * 600 * DAY),
      children: [],
    });
  });
  return folder(name, children, Math.floor(now - rnd() * 90 * DAY));
}

function build(path: string, name: string, spec: Spec, now: number): MockNode {
  const children = Object.entries(spec).map(([n, v]) => {
    const p = `${path}\\${n}`;
    if (typeof v !== "number") return build(p, n, v, now);
    if (/\.[a-z0-9]+$/i.test(n)) {
      return { name: n, dir: false, bytes: Math.round(v * GB), files: 1, modified: now - 2 * DAY, children: [] };
    }
    return fill(p, n, v * GB, now);
  });
  return folder(name, children, now - DAY);
}

export interface MockSpace {
  tree(deviceId: string, path: string, depth: number): SpaceNode;
  largest(deviceId: string, path: string, limit: number): ResultRow[];
  types(deviceId: string, path: string): TypeTotal[];
}

export function createMockSpace(
  devices: () => { id: string; displayName: string; lastMountPoint: string; online: boolean }[],
  now: number,
): MockSpace {
  const roots = new Map<string, MockNode>();

  function device(id: string) {
    const d = devices().find((x) => x.id === id);
    if (!d || !SPECS[id]) throw new Error("Unknown drive");
    return d;
  }

  function root(id: string): MockNode {
    let r = roots.get(id);
    if (!r) {
      const mount = device(id).lastMountPoint.replace(/\\+$/, "");
      r = build(mount, "", SPECS[id]!, now);
      for (const [parent, bytes] of HIDDEN[id] ?? []) {
        const p = r.children.find((c) => c.name === parent)!;
        p.children.push({ name: "Other users", dir: true, bytes, files: 1840, modified: 0, hidden: true, children: [] });
        p.bytes += bytes;
        p.files += 1840;
        r.bytes += bytes;
        r.files += 1840;
      }
      roots.set(id, r);
    }
    return r;
  }

  /** The node at `path` plus its full path (no trailing backslash). */
  function find(id: string, path: string): [MockNode, string] {
    const d = device(id);
    const mount = d.lastMountPoint.replace(/\\+$/, "");
    const p = path.replace(/\\+$/, "");
    if (p.toLowerCase() === mount.toLowerCase()) return [root(id), mount];
    if (!p.toLowerCase().startsWith(mount.toLowerCase() + "\\")) throw new Error("That folder no longer exists");
    let node = root(id);
    let full = mount;
    for (const part of p.slice(mount.length + 1).split("\\")) {
      const next = node.children.find((c) => c.dir && !c.hidden && c.name.toLowerCase() === part.toLowerCase());
      if (!next) throw new Error("That folder no longer exists");
      node = next;
      full += `\\${next.name}`;
    }
    return [node, full];
  }

  function toSpace(n: MockNode, level: number, depth: number, min: number): SpaceNode {
    const kind = n.hidden ? "hidden" : n.dir ? "folder" : "file";
    return {
      name: n.name,
      kind,
      bytes: n.bytes,
      files: n.files,
      modified: n.modified,
      cloud: n.name === "OneDrive",
      children: kind === "folder" && level < depth ? listChildren(n, level + 1, depth, min) : [],
    };
  }

  function listChildren(n: MockNode, level: number, depth: number, min: number): SpaceNode[] {
    const items = [...n.children].sort((a, b) => b.bytes - a.bytes);
    const keep = level === 1 ? Math.min(items.length, MAX_LISTED) : items.filter((c) => c.bytes >= min).length;
    const out = items.slice(0, keep).map((c) => toSpace(c, level, depth, min));
    const rest = items.slice(keep);
    if (rest.length) {
      out.push({
        name: "",
        kind: { smaller: { count: rest.length } },
        bytes: rest.reduce((s, c) => s + c.bytes, 0),
        files: rest.reduce((s, c) => s + c.files, 0),
        modified: Math.max(...rest.map((c) => c.modified)),
        cloud: false,
        children: [],
      });
    }
    return out;
  }

  function filesBelow(n: MockNode, path: string, out: [MockNode, string][]) {
    for (const c of n.children) {
      if (c.hidden) continue;
      if (c.dir) filesBelow(c, `${path}\\${c.name}`, out);
      else out.push([c, path]);
    }
  }

  return {
    tree(id, path, depth) {
      const [n] = find(id, path);
      const top = toSpace(n, 0, depth, n.bytes * MIN_SHARE);
      return { ...top, kind: "folder" };
    },
    largest(id, path, limit) {
      const [n, full] = find(id, path);
      const d = device(id);
      const files: [MockNode, string][] = [];
      filesBelow(n, full, files);
      files.sort((a, b) => b[0].bytes - a[0].bytes);
      return files.slice(0, limit).map(([f, folderPath]) => ({
        name: f.name,
        folder: folderPath,
        fullPath: `${folderPath}\\${f.name}`,
        size: f.bytes,
        modified: f.modified,
        created: f.modified - 3 * DAY,
        isDir: false,
        attrs: 0,
        deviceId: d.id,
        deviceName: d.displayName,
        online: d.online,
      }));
    },
    types(id, path) {
      const [n, full] = find(id, path);
      const files: [MockNode, string][] = [];
      filesBelow(n, full, files);
      const sums = new Map<TypeCategory, TypeTotal>();
      for (const [f] of files) {
        const category = typeOf(f.name.toLowerCase());
        const t = sums.get(category) ?? { category, bytes: 0, files: 0 };
        t.bytes += f.bytes;
        t.files += 1;
        sums.set(category, t);
      }
      return [...sums.values()].sort((a, b) => b.bytes - a.bytes);
    },
  };
}

const TYPE_EXTS: Record<Exclude<TypeCategory, "other">, string[]> = {
  video: ["mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v", "mpg", "mpeg", "m2ts", "mts", "3gp"],
  pictures: ["jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "avif", "raw", "cr2", "cr3",
    "nef", "arw", "dng", "psd", "svg", "ico"],
  music: ["mp3", "flac", "wav", "aac", "m4a", "ogg", "opus", "wma", "aiff", "aif", "mid", "midi"],
  documents: ["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt", "md", "csv",
    "epub"],
  archives: ["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso", "img", "cab", "dmg", "wim", "esd",
    "vhd", "vhdx", "vmdk"],
  apps: ["exe", "dll", "msi", "msix", "appx", "so", "jar", "apk", "pyd", "node", "wasm", "lib", "pdb", "obj", "rlib",
    "rmeta", "js", "ts", "py", "rs", "c", "cpp", "h", "cs", "java", "go", "pak", "asar"],
  system: ["sys", "drv", "mui", "cat", "etl", "evtx", "log", "tmp", "dmp", "blf"],
};

/** Same table as `type_of` in crates/seek-core/src/space.rs (subset). */
export function typeOf(nameLower: string): TypeCategory {
  const dot = nameLower.lastIndexOf(".");
  if (dot <= 0) return "other";
  const ext = nameLower.slice(dot + 1);
  for (const [cat, exts] of Object.entries(TYPE_EXTS)) {
    if (exts.includes(ext)) return cat as TypeCategory;
  }
  return "other";
}
