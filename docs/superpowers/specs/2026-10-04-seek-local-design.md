# Seek — Local Version Design Spec

- **Date:** 2026-10-04
- **Status:** Draft for review
- **Scope:** Windows-only, single-PC ("local") version of Seek, including a polished desktop UI.
  Cross-PC sync and other operating systems are explicitly out of scope and will get their own spec;
  this design only guarantees the data model supports them.

---

## 1. Purpose

Seek is an open-source instant file-name search tool for Windows (university project), in the spirit of
voidtools *Everything*: type a few characters, get matching files and folders from every drive in
milliseconds.

**The differentiator:** Seek remembers devices. A USB stick, external disk or NAS share that is unplugged —
or that no longer exists at all — stays searchable. Results from those devices are shown as *offline*, with
where the file lived and when the device was last seen.

### Success criteria

| Criterion | Target |
|---|---|
| Initial index of an NTFS volume with ~1M entries | < 10 s |
| Query latency on 10M total entries (substring, no filters) | < 50 ms (p95) |
| App start → searchable (index loaded from disk) | < 1 s for 1–2M entries |
| Live update latency (file created → appears in results) | < 2 s |
| Devices supported | ≥ 5 concurrently known (online + offline) |
| UI | Professional, clean, keyboard-efficient, 60 fps scrolling through 1M+ results |

---

## 2. Decisions made during brainstorming

| Topic | Decision |
|---|---|
| Platform | Windows only now; core kept OS-agnostic for later ports |
| License / audience | Open source (MIT OR Apache-2.0), university project |
| Search scope (v1) | File and folder **names** only (no content search) |
| Duplicates across devices | Not in v1 |
| Offline data kept | Name, path, size, dates, attributes — no thumbnails |
| Reformatted drive | Treated as a **new** device; old one stays as an offline device, renamable |
| NAS | Read from Windows as a network share (UNC / mapped drive); nothing runs on the NAS |
| Device updates | When a device is available it is read and kept up to date live |
| Hidden/system files | Indexed and **shown** by default; visibility settings come later |
| Background behaviour | Elevated Windows service + tray icon + start with Windows; **no global hotkey** |
| UI stack | Tauri 2 + React + TypeScript |
| UI quality | Professional and polished from the first UI milestone, not a throwaway |
| Sync (later spec) | LAN first; fallback to a self-hosted server per user; no accounts/auth in our infra |
| GitHub | User creates the remote repo; we only `git init` locally |

---

## 3. Architecture

```
┌──────────────────────────────┐        named pipe         ┌───────────────────────────────┐
│ seek-app (Tauri, user level) │ ◀──────────────────────▶ │ seek-service (LocalSystem)    │
│  React UI                    │   \\.\pipe\seek-v1        │  owns all device indexes      │
│  tray, autostart             │   length-prefixed JSON    │  NTFS MFT scan + USN follow   │
│  NAS-share scanner (user     │                           │  FAT/exFAT walker + watcher   │
│  credentials) → pushes index │                           │  volume arrival/removal       │
└──────────────────────────────┘                           │  query engine, persistence    │
                                                           └───────────────────────────────┘
```

**Why a service:** reading the NTFS Master File Table and USN journal requires administrator rights. A
service installed once (one UAC prompt at install) avoids a UAC prompt on every launch — the same model as
Everything.

**NAS caveat:** a service running as LocalSystem in session 0 cannot see drives the user mapped and has no
access to the user's SMB credentials. Therefore **network shares are scanned by the user-level app process**,
which sends the resulting index and subsequent change batches to the service over the pipe. The service
remains the single owner of all indexes and the only place queries run.

**Dev mode:** for development, `seek-service` can also run as a normal console process (started elevated)
with `--console`, so the UI can be developed without installing a service.

### 3.1 Workspace layout

```
seek/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ seek-core/              # OS-agnostic: index, query parser, search, persistence, device model
│  ├─ seek-scan/              # Windows scanners: MFT reader, USN follower, dir walker, watcher, volumes
│  ├─ seek-ipc/               # pipe protocol types + client/server helpers (shared by service & app)
│  ├─ seek-service/           # Windows service / console host
│  └─ seek-cli/               # dev tool: scan, query, bench, dump
├─ app/                       # Tauri 2 app
│  ├─ src-tauri/              # Rust side: pipe client, NAS scanner, tray, autostart
│  └─ src/                    # React + TypeScript UI
└─ docs/
```

Each crate has one job and a narrow public API; `seek-core` has no Windows dependencies so it can be
reused on other OSes and unit-tested anywhere.

---

## 4. Data model

### 4.1 Device

A *device* is one indexed root: a local volume or a network share.

| Field | Notes |
|---|---|
| `id` | Seek-generated UUID v4 — stable forever |
| `identity` | Local volume: `(volume_serial_number, filesystem)`; NTFS also stores the 64-bit serial. Network share: normalised UNC path `\\server\share` |
| `display_name` | User-editable; defaults to volume label or share name |
| `label`, `filesystem`, `total_bytes`, `free_bytes` | Last known values |
| `kind` | `Fixed` / `Removable` / `Network` |
| `last_mount_point` | e.g. `E:\` — used to display paths of offline files |
| `indexed_by_host` | Host UUID of the PC that indexed it (for future sync) |
| `first_seen`, `last_seen`, `index_updated_at` | UTC timestamps |
| `state` | `Online` / `Offline` / `Scanning` |

**Matching rule:** when a volume appears, Seek looks up its identity. Match → same device (comes online,
incremental update). No match → new device. Reformatting changes the serial → new device; the previous one
stays as an offline device the user can rename or delete.

### 4.2 Device index (`<device-id>.seekidx`)

Struct-of-arrays, one row per file/folder:

| Column | Type | Notes |
|---|---|---|
| `name_off`, `name_len` | `u32`, `u16` | slice into a shared UTF-8 name arena |
| `parent` | `u32` | row index of parent folder (`u32::MAX` = root) |
| `size` | `u64` | 0 for folders (folder sizes computed lazily) |
| `modified`, `created` | `i64` | FILETIME → unix ms |
| `attrs` | `u32` | Windows attributes; bit for directory |

Plus a parallel **lower-cased name arena** used for case-insensitive matching, and for NTFS a map
`FRN → row` for applying USN changes. Deleted rows are tombstoned and compacted on save.

Memory estimate: ~60–80 bytes/entry → 10M entries ≈ 0.6–0.8 GB.

### 4.3 Change checkpoint

Per NTFS device: `(usn_journal_id, next_usn)`. On restart the service resumes the journal from that point.
If the journal id changed or the USN is no longer available (journal wrapped), the device is fully rescanned.

### 4.4 Persistence

- Directory: `%ProgramData%\Seek\` — `devices.json` (device registry) + `indexes\<id>.seekidx`.
- Index file: header (magic, format version, device id, host id, checkpoint, timestamps) + raw column
  buffers, zstd-compressed. Read with a single sequential read; no per-entry allocation.
- Written atomically (temp file + rename) every 5 min if dirty, on device removal, and on shutdown.
- Unknown/newer format version → file is ignored and the device rescanned when online (never crash).

---

## 5. Scanning and live updates

| Source | Initial scan | Live updates while online |
|---|---|---|
| NTFS local volume | Read the MFT via `FSCTL_ENUM_USN_DATA` (fast path, no directory walk) | Follow the USN journal (`FSCTL_READ_USN_JOURNAL`), batch changes every ~500 ms |
| FAT32 / exFAT / ReFS / other | Parallel directory walk | `ReadDirectoryChangesW` on root (recursive) |
| Network share | Parallel walk from the **app process** | `ReadDirectoryChangesW` + full rescan whenever the share reconnects; changes forwarded to service |

**Volume arrival/removal:** the service listens for device notifications (`WM_DEVICECHANGE` /
`RegisterDeviceNotification` in a hidden window, or polling `GetLogicalDrives` every 2 s as a fallback). On
arrival → match identity → online + incremental update. On removal → flush index, mark offline.

**Errors:** unreadable folders are skipped and counted (shown in device details); a failed scan leaves the
previous index intact and marks the device with an error state; journal overflow triggers a rescan.

---

## 6. Search

### 6.1 Query language (v1)

| Syntax | Meaning |
|---|---|
| `foo bar` | name contains `foo` AND `bar` (case-insensitive) |
| `foo \| bar` | OR |
| `!foo` | NOT |
| `"exact phrase"` | literal including spaces |
| `*.pdf`, `rep?rt` | wildcards (whole-name match when wildcards are present) |
| `ext:pdf;docx` | extension filter |
| `size:>100mb`, `size:1kb..5mb` | size filter |
| `dm:2024`, `dm:>2024-06-01`, `dm:today` | date-modified filter |
| `path:` prefix on a term | match against full path instead of name |
| `regex:<pattern>` | regular expression on name |
| `device:<name>` | restrict to a device |
| `folder:` / `file:` | type filter |
| `online:` / `offline:` | device state filter |

Parse errors never block typing: the UI shows the error inline and keeps the last good results.

### 6.2 Execution

- Each device index is searched in parallel (rayon), and within a device the name arena is split into
  chunks across threads. Substring matching uses SIMD `memchr::memmem`.
- Cheap filters (`ext`, `size`, `dm`, `folder`) are evaluated before name matching where possible.
- Every new keystroke cancels the in-flight query (generation counter).
- Results are a list of `(device_id, row)` sorted by the requested column (name, path, size, modified,
  device). Sorting by path builds paths lazily and caches them.
- The UI fetches **windows of rows** (`offset`, `limit`) — the service never sends the full result set.

---

## 7. IPC protocol (`seek-ipc`)

Named pipe `\\.\pipe\seek-v1`, ACL restricted to the interactive user + SYSTEM. Messages are
length-prefixed JSON (simple to debug; can switch to a binary codec later without changing the API).

| Request | Response |
|---|---|
| `Search { query, sort, descending }` | `SearchStarted { search_id, total, elapsed_ms }` |
| `FetchRows { search_id, offset, limit }` | `Rows { rows: [ { name, path, size, modified, created, is_dir, device_id, online } ] }` |
| `ListDevices` | `Devices [ … ]` |
| `RenameDevice { id, name }` / `ForgetDevice { id }` / `RescanDevice { id }` | `Ok` / `Error` |
| `AddNetworkShare { unc }` / `PushShareIndex { … }` / `PushShareChanges { … }` | `Ok` / `Error` |
| `Status` | service version, indexing progress, entry counts |
| `Subscribe` | stream of events: `DeviceChanged`, `IndexProgress`, `ResultsInvalidated` |

The protocol carries a version number; the app shows a clear "service out of date" message on mismatch.

---

## 8. User interface

Goal: **professional, clean and efficient** — feels native to Windows 11, fast enough that the UI never
feels like the bottleneck, and visually polished from the first UI milestone.

### 8.1 Visual direction

- Windows 11 native feel: Mica window backdrop (Tauri window effects), custom title bar, rounded corners,
  Segoe UI Variable typography, Fluent-style iconography (file-type icons from the system shell where
  available, otherwise a consistent icon set).
- Light and dark themes following the system setting, built on design tokens (CSS variables).
- Restrained palette: neutral surfaces, one accent colour, clear hierarchy; offline results visibly muted
  with an "offline · last seen 3 days ago" badge rather than hidden.
- Motion is subtle and fast (≤150 ms), respects "reduce motion".

### 8.2 Layout

```
┌────────────────────────────────────────────────────────────────────────────┐
│  ◎ Seek                                                     —  ☐  ✕      │
├──────────────┬─────────────────────────────────────────────────────────────┤
│ DEVICES      │  🔍  report ext:pdf                          [filters ▾]   │
│ ● System (C:)│─────────────────────────────────────────────────────────────│
│ ● Data (D:)  │  Name              Path               Size    Modified  Dev │
│ ○ Backup 2TB │  ▸ report.pdf      D:\Uni\Sem2        1.2 MB  2 Oct     D:  │
│   offline 3d │  ▸ report-v2.pdf   E:\Old             840 KB  2023     ○ B │
│ ● NAS media  │  …  (virtualised, 1M+ rows)                                 │
│              │                                                             │
│ + Add share  │                                                             │
├──────────────┴─────────────────────────────────────────────────────────────┤
│ 12,481 results · 9 ms   ·   3 online · 1 offline   ·   Indexing NAS 42%    │
└────────────────────────────────────────────────────────────────────────────┘
```

- **Search bar:** always focused on open; results update per keystroke; inline syntax highlighting of
  filters; query errors shown under the bar.
- **Results table:** virtualised rows, resizable/sortable columns, highlighted matched substring, file-type
  icon, multi-select. Row actions: Open, Open containing folder, Copy path, Copy name; offline rows disable
  Open and show device + last-seen instead.
- **Device sidebar:** online/offline status dot, capacity bar, last-seen time, entry count; click to filter;
  context menu: Rename, Rescan, Forget; "Add network share" dialog.
- **Status bar:** result count + query time, device summary, background indexing progress.
- **Settings page:** start with Windows, exclude folders, theme override, index location; service status.
- **First-run experience:** explains the one-time admin prompt, installs/starts the service, shows initial
  indexing progress per drive.

### 8.3 Keyboard

`Ctrl+F`/`Esc` focus/clear search, `↑↓` move, `Enter` open, `Ctrl+Enter` open folder, `Ctrl+C` copy path,
`Ctrl+Shift+C` copy name, `Ctrl+1..9` sort column. No global hotkey (by decision).

### 8.4 Design process

Before the UI milestone is implemented, a high-fidelity mockup (light + dark) is produced and reviewed with
the user; implementation follows the approved mockup.

### 8.5 Tray & lifecycle

Closing the window hides to tray; the tray menu offers Open, Pause indexing, Settings, Quit. Autostart via
the Tauri autostart plugin (user-level). The service itself is set to automatic start.

---

## 9. Packaging

- Tauri bundler produces an MSI/NSIS installer that also installs and registers `seek-service`
  (one UAC prompt at install).
- Uninstall stops and removes the service; index data in `%ProgramData%\Seek` is removed on request.

---

## 10. Testing

| Layer | How |
|---|---|
| `seek-core` | Unit tests: query parser (table-driven), matcher semantics, filters, sorting, index serialise/deserialise round-trip, tombstone + compaction |
| `seek-scan` | NTFS reader and USN follower tested against a small VHDX created and mounted by an opt-in, admin-only integration test; walker tested on temp directories |
| `seek-ipc` | Protocol round-trip tests; client/server test over an in-process pipe |
| Performance | `criterion` benchmarks on a synthetic 10M-entry index (query latency) and save/load time |
| UI | Component tests (Vitest + Testing Library) for search bar, table, sidebar; manual checklist for visual review |
| End-to-end | Manual: install, initial index, plug/unplug USB, rename device, search offline results |

---

## 11. Milestones

1. **Core + CLI** — `seek-core` index/search/query parser; `seek-scan` MFT reader; `seek-cli scan` / `query`.
2. **Persistence + live updates** — index files, USN follow, checkpoint resume, FAT/exFAT walker + watcher.
3. **Service + IPC** — `seek-service` (console + service mode), `seek-ipc` protocol, volume arrival/removal.
4. **UI** — mockup review, then the Tauri app per §8 (search, results, sidebar, status bar, tray, settings).
5. **Multi-device** — offline devices, rename/forget, network shares via app process, first-run flow, installer.

Later, separate specs: LAN sync → self-hosted server sync → macOS/Linux ports → content search / duplicates.

---

## 12. Future-proofing for sync (not built now)

- Every device index records `indexed_by_host` and `index_updated_at`; a host has a persistent UUID.
- Index files are self-contained and versioned, so they can be transferred between PCs as-is.
- Intended merge rule: per device, the snapshot with the newest `index_updated_at` wins.
- `seek-core` has no Windows dependencies, so the same index format can be read on other OSes.

---

## 13. Risks

| Risk | Mitigation |
|---|---|
| MFT/USN APIs are intricate | Use `FSCTL_ENUM_USN_DATA` (documented, simpler than raw MFT parsing); keep a walker fallback |
| Memory use with many devices | Compact struct-of-arrays layout; benchmark at 10M entries early (milestone 1) |
| Service ↔ NAS credential gap | NAS scanning lives in the user-level app process (§3) |
| Tauri window effects vary by Windows build | Fall back to a solid themed background when Mica is unavailable |
| Scope creep into sync | Sync explicitly deferred to its own spec |
