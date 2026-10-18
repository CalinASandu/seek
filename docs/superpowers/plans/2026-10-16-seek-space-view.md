# Space view Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> Execution: **native** (user, 2026-10-16: "write the plan and then build it"). The implementer is the author of
> this plan and has the code in context, so steps give exact interfaces and test cases; code is written in TDD
> order during execution. **Never `git commit`** (repo rule) — "Commit" steps are replaced by running the gates.

**Goal:** A Space view that shows what fills any indexed drive (local, offline, other PCs) as a sunburst + list,
computed from the index, in the app's Apple-style design.

**Architecture:** `seek-core::space` computes per-folder totals in one O(n) pass (CSR child lists + post-order)
and builds size-sorted trees / largest files / type totals. The service caches totals per (device, user) like
the hidden-row masks and answers three new IPC requests. The React app adds a Search | Space switch and a
`space/` component set; sunburst geometry is a pure, tested module.

**Tech Stack:** Rust 2021 (seek-core, seek-ipc, seek-service, seek-cli), Tauri 2, React 18 + TypeScript, Vitest.

**Spec:** `docs/superpowers/specs/2026-10-16-seek-space-view-design.md`

## Global Constraints

- `seek-core` stays free of Windows dependencies. Timestamps are unix epoch milliseconds.
- IPC JSON is camelCase (`rename_all = "camelCase", rename_all_fields = "camelCase"`), tagged by `type`.
- UI uses only existing tokens in `app/src/styles/tokens.css` (system colors, `--spring`, materials); light,
  dark, increased contrast and reduced motion must all work.
- Read-only: no delete or move actions anywhere in this feature.
- Gates after every task: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
  and for app tasks `pnpm -C app test` + `pnpm -C app build`.

## Review Focus

1. Folder deleted / renamed while open → request errors with "That folder no longer exists"; UI climbs to the
   nearest existing parent instead of showing an error screen. (Task 3 test, Task 7 test.)
2. Non-elevated user on a drive with other users' profiles → no hidden names anywhere (tree, largest, types),
   one "Other users" node carries their bytes, totals still add up. (Task 1 + Task 3 tests.)
3. Index changes (compaction → new epoch) between the totals cache and a request → cache recomputed, never
   stale row numbers. (Task 1 `is_current` test, Task 3 cache keyed on epoch + len.)
4. Empty drive / empty folder / drive still scanning → friendly empty or "Measuring…" state, no NaN angles.
   (Task 6 zero-total test, Task 7 test.)
5. Huge folders (100k children) → level 1 capped at 500 items + one "smaller items" node, the sunburst
   merges slivers < 0.5°. (Task 1 cap test, Task 6 merge test.)

---

### Task 1: `seek-core::space` — totals, tree, largest, types

**Files:**
- Create: `crates/seek-core/src/space.rs`
- Modify: `crates/seek-core/src/lib.rs` (add `pub mod space;` + re-exports)
- Modify: `crates/seek-core/src/model.rs` (attribute constants, if missing)

**Interfaces (produces):**
```rust
pub const ATTR_OFFLINE: u32 = 0x1000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x40000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x400000;

pub struct FolderTotals { /* bytes, files, cloud, hidden_bytes, hidden_files per row + virtual root slot,
                              CSR child lists, epoch, len */ }
impl FolderTotals {
    /// `hidden[row] == true` rows are excluded from named nodes; their bytes go to the parent's hidden bucket.
    pub fn compute(ix: &DeviceIndex, hidden: Option<&[bool]>) -> FolderTotals;
    pub fn is_current(&self, ix: &DeviceIndex) -> bool;
    pub fn bytes(&self, folder: Option<u32>) -> u64;   // None = drive root
    pub fn files(&self, folder: Option<u32>) -> u64;
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)] #[serde(rename_all = "camelCase")]
pub struct SpaceNode { pub name: String, pub kind: SpaceKind, pub bytes: u64, pub files: u64,
                       pub modified: i64, pub cloud: bool, pub children: Vec<SpaceNode> }
#[serde(rename_all = "camelCase")] pub enum SpaceKind { Folder, File, Smaller { count: u64 }, Hidden }
pub fn space_tree(ix: &DeviceIndex, t: &FolderTotals, folder: Option<u32>, depth: u8) -> SpaceNode;
pub fn largest_files(ix: &DeviceIndex, folder: Option<u32>, limit: usize, hidden: Option<&[bool]>) -> Vec<u32>;
#[serde(rename_all = "camelCase")] pub enum TypeCategory { Video, Pictures, Music, Documents, Archives, Apps, System, Other }
pub struct TypeTotal { pub category: TypeCategory, pub bytes: u64, pub files: u64 } // serde camelCase
pub fn type_totals(ix: &DeviceIndex, folder: Option<u32>, hidden: Option<&[bool]>) -> Vec<TypeTotal>; // bytes desc, zero omitted
pub fn type_of(name_lower: &str) -> TypeCategory;
```
Tree rules: level-1 children sorted by bytes desc, max 500 then one `Smaller`; deeper levels merge children
under 0.5 % of the requested folder's bytes into `Smaller`; `Hidden` node (name "Other users") appended where
hidden bytes > 0; files are `File` with no children; `depth` counts levels below `folder`.

- [ ] **Step 1: Write failing tests** in `space.rs` `mod tests` with a small hand-built index
  (`DeviceIndex::empty` + `push_child`) and `synth::` for random checks:
  - `totals_match_brute_force` — random synthetic index; for every folder, totals == sum of sizes of live
    files below (walk parents).
  - `parent_after_child_rows_are_handled` — folder moved under a later row via `apply` (Change::Upsert with a
    new parent) still sums correctly.
  - `dead_rows_are_ignored` — deleted file not counted.
  - `hidden_rows_go_to_other_users` — mask a folder: tree has no node with its name or its files' names; root
    has `Hidden` node with its bytes; root bytes unchanged.
  - `tree_sorted_and_merged` — 1 big + 300 tiny files at depth 2 → tiny ones become one `Smaller{count:300}`.
  - `level_one_is_capped_at_500`.
  - `cloud_flag_propagates` — one file with `ATTR_RECALL_ON_DATA_ACCESS` marks all ancestors `cloud`.
  - `largest_files_in_subtree` — order desc, limited, only inside folder, hidden excluded.
  - `type_totals_sum_to_folder_total` and `type_of` table (`mkv`→Video, `heic`→Pictures, `flac`→Music,
    `pdf`→Documents, `7z`→Archives, `exe`/`dll`→Apps, `sys`/`pagefile.sys`→System, none→Other).
  - `is_current_false_after_compact`.
- [ ] **Step 2:** `cargo test -p seek-core space` → fails (module missing).
- [ ] **Step 3:** Implement: CSR (`offsets: Vec<u32>` len n+2, `kids: Vec<u32>`, slot n = virtual root for
  `NO_PARENT`), iterative post-order DFS from the root slot over live rows, accumulating bytes/files/cloud,
  hidden subtrees summed into the parent's hidden bucket. Tree: sort child slice by bytes desc per node built.
- [ ] **Step 4:** `cargo test -p seek-core space` → pass; clippy clean.

### Task 2: IPC protocol

**Files:** Modify `crates/seek-ipc/src/protocol.rs`.

**Interfaces (produces):**
```rust
Request::SpaceTree { device_id: DeviceId, path: String, depth: u8 }
Request::SpaceLargest { device_id: DeviceId, path: String, limit: u32 }
Request::SpaceTypes { device_id: DeviceId, path: String }
Response::SpaceTree { node: SpaceNode }
Response::SpaceFiles { files: Vec<ResultRow> }
Response::SpaceTypes { types: Vec<TypeTotal> }
```
`path` is a full folder path; the drive root is its mount point (e.g. `D:\`).

- [ ] **Step 1:** Add a round-trip test asserting JSON shape:
  `{"type":"spaceTree","deviceId":"…","path":"D:\\Games","depth":3}` and a `SpaceKind::Smaller{count}` node
  serialises as `{"smaller":{"count":3}}` (or the chosen tagged form, mirrored in TS).
- [ ] **Step 2:** Implement the variants; `cargo test -p seek-ipc` passes.

### Task 3: Service handlers + totals cache

**Files:**
- Modify: `crates/seek-service/src/state.rs` (cache `space_totals: Mutex<HashMap<(DeviceId, String), Arc<FolderTotals>>>`)
- Modify: `crates/seek-service/src/server.rs` (three handlers, `spawn_blocking`)
- Test: `crates/seek-service/tests/service.rs`

**Interfaces:**
- Consumes: Task 1 functions, `State::hidden_for`, `State::hidden_mask`, `DeviceIndex::find_folders`, `materialize`.
- Produces: `State::space_totals(&self, device: &Device, ix: &DeviceIndex, sid: &str, mask: Option<&[bool]>) -> Arc<FolderTotals>`
  (recomputed when `!is_current(ix)`).
- Path resolution: trimmed path equal to the mount (case-insensitive) → `None` (root); else `find_folders`;
  not found or hidden → `err("That folder no longer exists")`; device unknown → `err("Unknown drive")`.

- [ ] **Step 1: Failing tests** in `tests/service.rs` (fake drive `T:\`, folder1..10, files `id % 10 + 1`):
  - `space_tree_sums_the_drive` — root bytes == Σ 11..=1000 file sizes; 10 folder children sorted desc;
    `folder1` (files id%10==0 → sizes 20,30,…,1000) is first.
  - `space_tree_of_a_folder_and_unknown_folder` — `T:\folder3` has 99 file children (depth 1); `T:\nope` → error text.
  - `space_largest_and_types` — largest 5 under root are file1000, 999, …; types: one `Documents` (txt) total.
  - `space_hides_other_users` — profiles as in `other_users_folders_are_hidden_unless_elevated`: no
    `folder2` node, an `Other users` node with folder2's bytes; `T:\folder2` → error; elevated sees it.
- [ ] **Step 2:** `cargo test -p seek-service --test service space` → fail.
- [ ] **Step 3:** Implement handlers (snapshot device by id, read lock, hidden mask, totals cache, build response).
- [ ] **Step 4:** Tests pass; whole workspace tests + clippy.

### Task 4: Perf section

**Files:** Modify `crates/seek-cli/src/perf.rs`.
- [ ] Add `== space ==`: `FolderTotals::compute` on synthetic 2M and 10M, `space_tree(depth 3)` at root,
  `largest_files(50)`, `type_totals` — printed and saved in `--json` like other metrics. Targets: totals 2M
  < 150 ms, tree < 20 ms. Run `seek perf` once and record numbers in NEXT_STEPS.

### Task 5: App API, Tauri commands, mock

**Files:** `app/src/api/types.ts`, `app/src/api/tauri.ts`, `app/src/api/mock.ts`,
`app/src-tauri/src/commands.rs`, `app/src-tauri/src/lib.rs`, test `app/src/api/mock.test.ts` (or existing).

**Interfaces (produces, TS):**
```ts
export type SpaceKind = "folder" | "file" | { smaller: { count: number } } | "hidden";
export interface SpaceNode { name: string; kind: SpaceKind; bytes: number; files: number; modified: number; cloud: boolean; children: SpaceNode[] }
export type TypeCategory = "video" | "pictures" | "music" | "documents" | "archives" | "apps" | "system" | "other";
export interface TypeTotal { category: TypeCategory; bytes: number; files: number }
SeekApi.spaceTree(deviceId: string, path: string, depth: number): Promise<SpaceNode>
SeekApi.spaceLargest(deviceId: string, path: string, limit: number): Promise<ResultRow[]>
SeekApi.spaceTypes(deviceId: string, path: string): Promise<TypeTotal[]>
```
- [ ] Tauri commands `space_tree`, `space_largest`, `space_types` (Response → JSON), registered.
- [ ] Mock: deterministic fake tree per mock drive (Users, Games, Program Files, Windows, …), consistent bytes.
- [ ] Test: mock tree child bytes sum ≤ parent; unknown path rejects with "That folder no longer exists".

### Task 6: Sunburst geometry (`app/src/lib/sunburst.ts`)

**Interfaces (produces):**
```ts
export interface Arc { key: string; path: string[]; depth: number; a0: number; a1: number; r0: number; r1: number;
                       hue: number; tone: number; node: SpaceNode }
export function layoutSunburst(root: SpaceNode, rings: number, size: number, minAngle?: number): Arc[];
export function arcPath(a: Arc, cx: number, cy: number): string; // SVG path, handles full circle
export function lerpArcs(from: Arc[], to: Arc[], t: number): Arc[]; // by key; missing keys grow/shrink from parent
```
- [ ] Tests: angles of children sum to parent span; zero-byte root → no arcs (no NaN); slivers < minAngle
  merged into one "smaller" arc; hue index follows first-ring order, deeper rings keep the ancestor hue with
  higher tone; full-circle single child produces a valid path.

### Task 7: Space UI + integration

**Files:**
- Create: `app/src/components/ViewSwitch.tsx`, `app/src/components/space/{SpaceView,Sunburst,SpaceList,LargestList,TypeList,CapacityBar,PathBar}.tsx`,
  `app/src/hooks/useSpace.ts`, `app/src/styles/space.css`
- Modify: `app/src/App.tsx` (view state, `Ctrl+Shift+S`, sidebar drive click opens drive in Space mode),
  `app/src/components/Sidebar.tsx` (selected drive highlight in Space mode), `app/src/main.tsx` (import css)
- Test: `app/src/test/space.test.tsx`

Behaviour per spec (layout, hover sync, drill animation 420 ms `--spring`-like easing via rAF, reduced motion
instant, context menu: Show in Explorer (local only) / Copy path / Search in this folder → Search view with
`path:"<folder>"`, keyboard: ↑/↓, Enter/→ in, Backspace/Alt+↑/← up, states: measuring / empty / remote as-of).

- [ ] Tests (mock API): switch to Space shows the first drive; clicking a folder row drills in (path bar
  shows it); Backspace goes up; clicking center goes up; hover on row highlights the matching arc
  (`data-active`); remote drive row menu has no "Show in Explorer"; "Search in this folder" switches view and
  sets the query; folder removed (mock rejects) → view climbs to parent.
- [ ] Implement, `pnpm -C app test`, `pnpm -C app build`.

### Task 8: Visual pass + docs

- [ ] Run `pnpm dev` (mock) → screenshot Space in light + dark at 1200×760 and 900×560; critique against the
  spec's design rules (alignment, type scale, contrast, motion); fix; repeat until clean.
- [ ] Build the real window (`pnpm tauri build --no-bundle`), check against the running dev service on real drives.
- [ ] Update `docs/NEXT_STEPS.md` (wave 6: Space view, perf numbers, how to try it) and README feature list.
- [ ] Final gates: workspace tests, clippy, app tests + build; delete stray empty files in repo root.
