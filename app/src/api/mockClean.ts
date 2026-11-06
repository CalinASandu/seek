import type {
  AppRow,
  CleanEvent,
  CommitResult,
  DupCopy,
  DupGroup,
  DupScanState,
  HistoryEntry,
  ItemOutcome,
  Level,
  LeftoverRow,
  Lock,
  OtherPcMatch,
  Prepared,
  PrepareRequest,
  PutBackResult,
  Reason,
  ReviewItem,
  SeekApi,
  UninstallState,
  Why,
} from "./types";
import { fileName, folderOf, freeableOf, isProtectedPath } from "../lib/clean";

/**
 * `default`: about 40 duplicate groups, leftovers at every level, about 30 apps.
 * `empty`: every list comes back empty (zero-results states).
 * `big`: also 1,800 more duplicate groups, so "Use suggestions" gives a review sheet of thousands of rows.
 * In the browser preview: `?clean=empty` or `?clean=big`.
 */
export type CleanScenario = "default" | "empty" | "big";

export interface MockCleanOptions {
  scenario: CleanScenario;
  /** Real timing (groups trickle in, uninstalls take a while) instead of everything at once. */
  simulate: boolean;
  delay: () => Promise<void>;
  protectedPaths: () => string[];
  now: number;
}

export type CleanApi = Pick<
  SeekApi,
  | "cleanScanStart"
  | "cleanScanCancel"
  | "cleanScanState"
  | "onCleanEvent"
  | "cleanLeftovers"
  | "cleanApps"
  | "cleanPrepare"
  | "cleanCommit"
  | "cleanHistory"
  | "cleanPutBack"
  | "uninstallStart"
  | "uninstallState"
  | "uninstallLeftovers"
  | "openRecycleBin"
>;

const DAY = 86_400_000;
const MB = 1024 ** 2;
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

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// ---- the engine's own sentences (crates/seek-clean/src/reasons.rs), word for word -----------------------------
const ENGINE_REASON: Record<Lock, string> = {
  systemFolder: "This is, or holds, a part of Windows or of your user folders, so it was left in place.",
  programFiles: "This is in Program Files, so it was left in place.",
  otherUser: "This belongs to another user, so it was left in place.",
  appData: "This is in an app data folder, so it was left in place.",
  toolFolder: "This is inside a tool folder (.git, node_modules and the like), so it was left in place.",
  noRecycleBin:
    "Windows might delete this for good because of how this drive or Windows is set up, so it was left in place.",
  binFull: "The Recycle Bin on this drive is too full to hold this. Empty it first, then try again. It was left in place.",
  pathTooLong: "Some paths in this are too long for the Recycle Bin to hold reliably, so it was left in place.",
  cloudPlaceholder: "This is an online-only cloud file, so it was left in place.",
  protected: "You marked this, or something inside it, as always protected, so it was left in place.",
  linkedPc: "This is on a linked PC. Seek only removes files on this PC, so it was left in place.",
  unsupportedPath: "Seek cannot remove this kind of path, so it was left in place.",
  reparsePoint: "This is a link to somewhere else, so it was left in place.",
  claimed: "An app or a running program is using this folder, so it was left in place.",
  sharedFolder: "Inside a folder Windows and other apps share, so Seek doesn't remove anything in it.",
};
const LAST_COPY = "This is the last copy, so it was left in place.";

// ---- duplicate groups --------------------------------------------------------------------------------------------

interface CopySpec {
  path: string;
  dev: string;
  age: number;
  lock?: Lock;
}

function suggestIndex(copies: { path: string; modified: number }[]): number {
  const rank = (c: { path: string; modified: number }) => [
    /\\(downloads|temp)\\/i.test(c.path) ? 1 : 0,
    c.path.length,
    c.modified,
  ];
  let best = 0;
  copies.forEach((c, i) => {
    const a = rank(c);
    const b = rank(copies[best]!);
    for (let k = 0; k < 3; k++) {
      if (a[k]! < b[k]!) return void (best = i);
      if (a[k]! > b[k]!) return;
    }
  });
  return best;
}

function makeGroup(id: number, size: number, specs: CopySpec[], now: number): DupGroup {
  const copies: DupCopy[] = specs.map((s) => ({
    path: s.path,
    deviceId: s.dev,
    size,
    modified: now - s.age * DAY,
    lock: s.lock ?? null,
    suggestedKeep: false,
  }));
  copies[suggestIndex(copies)]!.suggestedKeep = true;
  return {
    id,
    name: fileName(copies[0]!.path),
    size,
    copies,
    freeable: freeableOf(size, copies.map((c) => c.lock !== null)),
  };
}

const FILL_NAMES = [
  "Holiday-Photos-2023.zip", "Lecture-recordings.mp4", "Project-backup.7z", "Tax-return-2022.pdf", "Wedding-video-raw.mov",
  "Game-saves.zip", "Podcast-ep42.wav", "Thesis-draft-final.docx", "Design-assets.psd", "Invoice-archive.pdf",
  "Family-trip.mp4", "Budget-2024.xlsx", "Presentation-final.pptx", "Scan-0042.tiff", "Album-flac.zip",
  "Setup-installer.exe", "Dataset-2021.csv", "Render-final.mp4", "Camera-roll-export.zip", "Resume-and-letters.pdf",
  "Screen-recording-march.mkv", "Demo-reel.mov", "Sample-pack.zip", "Course-videos.mp4", "Ebook-collection.epub",
  "Map-tiles.pak", "Backup-2022.bak", "Photo-library.dmg.zip", "Voice-memos.m4a", "Site-export.zip",
  "Slides-lecture-9.pdf", "Recipe-book.pdf", "Contract-signed.pdf", "VM-snapshot.vhdx", "Notes-archive.zip",
];
const FILL_FOLDERS: [string, string][] = [
  ["dev-c", "C:\\Users\\Calin\\Documents\\Work"],
  ["dev-c", "C:\\Users\\Calin\\Downloads"],
  ["dev-c", "C:\\Users\\Calin\\Desktop"],
  ["dev-d", "D:\\Photos\\2023"],
  ["dev-d", "D:\\Backups\\Old laptop\\Documents"],
  ["dev-d", "D:\\Projects\\Archive"],
  ["dev-d", "D:\\Downloads"],
  ["dev-d", "D:\\Uni\\Sem2\\Lectures"],
];

function buildGroups(scenario: CleanScenario, now: number): DupGroup[] {
  if (scenario === "empty") return [];
  const rnd = mulberry32(7);
  const defs: { size: number; specs: CopySpec[] }[] = [];

  // Fillers: two to four copies in different folders.
  FILL_NAMES.forEach((name, i) => {
    const size = Math.round(Math.exp(Math.log(1.3 * MB) + rnd() * (Math.log(2.4 * GB) - Math.log(1.3 * MB))));
    const n = 2 + Math.floor(rnd() * 3);
    const picks = new Set<number>();
    while (picks.size < n) picks.add(Math.floor(rnd() * FILL_FOLDERS.length));
    defs.push({
      size,
      specs: [...picks].map((p, k) => ({
        path: `${FILL_FOLDERS[p]![1]}\\${k === 0 || rnd() > 0.3 ? name : name.replace(/\.(\w+)$/, " (1).$1")}`,
        dev: FILL_FOLDERS[p]![0],
        age: 20 + Math.floor(rnd() * 900) + i,
      })),
    });
  });

  // A copy in Program Files: locked, but it still counts as a copy that stays.
  defs.push({
    size: 24 * MB,
    specs: [
      { path: "C:\\Users\\Calin\\Downloads\\vc_redist.x64.exe", dev: "dev-c", age: 210 },
      { path: "C:\\Program Files\\Microsoft Visual Studio\\Installer\\Setup\\vc_redist.x64.exe", dev: "dev-c", age: 480, lock: "programFiles" },
      { path: "C:\\Users\\Calin\\Documents\\Installers\\vc_redist.x64.exe", dev: "dev-c", age: 330 },
    ],
  });
  // A USB stick without a Recycle Bin: locked, with its reason.
  defs.push({
    size: 31 * MB,
    specs: [
      { path: "G:\\DCIM\\100CANON\\IMG_4412.CR3", dev: "dev-usb-g", age: 60, lock: "noRecycleBin" },
      { path: "D:\\Photos\\2024\\Wedding\\IMG_4412.CR3", dev: "dev-d", age: 58 },
      { path: "D:\\Photos\\2024\\Wedding\\selects\\IMG_4412.CR3", dev: "dev-d", age: 40 },
    ],
  });
  // Locked in other ways.
  defs.push({
    size: 86 * MB,
    specs: [
      { path: "C:\\Users\\Calin\\AppData\\Local\\Spotify\\Data\\a1f3.file", dev: "dev-c", age: 12, lock: "appData" },
      { path: "D:\\Music\\Cache\\a1f3.file", dev: "dev-d", age: 90 },
      { path: "D:\\Music\\Cache copy\\a1f3.file", dev: "dev-d", age: 85 },
    ],
  });
  defs.push({
    size: 64 * MB,
    specs: [
      { path: "C:\\Users\\Calin\\OneDrive\\Pictures\\Trip-panorama.jpg", dev: "dev-c", age: 300, lock: "cloudPlaceholder" },
      { path: "D:\\Photos\\2023\\Trip-panorama.jpg", dev: "dev-d", age: 310 },
    ],
  });
  // A long name and very long paths: the row must stay legible.
  const deep = "D:\\Backups\\Old laptop\\Users\\Calin\\Documents\\Work\\Clients\\Northwind Traders International\\2019\\Board meetings\\Quarterly reviews\\Consolidated statements\\Drafts for review";
  const longName = "Quarterly-financial-statement-consolidated-draft-for-review-by-the-board-of-directors-FINAL-v7-signed.xlsx";
  defs.push({
    size: 412 * MB,
    specs: [
      { path: `${deep}\\${longName}`, dev: "dev-d", age: 1100 },
      { path: `C:\\Users\\Calin\\Documents\\Work\\Clients\\Northwind Traders International\\${longName}`, dev: "dev-c", age: 400 },
      { path: `D:\\Uni\\Thesis\\Drafts\\${longName}`, dev: "dev-d", age: 700 },
    ],
  });
  // One huge group, with a few copies inside tool folders.
  const hugeSpecs: CopySpec[] = Array.from({ length: 400 }, (_, i) => {
    const n = String(i + 1).padStart(4, "0");
    const inTool = i % 83 === 5;
    return {
      path: inTool
        ? `D:\\Programming\\web\\node_modules\\.cache\\build-${n}\\asset-pack.pak`
        : `D:\\Programming\\web\\.cache\\build-${n}\\asset-pack.pak`,
      dev: "dev-d",
      age: 3 + i,
      lock: inTool ? ("toolFolder" as Lock) : undefined,
    };
  });
  defs.push({ size: 18 * MB, specs: hugeSpecs });

  if (scenario === "big") {
    for (let i = 0; i < 1800; i++) {
      const n = String(i).padStart(4, "0");
      defs.push({
        size: Math.round((1.2 + rnd() * 180) * MB),
        specs: [
          { path: `D:\\Archive\\2022\\batch-${Math.floor(i / 40)}\\export-${n}.zip`, dev: "dev-d", age: 400 + (i % 300) },
          { path: `D:\\Archive\\2022\\batch-${Math.floor(i / 40)}-copy\\export-${n}.zip`, dev: "dev-d", age: 300 + (i % 300) },
          { path: `C:\\Users\\Calin\\Downloads\\export-${n}.zip`, dev: "dev-c", age: 100 + (i % 200) },
        ],
      });
    }
  }

  // Largest first, like the real scan.
  const sized = defs.map((d) => ({ ...d, free: freeableOf(d.size, d.specs.map((s) => s.lock !== undefined)) }));
  sized.sort((a, b) => b.free - a.free);
  return sized.filter((d) => d.free > 0).map((d, i) => makeGroup(i + 1, d.size, d.specs, now));
}

function buildOtherPcs(scenario: CleanScenario): OtherPcMatch[] {
  if (scenario === "empty") return [];
  return [
    { name: "Holiday-Photos-2023.zip", size: 1_280 * MB, pcName: "Laptop", path: "C:\\Users\\Calin\\Documents\\Holiday-Photos-2023.zip" },
    { name: "Thesis-draft-final.docx", size: 4 * MB, pcName: "Laptop", path: "C:\\Users\\Calin\\Documents\\Uni\\Thesis-draft-final.docx" },
    { name: "Setup-installer.exe", size: 212 * MB, pcName: "Office-PC", path: "D:\\Installers\\Setup-installer.exe" },
    { name: "Course-videos.mp4", size: 940 * MB, pcName: "Laptop", path: "C:\\Users\\Calin\\Videos\\Course-videos.mp4" },
  ];
}

// ---- leftovers ---------------------------------------------------------------------------------------------------

const LOCAL = "C:\\Users\\Calin\\AppData\\Local";
const ROAM = "C:\\Users\\Calin\\AppData\\Roaming";
const LOW = "C:\\Users\\Calin\\AppData\\LocalLow";
const PF = "C:\\Program Files";
const PF86 = "C:\\Program Files (x86)";

const R = {
  empty: { text: "Empty, or nothing left but empty folders", points: 4 },
  noSub: { text: "Has no subfolders", points: 2 },
  stale: { text: "Nothing inside changed for a year", points: 2 },
  generic: { text: "Generic folder name", points: -3 },
  programs: { text: "Contains programs (.exe, .dll)", points: -4 },
  many: { text: "Contains many files", points: -2 },
  publisher: { text: "The publisher still has an app installed", points: -4 },
  recent: { text: "Changed in the last 30 days", points: -3 },
  inFolder: { text: "Was inside an uninstalled app's install folder", points: 4 },
  name: { text: "Named like an app that is no longer installed", points: 2 },
  parent: { text: "Its parent folder is named after an uninstalled app's publisher", points: 4 },
  depth: { text: "Sits one level below the top", points: -2 },
} satisfies Record<string, Reason>;

export function levelOf(points: number): Level {
  return points >= 5 ? "veryLikely" : points >= 2 ? "likely" : points >= 0 ? "notSure" : "probablyInUse";
}

interface LeftDef {
  parent: string;
  name: string;
  mb: number;
  age: number;
  admin?: boolean;
  reasons: Reason[];
  contents: string[];
}

function leftoverRow(d: LeftDef, now: number): LeftoverRow {
  const total = d.reasons.reduce((s, r) => s + r.points, 0);
  return {
    path: `${d.parent}\\${d.name}`,
    parent: d.parent,
    size: Math.round(d.mb * MB),
    modified: now - d.age * DAY,
    needsAdmin: !!d.admin,
    level: levelOf(total),
    reasons: [...d.reasons].sort((a, b) => Math.abs(b.points) - Math.abs(a.points)),
    contents: d.contents,
  };
}

const LEFTOVER_DEFS: LeftDef[] = [
  { parent: LOCAL, name: "ZoomUpdater-Cache", mb: 30, age: 690, reasons: [R.empty, R.stale, R.noSub], contents: ["cache.db", "log.txt"] },
  { parent: ROAM, name: "Adobe Premiere Pro 2020 Presets", mb: 1820, age: 800, reasons: [R.inFolder, R.stale, R.noSub], contents: ["Effect Presets and Animation Presets.prfpset", "Lumetri Looks", "Titles", "Scratch"] },
  { parent: LOCAL, name: "OldVPNClient", mb: 96, age: 540, reasons: [R.name, R.stale, R.noSub, R.inFolder], contents: ["settings.json", "logs", "profiles"] },
  { parent: ROAM, name: "Skype for Desktop", mb: 212, age: 430, reasons: [R.name, R.stale, R.noSub], contents: ["Cache", "logs", "settings.json"] },
  { parent: LOW, name: "CrashReporter-Legacy", mb: 14, age: 900, reasons: [R.empty, R.stale], contents: [] },
  { parent: PF86, name: "Battle.net Launcher", mb: 412, age: 380, admin: true, reasons: [R.inFolder, R.name, R.programs], contents: ["Battle.net Launcher.exe", "Agent", "Blizzard Entertainment"] },
  { parent: LOCAL, name: "Dropbox Update", mb: 58, age: 220, reasons: [R.name, R.stale, R.depth], contents: ["DropboxUpdate.exe", "Download", "Install"] },
  { parent: ROAM, name: "WinRAR", mb: 2, age: 600, reasons: [R.name, R.noSub], contents: ["rarreg.key", "WinRAR.ini"] },
  { parent: PF, name: "Intel Driver Cache", mb: 640, age: 700, admin: true, reasons: [R.stale, R.noSub, R.parent, R.programs], contents: ["setup.exe", "drivers.cab"] },
  { parent: LOCAL, name: "Temp-Installer-8821", mb: 0.4, age: 210, reasons: [R.stale, R.generic, R.noSub], contents: ["install.log"] },
  { parent: ROAM, name: "Config", mb: 3, age: 150, reasons: [R.generic, R.noSub, R.stale], contents: ["app.cfg"] },
  { parent: LOCAL, name: "NVIDIA Corporation", mb: 380, age: 25, reasons: [R.publisher, R.recent, R.many, R.stale], contents: ["NvBackend", "NvCamera", "Drs"] },
  { parent: ROAM, name: "Code", mb: 1240, age: 2, reasons: [R.publisher, R.recent, R.many], contents: ["User", "logs", "CachedData", "Backups"] },
  { parent: PF, name: "Common Tools", mb: 88, age: 40, admin: true, reasons: [R.generic, R.programs, R.many], contents: ["tool.exe", "helper.dll"] },
  { parent: LOCAL, name: "Packages-Staging", mb: 120, age: 18, reasons: [R.recent, R.generic, R.stale], contents: ["staging.bin"] },
  {
    parent: LOCAL,
    name: "com.example.vendor-with-an-extremely-long-name.productsuite.installer.cache.2021-10-12T08-33-21Z-backup-copy",
    mb: 505,
    age: 780,
    reasons: [R.name, R.stale, R.noSub],
    contents: ["installer-cache-with-a-long-file-name-so-the-contents-list-has-to-cope-too.dat", "manifest.json"],
  },
];

/** Seek's own index for a drive it no longer knows, as the service reports it. */
const STALE_INDEX_LINE = "Seek's index for a drive it no longer knows";

function staleRow(now: number): LeftoverRow {
  return {
    path: "C:\\ProgramData\\Seek\\indexes\\5e1f0a52-7c3b-4d0e-9a41-0d3a2f7b9c66.seekidx",
    parent: "C:\\ProgramData\\Seek\\indexes",
    size: 48 * MB,
    modified: now - 200 * DAY,
    needsAdmin: true,
    level: "veryLikely",
    reasons: [{ text: "No drive Seek knows about uses this file", points: 5 }],
    contents: [STALE_INDEX_LINE],
  };
}

function buildLeftovers(scenario: CleanScenario, now: number): LeftoverRow[] {
  if (scenario === "empty") return [];
  return [...LEFTOVER_DEFS.map((d) => leftoverRow(d, now)), staleRow(now)];
}

/** What an app might leave behind once its own uninstaller is done. Nothing in here is what the uninstaller removes. */
function uninstallRows(app: AppRow, now: number): LeftoverRow[] {
  const pub = app.publisher || app.name;
  const defs: LeftDef[] = [
    { parent: LOCAL, name: app.name, mb: Math.max(1, app.size / MB / 12), age: 5, reasons: [R.name, R.stale, R.noSub], contents: ["cache", "settings.json"] },
    { parent: ROAM, name: app.name, mb: Math.max(1, app.size / MB / 30), age: 4, reasons: [R.name, R.noSub], contents: ["settings.json", "logs"] },
    { parent: LOCAL, name: `${pub} Shared`, mb: 18, age: 3, reasons: [R.parent, R.programs, R.depth], contents: ["shared.dll", "updater.exe"] },
  ];
  if (app.folder) {
    defs.push({ parent: folderOf(app.folder), name: fileName(app.folder), mb: Math.max(2, app.size / MB / 200), age: 1, admin: app.folder.startsWith(PF), reasons: [R.inFolder, R.empty], contents: ["unins000.dat"] });
  }
  return defs.map((d) => leftoverRow(d, now));
}

// ---- apps --------------------------------------------------------------------------------------------------------

function buildApps(scenario: CleanScenario, now: number): AppRow[] {
  if (scenario === "empty") return [];
  const a = (
    id: string, name: string, publisher: string, version: string, mb: number, ageDays: number | null,
    folder: string | null, extra: Partial<AppRow> = {},
  ): AppRow => ({
    id, name, publisher, version, size: Math.round(mb * MB), installDate: ageDays === null ? null : now - ageDays * DAY,
    icon: null, folder, folderGuessed: false, canUninstall: true, note: null, running: false, ...extra,
  });
  const runtime = "Other apps may need this.";
  return [
    a("app-chrome", "Google Chrome", "Google LLC", "130.0.6723.92", 640, 12, `${PF}\\Google\\Chrome`),
    a("app-vscode", "Microsoft Visual Studio Code", "Microsoft Corporation", "1.95.1", 410, 30, `${LOCAL}\\Programs\\Microsoft VS Code`),
    a("app-spotify", "Spotify", "Spotify AB", "1.2.52", 360, 45, `${ROAM}\\Spotify`, { running: true, folderGuessed: true }),
    a("app-vlc", "VLC media player", "VideoLAN", "3.0.21", 170, 200, `${PF}\\VideoLAN\\VLC`),
    a("app-7zip", "7-Zip", "Igor Pavlov", "24.08", 5.6, 120, `${PF}\\7-Zip`),
    a("app-zoom", "Zoom Workplace", "Zoom Video Communications", "6.2.5", 410, 20, `${PF}\\Zoom`),
    a("app-steam", "Steam", "Valve Corporation", "2.10.91", 3_600, 700, `${PF86}\\Steam`),
    a("app-discord", "Discord", "Discord Inc.", "1.0.9172", 380, 8, `${LOCAL}\\Discord`),
    a("app-git", "Git", "The Git Development Community", "2.47.0", 330, 90, `${PF}\\Git`),
    a("app-node", "Node.js", "Node.js Foundation", "22.11.0", 90, 60, `${PF}\\nodejs`, { note: "Developer tools may need this." }),
    a("app-vcredist-x64", "Microsoft Visual C++ 2015-2022 Redistributable (x64)", "Microsoft Corporation", "14.40.33810", 24, 400, null, { note: runtime }),
    a("app-vcredist-x86", "Microsoft Visual C++ 2015-2022 Redistributable (x86)", "Microsoft Corporation", "14.40.33810", 20, 400, null, { note: runtime }),
    a("app-dotnet", "Microsoft .NET Runtime 8.0.10 (x64)", "Microsoft Corporation", "8.0.10", 74, 80, `${PF}\\dotnet`, { note: runtime }),
    a("app-nvidia", "NVIDIA Graphics Driver 561.09", "NVIDIA Corporation", "561.09", 920, 30, null, { note: "Your graphics card needs this." }),
    a("app-obs", "OBS Studio", "OBS Project", "30.2.3", 330, 150, `${PF}\\obs-studio`),
    a("app-gimp", "GIMP 2.10.38", "The GIMP Team", "2.10.38", 1_040, 260, `${PF}\\GIMP 2`),
    a("app-blender", "Blender", "Blender Foundation", "4.2.3", 1_900, 40, `${PF}\\Blender Foundation\\Blender 4.2`),
    a("app-odd", "Odd Portable Tool", "Unknown", "1.0", 8, 100, null, { canUninstall: false, note: "Seek can't start this app's uninstaller." }),
    a("app-notepadpp", "Notepad++ (64-bit x64)", "Notepad++ Team", "8.7.1", 12, 300, `${PF}\\Notepad++`),
    a("app-qbit", "qBittorrent", "The qBittorrent project", "5.0.1", 66, 500, `${PF}\\qBittorrent`),
    a("app-winrar", "WinRAR 7.01 (64-bit)", "win.rar GmbH", "7.01", 11, 610, `${PF}\\WinRAR`),
    a("app-java", "Java 8 Update 421", "Oracle Corporation", "8.0.4210.9", 118, 900, `${PF}\\Java\\jre-1.8`, { note: "Other apps may need this." }),
    a("app-teamviewer", "TeamViewer", "TeamViewer Germany GmbH", "15.58.4", 240, 330, `${PF86}\\TeamViewer`),
    a("app-libreoffice", "LibreOffice 24.8.2.1", "The Document Foundation", "24.8.2.1", 1_150, 75, `${PF}\\LibreOffice`),
    a("app-audacity", "Audacity", "Audacity Team", "3.7.0", 150, 22, `${PF}\\Audacity`),
    a("app-battlenet", "Battle.net", "Blizzard Entertainment", "2.43.1", 520, 410, `${PF86}\\Battle.net`),
    a("app-stubborn", "Legacy Backup Suite", "Contoso Software", "4.1.0", 880, 1200, `${PF86}\\Contoso\\Legacy Backup Suite`),
    a("app-nofolder", "Realtek Audio Console", "Realtek Semiconductor Corp.", "1.45.268", 38, 100, null),
    a("app-msedge", "Microsoft Edge WebView2 Runtime", "Microsoft Corporation", "130.0.2849.56", 640, 14, `${PF86}\\Microsoft\\EdgeWebView`, { note: runtime }),
    a("app-longname", "Autodesk Fusion 360 Subscription Edition with Manufacturing Extension and Generative Design Collaboration Tools", "Autodesk, Inc.", "2.0.20981", 4_700, 55, `${LOCAL}\\Autodesk\\webdeploy\\production\\6a0c9611291d45bb9226980209917c3d`),
  ];
}

// ---- the history of past removals --------------------------------------------------------------------------------

function seedHistory(scenario: CleanScenario, now: number): HistoryEntry[] {
  if (scenario === "empty") return [];
  const bin = (n: string, ext: string) => `C:\\$Recycle.Bin\\S-1-5-21-3857284719-221946123-1730241-1001\\$R${n}${ext}`;
  return [
    { id: "h-seed-1", timeMs: now - 2 * 3_600_000, originalPath: "D:\\Downloads\\ubuntu-22.04.3-desktop-amd64 (1).iso", size: 4_700 * MB, why: "duplicate", binPath: bin("AB12CD", ".iso"), leftReason: null, putBackMs: null },
    { id: "h-seed-2", timeMs: now - 2 * 3_600_000, originalPath: "C:\\Users\\Calin\\Downloads\\invoice-0382 (2).pdf", size: 2 * MB, why: "duplicate", binPath: bin("EF34GH", ".pdf"), leftReason: null, putBackMs: null },
    { id: "h-seed-3", timeMs: now - 2 * 3_600_000, originalPath: "C:\\Users\\Calin\\Documents\\Work\\report-final.docx", size: 6 * MB, why: "duplicate", binPath: null, leftReason: "This changed after you reviewed it, so it was left in place.", putBackMs: null },
    { id: "h-seed-4", timeMs: now - 3 * DAY, originalPath: "C:\\Users\\Calin\\AppData\\Local\\OldToolCache", size: 380 * MB, why: "leftover", binPath: bin("IJ56KL", ""), leftReason: null, putBackMs: null },
    { id: "h-seed-5", timeMs: now - 9 * DAY, originalPath: "C:\\Users\\Calin\\AppData\\Roaming\\Skype for Desktop", size: 212 * MB, why: "leftover", binPath: bin("MN78OP", ""), leftReason: null, putBackMs: null },
    { id: "h-seed-6", timeMs: now - 40 * DAY, originalPath: "C:\\Program Files (x86)\\Contoso Updater", size: 54 * MB, why: "uninstall", binPath: bin("QR90ST", ""), leftReason: null, putBackMs: null },
  ];
}

// ---- the mock --------------------------------------------------------------------------------------------------

export function createMockClean(opts: MockCleanOptions): CleanApi {
  const { now, delay, scenario, simulate } = opts;
  let groups = buildGroups(scenario, now);
  const otherPcs = buildOtherPcs(scenario);
  let leftovers = buildLeftovers(scenario, now);
  let apps = buildApps(scenario, now);
  let history = seedHistory(scenario, now);
  const uninstallLeft = new Map<string, LeftoverRow[]>();
  const uninstalls = new Map<string, { at: number; stubborn: boolean }>();
  const reviews = new Map<string, { items: ReviewItem[] }>();
  let nextToken = 1;
  let nextHistory = 1;

  const listeners = new Set<(e: CleanEvent) => void>();
  const emit = (e: CleanEvent) => listeners.forEach((l) => l(e));

  const scan = { running: false, found: [] as DupGroup[], checked: 0, total: 0, current: null as string | null };
  let run = 0;

  const state = (): DupScanState => ({
    running: scan.running,
    found: [...scan.found],
    otherPcs: scan.found.length > 0 || !scan.running ? otherPcs.slice() : [],
    checkedBytes: scan.checked,
    totalBytes: scan.total,
    current: scan.current,
  });

  async function runScan(id: number, scope: string[]) {
    const wanted = scope.length === 1 ? groups.filter((g) => g.copies.some((c) => c.deviceId === scope[0])) : groups;
    const total = wanted.reduce((s, g) => s + g.size * g.copies.length, 0) * 3;
    scan.running = true;
    scan.found = [];
    scan.checked = 0;
    scan.total = total;
    scan.current = null;
    if (!simulate) {
      await sleep(0);
      if (id !== run) return;
      for (const g of wanted) {
        scan.found.push(g);
        emit({ type: "group", group: g });
      }
      scan.checked = total;
      emit({ type: "progress", checkedBytes: total, totalBytes: total, current: null });
      scan.running = false;
      emit({ type: "done" });
      return;
    }
    // Real timing: about 14 s in all, a few groups every tick, progress between them.
    const ticks = 50;
    const per = Math.max(1, Math.ceil(wanted.length / ticks));
    const roots = ["C:\\Users\\Calin\\Documents", "D:\\Photos\\2023", "D:\\Backups\\Old laptop", "D:\\Programming\\web\\.cache", "C:\\Users\\Calin\\Downloads", "D:\\Uni\\Sem2"];
    for (let i = 0; i < wanted.length; i += per) {
      await sleep(280);
      if (id !== run) return;
      for (const g of wanted.slice(i, i + per)) {
        scan.found.push(g);
        emit({ type: "group", group: g });
      }
      const frac = Math.min(1, (i + per) / wanted.length);
      scan.checked = Math.round(total * frac);
      scan.current = `${roots[Math.floor(frac * 40) % roots.length]}\\${wanted[Math.min(i + per, wanted.length - 1)]?.name ?? ""}`;
      emit({ type: "progress", checkedBytes: scan.checked, totalBytes: total, current: scan.current });
    }
    await sleep(200);
    if (id !== run) return;
    scan.running = false;
    scan.current = null;
    emit({ type: "done" });
  }

  const isProtected = (p: string) => isProtectedPath(p, opts.protectedPaths());
  const binPathFor = (path: string, n: number) => {
    const dot = path.lastIndexOf(".");
    const ext = dot > path.lastIndexOf("\\") ? path.slice(dot) : "";
    return `C:\\$Recycle.Bin\\S-1-5-21-3857284719-221946123-1730241-1001\\$R${(n * 7919).toString(36).toUpperCase().padStart(6, "0").slice(0, 6)}${ext}`;
  };

  function review(items: Omit<ReviewItem, "id">[]): Prepared {
    const token = `tok-${nextToken++}`;
    const full = items.map((it, id) => ({ ...it, id }));
    reviews.set(token, { items: full });
    // like the engine, only the newest few reviews stay valid
    for (const k of [...reviews.keys()].slice(0, Math.max(0, reviews.size - 8))) reviews.delete(k);
    return { token, items: full.map((i) => ({ ...i })) };
  }

  const adminPath = (p: string) => /^c:\\program files/i.test(p);
  const allLeftRows = () => [...leftovers, ...[...uninstallLeft.values()].flat()];

  function dropMoved(paths: string[]) {
    const set = new Set(paths.map((p) => p.toLowerCase()));
    const next: DupGroup[] = [];
    for (const g of groups) {
      const copies = g.copies.filter((c) => !set.has(c.path.toLowerCase()));
      const freeable = freeableOf(g.size, copies.map((c) => c.lock !== null));
      if (freeable > 0) next.push({ ...g, copies, freeable });
    }
    groups = next;
    scan.found = scan.found.flatMap((g) => groups.filter((x) => x.id === g.id));
    leftovers = leftovers.filter((r) => !set.has(r.path.toLowerCase()));
    for (const [k, rows] of uninstallLeft) uninstallLeft.set(k, rows.filter((r) => !set.has(r.path.toLowerCase())));
  }

  return {
    async cleanScanStart(deviceIds) {
      await delay();
      const id = ++run;
      void runScan(id, deviceIds);
    },
    async cleanScanCancel() {
      await delay();
      if (!scan.running) return;
      run++;
      scan.running = false;
      scan.current = null;
      emit({ type: "done" });
    },
    async cleanScanState() {
      await delay();
      return state();
    },
    onCleanEvent(cb) {
      listeners.add(cb);
      return () => void listeners.delete(cb);
    },
    async cleanLeftovers() {
      await delay();
      if (simulate) await sleep(900);
      return leftovers.map((r) => ({ ...r }));
    },
    async cleanApps() {
      await delay();
      if (simulate) await sleep(500);
      const done = (id: string) => uninstalls.has(id) && !uninstalls.get(id)!.stubborn && Date.now() - uninstalls.get(id)!.at >= (simulate ? 2500 : 0);
      return apps.filter((a) => !done(a.id)).map((a) => ({ ...a }));
    },
    async cleanPrepare(req: PrepareRequest) {
      await delay();
      const found = new Map<string, DupCopy>();
      groups.forEach((g) => g.copies.forEach((c) => found.set(c.path.toLowerCase(), c)));
      const rows = new Map(allLeftRows().map((r) => [r.path.toLowerCase(), r]));
      const base = {
        modifiedMs: now,
        isDir: false,
        files: 1,
        needsAdmin: false,
        group: null as number | null,
        level: null as Level | null,
        reasons: [] as Reason[],
      };
      if (req.kind === "duplicates") {
        const items: Omit<ReviewItem, "id">[] = [];
        req.groups.forEach((copies, g) => {
          for (const c of copies) {
            const known = found.get(c.path.toLowerCase());
            if (!known) continue; // gone since the scan: left out, like the engine
            items.push({
              ...base,
              path: c.path,
              size: known.size,
              modifiedMs: known.modified,
              why: "duplicate",
              lock: known.lock ?? (isProtected(c.path) ? "protected" : null),
              needsAdmin: adminPath(c.path),
              group: g,
            });
          }
        });
        return review(items);
      }
      if (req.kind === "stale") {
        return review(
          req.files.map((p) => ({ ...base, path: p, size: 2 * MB, why: "staleData" as Why, lock: isProtected(p) ? ("protected" as Lock) : null, needsAdmin: true })),
        );
      }
      return review(
        req.folders.flatMap((f) => {
          const row = rows.get(f.path.toLowerCase());
          if (!row) return [];
          return [
            {
              ...base,
              path: f.path,
              size: row.size,
              modifiedMs: row.modified,
              isDir: true,
              files: Math.max(1, row.contents.length * 40),
              why: req.why,
              lock: isProtected(f.path) ? ("protected" as Lock) : null,
              needsAdmin: row.needsAdmin || adminPath(f.path),
              level: f.level,
              reasons: f.reasons,
            },
          ];
        }),
      );
    },
    async cleanCommit(token, ids): Promise<CommitResult> {
      await delay();
      const rev = reviews.get(token);
      if (!rev) throw new Error("This review has expired. Review again.");
      reviews.delete(token);
      const chosen = [...new Set(ids)].filter((id) => rev.items.some((i) => i.id === id)).sort((a, b) => a - b);
      const chosenSet = new Set(chosen);
      // rule 4: one copy of every group stays; if every local copy is chosen, the last of them is left
      const lastOfGroup = new Map<number, number>();
      const groupIds = new Set(rev.items.filter((i) => i.group !== null).map((i) => i.group!));
      for (const g of groupIds) {
        const all = rev.items.filter((i) => i.group === g);
        const survives = all.some((i) => !chosenSet.has(i.id) || i.lock !== null);
        if (!survives) lastOfGroup.set(g, Math.max(...all.filter((i) => chosenSet.has(i.id)).map((i) => i.id)));
      }
      const out: ItemOutcome[] = [];
      const entries: HistoryEntry[] = [];
      const movedPaths: string[] = [];
      let movedBytes = 0;
      for (const id of chosen) {
        const it = rev.items.find((i) => i.id === id)!;
        const reason =
          it.lock !== null
            ? ENGINE_REASON[it.lock]
            : isProtected(it.path)
              ? ENGINE_REASON.protected
              : it.group !== null && lastOfGroup.get(it.group) === id
                ? LAST_COPY
                : it.why === "leftover" && it.level === "probablyInUse"
                  ? ENGINE_REASON.claimed
                  : null;
        const entryId = `h-${nextHistory++}`;
        if (reason) {
          out.push({ id, path: it.path, size: it.size, outcome: { kind: "left", reason } });
          entries.push({ id: entryId, timeMs: Date.now(), originalPath: it.path, size: it.size, why: it.why, binPath: null, leftReason: reason, putBackMs: null });
        } else {
          const bin = binPathFor(it.path, id + nextHistory);
          out.push({ id, path: it.path, size: it.size, outcome: { kind: "moved", binPath: bin } });
          entries.push({ id: entryId, timeMs: Date.now(), originalPath: it.path, size: it.size, why: it.why, binPath: bin, leftReason: null, putBackMs: null });
          movedBytes += it.size;
          movedPaths.push(it.path);
        }
      }
      history = [...entries.reverse(), ...history];
      dropMoved(movedPaths);
      return { items: out, movedBytes, stoppedEarly: null, historyError: null };
    },
    async cleanHistory() {
      await delay();
      return history.map((h) => ({ ...h }));
    },
    async cleanPutBack(entryIds): Promise<PutBackResult[]> {
      await delay();
      return entryIds.map((id) => {
        const e = history.find((h) => h.id === id);
        if (!e || e.binPath === null) return { id, restored: false, reason: "This item was not moved, so there is nothing to put back." };
        if (e.putBackMs !== null) return { id, restored: false, reason: "Already put back." };
        if (e.id === "h-seed-5") return { id, restored: false, reason: "No longer in the Recycle Bin" };
        history = history.map((h) => (h.id === id ? { ...h, putBackMs: Date.now() } : h));
        return { id, restored: true, reason: null };
      });
    },
    async uninstallStart(appId) {
      await delay();
      const app = apps.find((a) => a.id === appId);
      if (!app) throw new Error("That app is not installed any more.");
      uninstalls.set(appId, { at: Date.now(), stubborn: appId === "app-stubborn" });
      uninstallLeft.set(appId, uninstallRows(app, now));
    },
    async uninstallState(appId): Promise<UninstallState> {
      await delay();
      const u = uninstalls.get(appId);
      if (!u) throw new Error("Seek hasn't started an uninstaller for that app.");
      const wait = simulate ? 2500 : 0;
      if (Date.now() - u.at < wait) return "running";
      return u.stubborn ? "unresolved" : "finished";
    },
    async uninstallLeftovers(appId) {
      await delay();
      return (uninstallLeft.get(appId) ?? []).map((r) => ({ ...r }));
    },
    async openRecycleBin() {
      await delay();
    },
  };
}
