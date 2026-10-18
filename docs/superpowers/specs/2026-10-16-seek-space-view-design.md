# Seek — Space view (design)

_2026-10-16. Approved in conversation; this file is the written spec._

## Goal

Show what fills each drive, instantly, from the index Seek already has: every drive on this PC, offline drives,
and the drives of linked PCs (even while those PCs are off). No disk access, no separate scan. Read-only in this
version; deleting arrives later with the Cleanup feature (reversible, via Recycle Bin / quarantine).

Success: opening Space on C: (2.1M entries) shows the sunburst in < 150 ms the first time and < 20 ms after;
drilling feels instant; the view looks and moves like a native macOS app (same tokens as the rest of Seek).

## Decisions (user, 2026-10-16)

1. Entry: a **Search | Space** segmented control in the main header; `Ctrl+Shift+S` toggles.
2. Visual: DaisyDisk-style **sunburst** (left) + ranked **list** (right), drill in on either, breadcrumb path.
3. Scope: **all drives**: local, offline, other PCs. Open / reveal only for local items.
4. **Read-only**: Show in Explorer, Copy path, Search in this folder. No delete.
5. Extra panels: **Largest files** and **By type**.
6. Cloud placeholders: logical size; folders containing cloud-only files get a cloud badge.

## Design rules (Apple HIG, applied)

- Charts: keep the chart simple and reveal detail on demand → 3 rings at a time, detail on hover / in the list.
- Every chart accessible → the list is the accessible representation; the SVG is `role="img"` with a summary label.
- Never use color alone for meaning → every slice has a named row; hover links both.
- Color: system colors only, light + dark + increased-contrast variants (already in `tokens.css`).
- Segmented control in the toolbar to switch views; tooltips on each segment.
- Materials: the view lives on the solid content sheet; no extra glass. Motion: the existing `--spring`
  curve, transform/opacity or rAF-interpolated arc angles only; reduced motion → instant.

## Layout

```
┌ Sidebar ─────┬──────────────────────────────────────────────────────────┐
│ Drives       │ [ Search | Space ]          Desktop › D: › Games         │
│ ● D: Data    │ ▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬░░░░  412 GB of 931 GB used            │
│ ○ E: Backup  │ ┌────────────────────────┐  Contents · Largest · By type │
│ Laptop       │ │      sunburst,         │  ▸ Steam       182 GB  ████▊  │
│ ○ C: System  │ │      3 rings           │  ▸ Epic         61 GB  █▍     │
│              │ │      ( Games )         │    setup.iso   4.2 GB  ▏      │
│              │ │      ( 251 GB )        │    smaller items 1.1 GB       │
│              │ └────────────────────────┘                               │
└──────────────┴──────────────────────────────────────────────────────────┘
```

- **Header**: segmented control (left), breadcrumb path control (right; each segment clickable).
- **Capacity bar** (drive root only, when `totalBytes > 1`): thin rounded bar segmented by type category
  (same colors as By type), free space as the unfilled track, caption `412 GB of 931 GB used`.
- **Sunburst**: 3 rings around a center disk. Center: folder name + total size; click center = up one level.
  Each first-ring folder takes the next system hue (blue, indigo, purple, pink, orange, yellow, green, teal,
  cyan, mint), its descendants the same hue lighter per ring; files are neutral gray; merged small items are a
  translucent gray slice. Hover: slice + row highlight, others dim to 40%. Click a folder slice = drill in.
  Drill animation: arc angles interpolate from the clicked slice to the full circle (420 ms spring).
- **Inspector** (right) with a small segmented control **Contents · Largest · By type**:
  - Contents: children of the current folder by size: icon, name, thin inline bar (share of the folder),
    size (tabular numerals), `smaller items` row last. Folders have a chevron; Enter / double-click drills in.
  - Largest: the 50 biggest files in the subtree with their folder path (muted).
  - By type: categories with bars and totals.
- **Context menu** on rows and slices: Show in Explorer (local only), Copy path, Search in this folder
  (switches to Search with `path:"<folder>"`).
- **States**: drive scanning → "Measuring D:…" with the existing progress; empty folder → "This folder is
  empty"; remote PC offline → data still shown, with a muted "as of <last sync>" caption.
- **Keyboard**: `Ctrl+Shift+S` toggle view; ↑/↓ in list; Enter / → drill in; Backspace / Alt+↑ / ← up;
  Ctrl+C copy path.
- In Space mode, clicking a drive in the sidebar opens it (instead of adding a `drive:` filter token).

## Core: `crates/seek-core/src/space.rs` (OS-agnostic)

```rust
pub struct FolderTotals { pub bytes: Vec<u64>, pub files: Vec<u32>, epoch: u64, len: usize }
impl FolderTotals {
    pub fn compute(ix: &DeviceIndex, hidden: Option<&[bool]>) -> FolderTotals;
    pub fn is_current(&self, ix: &DeviceIndex) -> bool; // epoch + len unchanged
}
pub struct SpaceNode { name, row: Option<u32>, is_dir, bytes, files, modified, cloud: bool,
                       kind: NodeKind /* Item | Smaller{count} | Hidden */, children: Vec<SpaceNode> }
pub fn tree(ix, totals, folder_row, depth: u8, min_fraction: f64) -> SpaceNode;
pub fn largest(ix, folder_row, limit, hidden) -> Vec<u32>;         // top-K heap over the subtree
pub fn by_type(ix, folder_row, hidden) -> Vec<(TypeCategory, u64, u32)>;
pub fn type_of(name: &str) -> TypeCategory; // extension table: Video, Pictures, Music, Documents,
                                            // Archives, AppsCode, System, Other
```

- Totals: one pass, each live file adds its size to every ancestor (depth ≈ 10 → ~20M adds for C:).
  Rows hidden by the access mask are excluded from named items and summed into one `Hidden` node
  ("Other users") at the level where they start, so totals stay true.
- `min_fraction` default 0.005 of the current folder: smaller siblings merge into one `Smaller` node.
- Cloud badge: attribute bits `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` (0x400000) / `OFFLINE` (0x1000) on any
  file below (computed in the same pass as a bit per folder). Walker-scanned drives may lack it; then no badge.

## Service + IPC

- New requests (camelCase JSON like the rest):
  `SpaceTree { deviceId, path, depth }` → `SpaceTree { node, deviceTotal, epoch }`;
  `SpaceLargest { deviceId, path, limit }` → `SpaceFiles { files: Vec<ResultRow> }`;
  `SpaceTypes { deviceId, path }` → `SpaceTypes { types: Vec<{category, bytes, files}> }`.
  `path` is the folder's full path (stable across compaction); resolved with `find_folders`; unknown path →
  `Error { "That folder no longer exists" }` and the UI climbs to the nearest existing parent.
- `state.rs` caches `FolderTotals` per (device, access class) next to the existing row masks; invalidated by
  epoch / row count. Computed off the request thread for large devices like search.
- Live updates: the existing `ResultsInvalidated` event makes the UI refetch the current folder (debounced 1 s).

## App

- `api/types.ts` + `tauri.ts` + `mock.ts`: `spaceTree`, `spaceLargest`, `spaceTypes`; the mock builds a fake
  tree for every mock drive (browser preview and tests).
- Components: `ViewSwitch.tsx`, `space/SpaceView.tsx` (state: device, path stack, hover, tab),
  `space/Sunburst.tsx` (SVG arcs, layout math in `lib/sunburst.ts`, pure + unit-tested),
  `space/SpaceList.tsx`, `space/LargestList.tsx`, `space/TypeList.tsx`, `space/CapacityBar.tsx`,
  `space/PathBar.tsx`. Styles in `styles/space.css` using existing tokens only.

## Testing

- Core: totals equal a brute-force sum (synthetic index); hidden rows go to `Hidden`; merging threshold;
  `largest` order; `by_type` sums equal folder total; cloud bit propagation; totals stale after `apply`.
- Service: each request on a test device, including access filtering and an unknown path.
- App (Vitest): toggle view, drill in via list and slice, up via center/Backspace, hover sync, remote item has
  no "Show in Explorer"; `lib/sunburst.ts` angle math.
- Perf: `seek perf` gets `== space ==` (totals on synthetic 2M / 10M, tree fetch, largest, types).
- Visual: screenshots of the real window light + dark, critique against the rules above, fix, repeat.

## Out of scope

Deleting / Recycle Bin, duplicates, AI suggestions (Cleanup feature); per-user ownership of files; size on
disk (allocated) vs logical size.
