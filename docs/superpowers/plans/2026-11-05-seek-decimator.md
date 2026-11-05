# Seek Decimator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task, with `model: sonnet` coding agents. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A third view, Decimator, that removes duplicates and leftovers to the Recycle Bin, uninstalls apps, and can never remove what the user did not see and tick.

**Architecture:** A new Windows-only crate, `crates/seek-clean`, holds every rule that decides what may be removed, and is the only code that can delete. `app/src-tauri` drives it from the window process. The service gains two read-only requests. React is a client of `prepare` and `commit`, with no way around them.

**Tech Stack:** Rust 1.85 workspace; `windows` 0.62 (already in `Cargo.lock` through Tauri) for COM and shell; `blake3`; Tauri 2; React with vitest and the existing mock API.

**Spec:** `docs/superpowers/specs/2026-11-04-seek-cleanup-design.md`. Read it before any task. Where this plan is silent, the spec decides.

## Global Constraints

- Coding agents never commit. The user approved (2026-11-05) the orchestrator committing each task to `feat/v1` after its review passes, with no pushing. Every step list below ends at a passing check.
- `seek-core` stays free of Windows dependencies. Timestamps are unix epoch **milliseconds** everywhere.
- `seek-clean` is used only by `app/src-tauri`. `seek-service` never depends on it and never deletes anything for Cleanup.
- The engine has no permanent-delete call. The only permanent delete in the whole feature is the off-by-default step in `hooks.nsh` (Task 15).
- Nothing starts ticked, at any confidence level, in any tab. Suggestions are labelled and never pre-selected.
- The word "safe" never describes a removable item in the UI.
- Normal test runs never touch the real Recycle Bin. Real recycling tests are `#[ignore]`.
- Minimum duplicate size is 1 MB (1,048,576 bytes), a fixed constant named `MIN_DUPLICATE_BYTES`.
- Idle RAM does not go up: no index stays loaded for Cleanup while the window is hidden.
- UI: Apple-style, existing tokens (`--spring`, `--t`, `--t-spring`), `Segmented` for every selector, dark beam `#aeaeb2`. Read `emil-design-eng` before building a new panel or animation. No animation on tab change or keyboard list moves.
- Anything containing a backslash or non-ASCII text goes through the Write or Edit tools, never a heredoc, inline script or `Set-Content`.
- After agents run, delete stray empty files from the repo root.
- Full check: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, and `pnpm test` in `app/`.

## Review Focus

These are not spelled out in the spec's test list. Each has its test in the owning task.

1. **Path spellings around protected places:** `c:\WINDOWS\system32\x`, `C:\Users\me\..\..\Windows\x`, `C:\PROGRA~1\x`, `\\?\C:\Windows\x` and a trailing slash must all count as protected, while `C:\Program Files Extra\x` must not. On a non-English Windows the folders come from known-folder lookups, never hard-coded names. (Task 2)
2. **The file is in use, read-only, or denied at removal time:** the item stays, its reason is shown in plain words, and nothing is half-removed. (Task 4)
3. **The kept copy vanishes, changes, or its drive is unplugged between review and removal:** the copy that was going to be removed stays. (Task 3)
4. **A junction or symlink inside a leftover folder, or a candidate that is one:** sizes and "contains programs" never follow it, and removal never touches its target. (Tasks 3, 11)
5. **`cleanup-history.json` is corrupt, huge, or written by two windows at once:** the sheet still opens, the bad file is set aside rather than overwritten, and a removal still succeeds and is recorded. (Task 16)

---

## File map

```
crates/seek-clean/src/
  lib.rs          re-exports; MIN_DUPLICATE_BYTES
  paths.rs        canonical form and comparison of Windows paths
  protect.rs      protected places (rule 8), one table
  model.rs        Candidate, ReviewItem, Prepared, ItemOutcome, Why, Lock
  engine.rs       prepare/commit, tokens, rules 1, 3, 4, 5, 7
  facts.rs        traits Recycler, Elevator, FileFacts, ClaimCheck, and the real FileFacts
  recycle.rs      real Recycler: IFileOperation, sink, pre-checks
  dupes.rs        narrowing, BLAKE3, suggestion, freed-space figure
  claims.rs       what claims a folder; the always-claimed table
  confidence.rs   the points table and levels
  leftovers.rs    candidate folders, walking, scoring
  apps.rs         registry listing, install folder, uninstall command parsing, running it
  history.rs      cleanup-history.json, put back
  elevated.rs     job file, every re-check
crates/seek-core/src/same_size.rs
app/src-tauri/src/cleanup.rs              Tauri commands, scan state, events
app/src/api/{types,tauri,mock,mockClean}.ts
app/src/hooks/useCleanup.ts
app/src/components/clean/
  CleanView.tsx DuplicatesTab.tsx LeftoversTab.tsx AppsTab.tsx
  ReviewSheet.tsx UninstallSheet.tsx RemovedSheet.tsx ConfidenceMeter.tsx
app/src/styles/clean.css
app/src/test/clean.test.tsx
```

## Shared interfaces

These are the names every later task relies on.

```rust
// crates/seek-clean/src/model.rs
pub enum Why { Duplicate, Leftover, Uninstall, StaleData }
pub enum Lock { SystemFolder, ProgramFiles, OtherUser, AppData, ToolFolder, NoRecycleBin, CloudPlaceholder, Protected, LinkedPc }
pub enum Candidate {
    /// Every copy of one group, removable or not. Locked copies still count as copies.
    Duplicates { copies: Vec<PathBuf> },
    Folder { path: PathBuf, why: Why, level: Option<Level>, reasons: Vec<Reason> },
    /// Seek's own stale file in ProgramData. Needs admin.
    StaleFile { path: PathBuf },
    /// A file on a linked PC. Always refused, kept here so the rule has one place.
    LinkedPc { path: String },
}
pub struct ReviewItem { pub id: u32, pub path: String, pub size: u64, pub modified_ms: i64, pub is_dir: bool,
    pub files: u32, pub why: Why, pub lock: Option<Lock>, pub needs_admin: bool, pub group: Option<u32>,
    pub level: Option<Level>, pub reasons: Vec<Reason> }
pub struct Prepared { pub token: String, pub items: Vec<ReviewItem> }
pub enum Outcome { Moved { bin_path: String }, Left { reason: String } }
pub struct ItemOutcome { pub id: u32, pub path: String, pub size: u64, pub outcome: Outcome }
pub struct CommitResult { pub items: Vec<ItemOutcome>, pub moved_bytes: u64, pub stopped_early: Option<String> }
pub struct JobItem { pub id: u32, pub path: String, pub size: u64, pub modified_ms: i64, pub why: Why }
pub struct ElevatedJob { pub user_sid: String, pub items: Vec<JobItem> }
```

All of them derive `Serialize` and `Deserialize` with `rename_all = "camelCase"`. `model.rs` also holds `Level`, `Reason`, `DupCopy`, `DupGroup`, `LeftoverRow` and `AppRow`, field for field the TypeScript types in Task 6, which mirror all of this in `app/src/api/types.ts`.

```rust
// crates/seek-clean/src/facts.rs
pub trait Recycler: Send + Sync {
    /// Per-item results in order. Stops at the first item it can't confirm in the bin.
    fn recycle(&self, paths: &[PathBuf]) -> Vec<Result<String, String>>;
}
pub trait Elevator: Send + Sync { fn run(&self, job: &ElevatedJob) -> Result<Vec<ItemOutcome>, String>; }
pub trait FileFacts: Send + Sync {
    fn stat(&self, path: &Path) -> Option<Stat>;         // never follows reparse points
    fn same_bytes(&self, a: &Path, b: &Path) -> Result<bool, String>;
    fn file_id(&self, path: &Path) -> Option<(u32, u64)>; // volume serial, file ID
    fn recycle_block(&self, path: &Path, batch_bytes: u64) -> Option<Lock>; // the pre-checks
}
pub trait ClaimCheck: Send + Sync { fn is_claimed(&self, folder: &Path) -> Option<String>; }
pub struct Stat { pub size: u64, pub modified_ms: i64, pub is_dir: bool, pub files: u32, pub reparse: bool, pub cloud: bool }
```

Tests use in-memory fakes of `Recycler`, `Elevator` and `ClaimCheck`, plus the real `FileFacts` on temp folders.

```ts
// app/src/api/types.ts, added to SeekApi
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
uninstallState(appId: string): Promise<"running" | "finished" | "unresolved">;
uninstallLeftovers(appId: string): Promise<LeftoverRow[]>;
openRecycleBin(): Promise<void>;
```

`Settings` gains `protectedPaths: string[]`; the Rust field is `protected_paths`, with `#[serde(default)]`.

---

### Task 1: Spike: what does Windows really report when recycling?

The spec makes this first, because rule 3 rests on the answer. **Stop and report to the orchestrator before Task 4 if nothing can confirm an item is in the bin.**

**Files:**
- Create: `crates/seek-clean/Cargo.toml`, `src/lib.rs`, `src/recycle.rs`, `tests/recycle_real.rs`
- Modify: root `Cargo.toml` (member, `seek-clean` workspace dependency, `windows = "0.62"`, `blake3 = "1"`)

**Interfaces:**
- Produces: `recycle::recycle_one(path: &Path) -> Result<RecycleReport, String>`, `recycle::restore(bin_path: &str, original: &Path) -> Result<(), String>`

- [ ] **Step 1:** Create the crate. `windows` features: `Win32_UI_Shell`, `Win32_System_Com`, `Win32_Storage_FileSystem`, `Win32_Foundation`, `Win32_UI_Shell_PropertiesSystem`, `Win32_System_Registry`, `Win32_System_Threading`.
- [ ] **Step 2:** Implement `recycle_one` with `IFileOperation`, `FOFX_RECYCLEONDELETE | FOF_NO_UI | FOFX_EARLYFAILURE`, and an `IFileOperationProgressSink` that records `PreDeleteItem` flags and `PostDeleteItem`'s `psiNewlyCreated` path and `hrDelete`. COM is initialized apartment-threaded on a dedicated thread.
- [ ] **Step 3:** Write `#[ignore] real_recycle_reports_a_location_and_restores` in `tests/recycle_real.rs`: create a temp file on a fixed drive, recycle it, assert the original is gone, assert the reported path is under `$Recycle.Bin`, restore through the Recycle Bin shell folder's own verb (Spec: *Undo*), assert the contents match.
- [ ] **Step 4:** Write `#[ignore] real_oversized_file_is_visible_to_the_sink`, which logs `PreDeleteItem`'s flags for a file larger than the drive's bin `MaxCapacity` if the free space allows it, else records "not reproducible here". It never changes the user's bin settings.
- [ ] **Step 5:** Run: `cargo test -p seek-clean --test recycle_real -- --ignored --nocapture`. Expected: the first test passes.
- [ ] **Step 6:** Add a short *Spike result* section to the spec with what the sink really reported, and whether `PostDeleteItem` or the bin's shell folder is the confirmation used in Task 4.

### Task 2: Path comparison and protected places

**Files:**
- Create: `crates/seek-clean/src/paths.rs`, `src/protect.rs`, `src/model.rs`

**Interfaces:**
- Produces: `paths::canon(p: &Path) -> String` (lowercased, `\\?\` stripped, `.`/`..` resolved, 8.3 names expanded with `GetLongPathNameW` when the path exists, no trailing slash), `paths::is_inside(child: &str, parent: &str) -> bool` (whole-segment), `protect::Places::from_system() -> Places`, `Places::for_tests(...)`, `protect::lock_for(places, canon_path, Why, user_protected: &[String]) -> Option<Lock>`, `protect::never_listed(places, canon_path) -> bool`.

- [ ] **Step 1: Write the failing tests** in `protect.rs`, one per row of the spec's protected-places table, for both columns, plus Review Focus 1:

```rust
#[test] fn spellings_of_windows_all_lock() {
    for p in [r"c:\WINDOWS\system32\x", r"C:\Users\me\..\..\Windows\x", r"\\?\C:\Windows\x", r"C:\Windows\"] {
        assert_eq!(lock(p, Why::Duplicate), Some(Lock::SystemFolder), "{p}");
    }
}
#[test] fn a_lookalike_folder_is_not_program_files() { assert_eq!(lock(r"C:\Program Files Extra\x", Why::Duplicate), None); }
#[test] fn program_files_locks_duplicates_but_not_leftovers() {
    assert_eq!(lock(r"C:\Program Files\App\a.bin", Why::Duplicate), Some(Lock::ProgramFiles));
    assert_eq!(lock(r"C:\Program Files\App", Why::Leftover), None);
}
#[test] fn inside_a_tool_folder_locks_at_any_depth() { /* .git, node_modules, target, .venv */ }
#[test] fn user_protected_folder_locks_everything_inside_it() {}
#[test] fn other_users_profile_locks_and_your_own_does_not() {}
#[test] fn drive_root_is_never_listed() {}
```

- [ ] **Step 2:** Run `cargo test -p seek-clean protect`. Expected: compile failure on the missing items.
- [ ] **Step 3:** Implement. `Places::from_system` reads `FOLDERID_Windows`, `FOLDERID_ProgramFiles`, `FOLDERID_ProgramFilesX86`, `FOLDERID_UserProfiles`, `FOLDERID_Profile`, `FOLDERID_ProgramData`, `FOLDERID_LocalAppData`, `FOLDERID_RoamingAppData`. No hard-coded names, except in `for_tests`.
- [ ] **Step 4:** Run `cargo test -p seek-clean protect`. Expected: PASS.

### Task 3: The engine: prepare and commit

**Files:**
- Create: `crates/seek-clean/src/engine.rs`, `src/facts.rs`

**Interfaces:**
- Consumes: Task 2.
- Produces: `Engine::new(places, Arc<dyn Recycler>, Option<Arc<dyn Elevator>>, Arc<dyn FileFacts>, Arc<dyn ClaimCheck>)`, `Engine::prepare(&self, Vec<Candidate>, user_protected: &[String]) -> Prepared`, `Engine::commit(&self, token: &str, ids: &[u32], user_protected: &[String]) -> Result<CommitResult, String>`.

Rules for the implementer: a token is random, single-use, and remembers each item's `size`, `modified_ms` and `file_id`, each group's copies, and each folder's claim state at `prepare`. `commit` re-reads everything from disk. Admin items go through `Elevator`; with none, each one is left with a reason. Recycling is batched, and a failure to confirm stops the rest and sets `stopped_early`. `prepare` calls `stat` for every item, so `FileFacts::stat` for a folder walks it without following reparse points.

- [ ] **Step 1: Write the failing tests**, one per refusal, using temp folders and fakes:

```rust
#[test] fn commit_without_a_token_is_refused() {}
#[test] fn a_token_works_only_once() {}
#[test] fn an_item_not_in_the_token_is_refused() {}
#[test] fn the_last_copy_stays() {
    // select both copies: exactly one is moved, the other is Left with a reason
}
#[test] fn a_locked_copy_counts_as_the_surviving_copy() {}
#[test] fn a_copy_changed_since_review_stays() {}
#[test] fn a_copy_whose_contents_differ_from_the_survivor_stays() {}
#[test] fn a_copy_stays_if_the_keeper_was_deleted_after_review() {} // Review Focus 3
#[test] fn a_copy_stays_if_the_keeper_changed_after_review() {}     // Review Focus 3
#[test] fn a_hard_link_is_not_a_surviving_copy() {}                 // same volume and file id
#[test] fn a_protected_path_is_refused_even_if_added_after_prepare() {}
#[test] fn a_linked_pc_file_is_refused() {}
#[test] fn program_files_without_an_elevator_is_refused() {}
#[test] fn a_folder_claimed_after_review_stays() {}
#[test] fn a_folder_with_a_new_file_stays() {}
#[test] fn a_reparse_point_candidate_is_locked_and_its_target_untouched() {} // Review Focus 4
#[test] fn a_junction_inside_a_folder_is_not_counted_or_followed() {}        // Review Focus 4
#[test] fn the_rest_of_the_batch_stops_when_one_item_cant_be_confirmed() {}
```

- [ ] **Step 2:** Run `cargo test -p seek-clean engine`. Expected: compile failure.
- [ ] **Step 3:** Implement `Engine` and the real `FileFacts`. `same_bytes` compares in 64 KB blocks with early exit. `recycle_block` is a stub returning `None` until Task 4.
- [ ] **Step 4:** Run `cargo test -p seek-clean engine`. Expected: PASS.

### Task 4: The real recycler and its pre-checks

**Files:**
- Modify: `crates/seek-clean/src/recycle.rs`, `src/facts.rs`

**Interfaces:**
- Consumes: Task 1's finding, Task 3's traits.
- Produces: `recycle::ShellRecycler` implementing `Recycler`; `FileFacts::recycle_block` filled in.

- [ ] **Step 1: Write the failing tests** on pure helper functions, so no real bin is needed:

```rust
#[test] fn removable_network_and_unknown_drives_block() {}     // drive_allows(DriveType) -> bool
#[test] fn nuke_on_delete_blocks() {}                           // parses a BitBucket\Volume value set
#[test] fn a_batch_over_max_capacity_blocks() {}
#[test] fn a_too_long_path_blocks_with_its_own_reason() {}
#[test] fn cloud_placeholder_attributes_block() {}              // FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, _RECALL_ON_OPEN, _OFFLINE
```

- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement each check from the spec's *Before each batch* list, resolving links to the real volume first. A failed `SHQueryRecycleBinW` blocks the item.
- [ ] **Step 4:** Implement `ShellRecycler::recycle` on Task 1's code. It runs on its own COM thread. `PreDeleteItem` returns an abort for an item Windows reports won't be recycled. A sink error or missing location stops the batch.
- [ ] **Step 5:** Add `#[ignore] real_locked_file_stays_with_a_reason` and `real_read_only_file_outcome` (Review Focus 2): hold a file open with no sharing, recycle it, assert it still exists and the outcome text names it as in use.
- [ ] **Step 6:** Run `cargo test -p seek-clean` and `cargo test -p seek-clean --test recycle_real -- --ignored`. Expected: PASS.

### Task 5: Selectors and the beam

**Files:**
- Modify: `app/src/components/Segmented.tsx`, `SettingsPanel.tsx`, `LinkPcDialog.tsx`, `app/src/styles/space.css:12`, `app/src/styles/app.css:304-307,343`, `app/src/test/app.test.tsx`

**Interfaces:**
- Produces: `SegmentedItem.icon?: LucideIcon`, `className="is-block"` for full width.

- [ ] **Step 1: Write the failing tests** in `app.test.tsx`: Settings' theme selector is a `.seg` with three radios with icons; the Link dialog's tabs are `.seg` in the order Show a code, Enter a code, Hub; `document.querySelector(".segmented, .link-tabs")` is null with both open.
- [ ] **Step 2:** Run `pnpm test`. Expected: FAIL.
- [ ] **Step 3:** Add icons to `Segmented`, in both the button and the `seg-ink` copy. Add `.seg.is-block { display: grid; width: 100% }`. Move both dialogs to it, and delete `.segmented` and `.link-tabs`.
- [ ] **Step 4:** Change `.seg-face` to `background: light-dark(#1d1d1f, #aeaeb2); box-shadow: 0 2px 10px light-dark(#00000033, #00000059);`. Label ink stays `#1d1d1f`.
- [ ] **Step 5:** Fix the Link dialog height so it doesn't change between tabs: give the panel area one `min-height` measured from the tallest tab.
- [ ] **Step 6:** Run `pnpm test` and `pnpm build`. Expected: PASS.

### Task 6: The whole UI against the mock API

**This task ends in a design review.** The orchestrator checks the running mock in `pnpm dev`, light and dark, before any later task starts.

**Files:**
- Create: `app/src/api/mockClean.ts`, `app/src/hooks/useCleanup.ts`, `app/src/components/clean/*` (see file map), `app/src/styles/clean.css`, `app/src/test/clean.test.tsx`
- Modify: `app/src/api/types.ts`, `mock.ts`, `tauri.ts`, `app/src/components/ViewSwitch.tsx`, `app/src/App.tsx`, `app/src/components/SettingsPanel.tsx`

**Interfaces:**
- Consumes: the Shared interfaces block. TypeScript types mirror `model.rs`, and add:

```ts
export interface DupCopy { path: string; size: number; modified: number; lock: Lock | null; suggestedKeep: boolean }
export interface DupGroup { id: number; name: string; size: number; copies: DupCopy[]; freeable: number }
export interface OtherPcMatch { name: string; size: number; pcName: string; path: string }
export interface DupScanState { running: boolean; found: DupGroup[]; otherPcs: OtherPcMatch[]; checkedBytes: number; totalBytes: number; current: string | null }
export type CleanEvent = { type: "group"; group: DupGroup } | { type: "progress"; checkedBytes: number; totalBytes: number; current: string | null } | { type: "done" };
export interface LeftoverRow { path: string; parent: string; size: number; modified: number; needsAdmin: boolean; level: Level; reasons: Reason[]; contents: string[] }
export interface Reason { text: string; points: number }
export type Level = "veryLikely" | "likely" | "notSure" | "probablyInUse";
export interface AppRow { id: string; name: string; publisher: string; version: string; size: number; installDate: number | null; icon: string | null; folder: string | null; note: string | null; running: boolean }
export type PrepareRequest = { kind: "duplicates"; groups: string[][] } | { kind: "folders"; why: Why; folders: { path: string; level: Level | null; reasons: Reason[] }[] } | { kind: "stale"; files: string[] };
```

- [ ] **Step 1: Write the failing tests** in `clean.test.tsx`, with the mock API:

```ts
it("shows Decimator in the view switch with its tooltip"); // title "Cleans up files (Ctrl+Shift+D)"
it("Ctrl+Shift+D toggles it and the view is kept across switching");
it("starts with nothing ticked at any confidence level, in every tab");
it("Use suggestions ticks the others but never the suggested keep");
it("a group's last unlocked copy can't be ticked when the others are all ticked");
it("a locked copy has no tick and says why");
it("the review sheet's action button is not focused, and Enter on the list removes nothing");
it("the total and title follow unticking in the sheet");
it("every tab reaches removal only through the sheet"); // cleanCommit is called only after the sheet's button
it("the meter always writes the level out, and shows the reasons");
it("the Program Files row has a shield, and the button says it asks for admin");
it("the result replaces the list in the same sheet, with what stayed and why");
it("Recently removed is a button in the head, not a tab");
it("the uninstall sheet says it can't be undone before anything runs");
it("no row or text uses the word safe"); // scans rendered text in each tab
```

- [ ] **Step 2:** Run `pnpm test clean`. Expected: FAIL.
- [ ] **Step 3:** Add the types and `mockClean.ts`. The mock has about 40 duplicate groups arriving over time with `simulate`, including a Program Files copy, a USB copy locked with its reason and a linked-PC match. It has leftovers at all four levels and about 30 apps. Its `cleanPrepare` and `cleanCommit` enforce the last-copy rule and record history, so the flow can be tried end to end.
- [ ] **Step 4:** Add `"clean"` to `View` (`ViewSwitch.tsx`, `readView` in `App.tsx`), the `ctrl+shift+d` hotkey and the tooltip. Render `CleanView` in `<main>`, keeping its state mounted across view switches like Space. Clicking a drive narrows Duplicates to it.
- [ ] **Step 5:** Build the shared frame in `CleanView`: head with one number and one action, scan line, list, bottom bar, *Recently removed* button, `Segmented` tabs Duplicates | Leftovers | Apps.
- [ ] **Step 6:** Build `ReviewSheet` on `Dialog` at `width={720}`: virtualized rows grouped by folder, the list carrying `data-autofocus`, the result replacing the list. Use the same virtual-row approach as `ResultsTable`.
- [ ] **Step 7:** Build the three tabs, `ConfidenceMeter`, `UninstallSheet`, `RemovedSheet`, and *Protected from Cleanup* in Settings. Apply the spec's whole *Motion* table in `clean.css`, with reduced-motion handling.
- [ ] **Step 8:** Run `pnpm test` and `pnpm build`. Expected: PASS.
- [ ] **Step 9:** Orchestrator's design review in `pnpm dev`, light and dark, including `break-ui`-style worst cases: very long paths, one huge group, zero results, thousands of review rows. Fix before moving on.

### Task 7: Finding same-size files in the service

**Files:**
- Create: `crates/seek-core/src/same_size.rs`
- Modify: `crates/seek-core/src/lib.rs`, `crates/seek-ipc/src/protocol.rs:11,23-74,78-101`, `crates/seek-service/src/server.rs`, `crates/seek-service/tests/service.rs`

**Interfaces:**
- Produces:
  - `seek_core::sizes_with_files(ix: &DeviceIndex, hidden: Option<&[bool]>, min_size: u64) -> Vec<(u64, u32)>`: size and count, largest first, files only, alive, not hidden.
  - `seek_core::files_of_size(ix, hidden, sizes: &HashSet<u64>) -> HashMap<u64, Vec<u32>>`
  - `Request::SameSizeFiles { min_size: u64, device_ids: Vec<DeviceId>, cursor: Option<u64>, limit: u32 }`
  - `Response::SameSizeFiles { groups: Vec<SizeGroup>, next: Option<u64> }`, with `pub struct SizeGroup { pub size: u64, pub files: Vec<ResultRow> }`
  - `PROTOCOL_VERSION = 3`

The cursor is the largest size still to return (`None` starts at the top). A page takes whole size groups until `limit` files are reached, always at least one group. Only sizes with two or more files across the chosen drives are returned.

- [ ] **Step 1: Write the failing tests:**
  - `seek-core`: `same_size_skips_folders_deleted_and_hidden_rows`, `same_size_is_largest_first_and_honours_the_minimum`.
  - `seek-service/tests/service.rs`: `same_size_files_pages_through_every_group`, `same_size_files_survives_an_idle_unload_between_pages`, `same_size_files_hides_other_users_rows`, `same_size_counts_across_two_drives`.
- [ ] **Step 2:** Run `cargo test -p seek-core same_size` and `cargo test -p seek-service --test service same_size`. Expected: FAIL.
- [ ] **Step 3:** Implement, using `state.snapshot("same size")`, `state.hidden_for`, `state.hidden_mask` and `materialize`, as `space()` does at `server.rs:491`, in `spawn_blocking`. It stores nothing between pages.
- [ ] **Step 4:** Update the two `Hello` version tests if they pin the number, and any request-exhaustive `match`.
- [ ] **Step 5:** Run `cargo test -p seek-core -p seek-ipc -p seek-service`. Expected: PASS.

### Task 8: Narrowing and hashing

**Files:**
- Create: `crates/seek-clean/src/dupes.rs`

**Interfaces:**
- Consumes: Task 2 (`lock_for`), `FileFacts`.
- Produces:
  - `dupes::scan(input: Vec<SizeBatch>, facts: &dyn FileFacts, cancel: &AtomicBool, on_event: impl FnMut(ScanEvent) + Send)`
  - `dupes::suggest_keep(copies: &[PathBuf], downloads_like: &[String]) -> usize`
  - `dupes::freeable(size: u64, locks: &[Option<Lock>]) -> u64`
  - `SizeBatch { size: u64, paths: Vec<PathBuf> }`, `ScanEvent::{Group(DupGroup), Progress{checked_bytes,total_bytes,current}, Done}`, where Rust's `DupGroup` matches Task 6's TypeScript type.

- [ ] **Step 1: Write the failing tests** on temp folders:

```rust
#[test] fn same_size_different_contents_is_not_a_group() {}
#[test] fn matching_start_and_end_but_different_middle_is_not_a_group() {}
#[test] fn hard_links_are_not_duplicates() {}
#[test] fn under_the_minimum_is_skipped() {}
#[test] fn reparse_points_and_cloud_placeholders_are_never_read() {}
#[test] fn a_group_with_nothing_removable_is_not_shown() {}
#[test] fn freeable_counts_only_what_can_really_be_freed() {
    assert_eq!(freeable(100, &[None, None, None]), 200);                          // one copy stays
    assert_eq!(freeable(100, &[Some(Lock::ProgramFiles), None, None]), 200);      // the locked one stays
    assert_eq!(freeable(100, &[Some(Lock::ProgramFiles), Some(Lock::AppData), None]), 100);
    assert_eq!(freeable(100, &[Some(Lock::ProgramFiles), Some(Lock::AppData)]), 0);
}
#[test] fn suggestion_prefers_outside_downloads_then_shortest_then_oldest() {}
#[test] fn cancel_keeps_what_was_found() {}
#[test] fn largest_groups_come_first() {}
#[test] fn a_file_that_vanishes_mid_scan_is_dropped_without_failing_the_scan() {}
```

- [ ] **Step 2:** Run `cargo test -p seek-clean dupes`. Expected: FAIL.
- [ ] **Step 3:** Implement: first and last 64 KB compare, then full BLAKE3, with a fixed 1 MB buffer reused per thread and no result held beyond the group. One worker per physical drive, using `IOCTL_STORAGE_GET_DEVICE_NUMBER` and falling back to the volume. Each worker sets background I/O priority with `SetThreadPriority(THREAD_MODE_BACKGROUND_BEGIN)`.
- [ ] **Step 4:** Run `cargo test -p seek-clean dupes`. Expected: PASS.

### Task 9: Wiring the scan to the window

**Files:**
- Create: `app/src-tauri/src/cleanup.rs`
- Modify: `app/src-tauri/src/lib.rs`, `app/src-tauri/Cargo.toml`, `app/src-tauri/src/settings.rs`, `app/src/api/tauri.ts`

**Interfaces:**
- Consumes: Tasks 3, 7, 8.
- Produces: `CleanState` (managed Tauri state holding the `Engine`, scan thread and results), commands `clean_scan_start`, `clean_scan_cancel`, `clean_scan_state`, `clean_prepare`, `clean_commit`, `open_recycle_bin`; event channel `seek://clean`; `Settings.protected_paths`.

- [ ] **Step 1: Write the failing tests** in `cleanup.rs`: `split_rows_separates_local_from_linked_pcs` (linked-PC rows become `OtherPcMatch` only when name and size match, and are never candidates), `prepare_request_with_a_linked_device_becomes_a_refused_candidate`, `results_are_dropped_after_five_minutes_hidden_once_finished`, `results_stay_while_the_scan_runs` (hidden-time logic as a pure function), `a_service_error_mid_paging_ends_the_scan_with_what_was_found_and_says_so`. Update the settings JSON test with `"protectedPaths":[]`.
- [ ] **Step 2:** Run `cargo test -p seek-app`. Expected: FAIL.
- [ ] **Step 3:** Implement. Pages with `limit = 20000` and `min_size = MIN_DUPLICATE_BYTES`, all fetched first on one connection, so the scan then needs nothing from the service. A scan thread emits on `seek://clean`. The hidden-time check joins the look loop in `keep_service_awake`.
- [ ] **Step 4:** Register commands in `lib.rs`. Point `tauri.ts` at them. Check every command argument against `SeekApi` by name.
- [ ] **Step 5:** Run `cargo test -p seek-app`, `cargo clippy --workspace --all-targets -- -D warnings`, `pnpm test`. Expected: PASS.
- [ ] **Step 6:** Orchestrator's real-window check: scan real drives, hide the window, confirm with `seek memory` that the service's indexes unload on the usual timer while the scan continues. Do not remove anything yet.

### Task 10: Claiming a folder

**Files:**
- Create: `crates/seek-clean/src/claims.rs`

**Interfaces:**
- Produces: `claims::SystemClaims::read() -> SystemClaims` implementing `ClaimCheck`; `SystemClaims::from_parts(ClaimParts)` for tests; `claims::ALWAYS_CLAIMED: &[&str]`; `claims::whole_words_match(folder_name: &str, app_name: &str) -> bool`.

- [ ] **Step 1: Write the failing tests**, one per signal in the spec's list: `an_uninstall_location_claims`, `an_icon_path_claims`, `an_uninstall_command_claims`, `a_display_name_claims_as_whole_words`, `a_partial_word_does_not_claim` (`AppX Extended` doesn't claim `AppX`), `a_service_image_path_claims`, `a_run_key_claims`, `an_app_paths_entry_claims`, `a_start_menu_shortcut_target_claims`, `a_running_process_claims`, `the_always_claimed_table_claims` (including `Microsoft`, `Windows`, `Common Files`, `WindowsApps`, `Packages`, `Programs`, `Temp`, `Seek-dev`), `quoted_and_argument_paths_are_understood` (`"C:\Program Files\App\u.exe" /x`).
- [ ] **Step 2:** Run `cargo test -p seek-clean claims`. Expected: FAIL.
- [ ] **Step 3:** Implement. `read()` uses the registry, services (`EnumServicesStatusEx`, `QueryServiceConfig`), `IShellLink` for shortcuts in both Start Menus, and `K32EnumProcesses` with `QueryFullProcessImageNameW`. A process Seek can't read is skipped, and the result notes it. `is_claimed` uses `paths::is_inside` both ways.
- [ ] **Step 4:** Run `cargo test -p seek-clean claims`. Expected: PASS.

### Task 11: The confidence meter and the Leftovers tab

**Files:**
- Create: `crates/seek-clean/src/confidence.rs`, `src/leftovers.rs`
- Modify: `crates/seek-ipc/src/protocol.rs`, `crates/seek-service/src/server.rs`, `crates/seek-service/tests/service.rs`, `app/src-tauri/src/cleanup.rs`, `app/src/api/tauri.ts`

**Interfaces:**
- Produces:
  - `confidence::Evidence` (one enum variant per table row), `confidence::POINTS: &[(Evidence, i32, &str)]`, `confidence::level(total: i32) -> Level`, `confidence::score(ev: &[Evidence]) -> (i32, Level, Vec<Reason>)`
  - `leftovers::scan(places, claims: &dyn ClaimCheck, facts) -> Vec<LeftoverRow>`
  - `Request::StaleData`, `Response::StaleData { files: Vec<StaleFile> }`, `StaleFile { path, size, modified }`
  - command `clean_leftovers`

- [ ] **Step 1: Write the failing tests:**
  - `confidence.rs`: `each_evidence_moves_the_score_by_its_table_value` (table-driven over the spec's 15 rows), `level_boundaries` (5→veryLikely, 4 and 2→likely, 1 and 0→notSure, -1→probablyInUse), `names_of_four_letters_or_fewer_never_match`, `a_similar_named_app_is_penalized`, `every_reason_is_a_plain_sentence`.
  - `leftovers.rs`: `a_claimed_folder_is_not_listed`, `only_top_level_folders_are_candidates`, `a_junction_is_not_listed_and_not_followed`, `empty_folder_scores_high`, `a_folder_of_programs_scores_low`, `nothing_is_called_safe`.
  - `service.rs`: `stale_data_lists_only_files_no_known_drive_owns` (index in `indexes/` and journal in `journal/` with an unknown id are listed, a live one isn't, `devices.json` never is).
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement `StaleData` in the service: list `data_dir/indexes/*.seekidx` and `data_dir/journal/*.log` whose stem isn't a device id in the registry. Add it to the service as read-only, and to the protocol with no further version bump (Task 7's bump ships in the same release).
- [ ] **Step 4:** Implement `confidence` and `leftovers`, and the `clean_leftovers` command merging folder rows with stale files.
- [ ] **Step 5:** Run `cargo test -p seek-clean -p seek-service -p seek-app` and `pnpm test`. Expected: PASS.

### Task 12: The app list

**Files:**
- Create: `crates/seek-clean/src/apps.rs`
- Modify: `app/src-tauri/src/cleanup.rs`, `app/src/api/tauri.ts`

**Interfaces:**
- Produces: `apps::list() -> Vec<AppRow>`, `apps::from_entries(Vec<RawEntry>) -> Vec<AppRow>`, `apps::install_folder(&RawEntry, &Places) -> Option<PathBuf>`, `apps::parse_uninstall(cmd: &str) -> UninstallCommand { Exe { path, args }, Msi { guid } }`; command `clean_apps`.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn system_components_parents_and_no_command_are_hidden() {}
#[test] fn seek_itself_is_hidden() {}
#[test] fn runtimes_stay_listed_with_a_note() {}
#[test] fn install_location_wins() {}
#[test] fn folder_falls_back_to_the_icon_then_the_uninstaller() {}
#[test] fn derived_folder_rejected_under_windows_installer_temp_package_cache_and_pf_root() {}
#[test] fn no_folder_is_still_listed() {}
#[test] fn parse_quoted_path_with_args() {
    assert_eq!(parse_uninstall(r#""C:\Program Files\App\unins000.exe" /SILENT"#),
        UninstallCommand::Exe { path: r"C:\Program Files\App\unins000.exe".into(), args: "/SILENT".into() });
}
#[test] fn parse_unquoted_path_with_spaces() {}
#[test] fn parse_msiexec_x_and_i_forms_become_uninstall() {
    assert_eq!(parse_uninstall("MsiExec.exe /I{AAAA-BBBB}"), UninstallCommand::Msi { guid: "{AAAA-BBBB}".into() });
}
```

- [ ] **Step 2:** Run `cargo test -p seek-clean apps`. Expected: FAIL.
- [ ] **Step 3:** Implement from the three `Uninstall` locations. Size comes from `EstimatedSize` (KB). Icons use `ExtractIconEx`-to-PNG data URLs, lazily, with a generic fallback.
- [ ] **Step 4:** Run `cargo test -p seek-clean apps`, `cargo test -p seek-app`. Expected: PASS.

### Task 13: Uninstalling

**Files:**
- Modify: `crates/seek-clean/src/apps.rs`, `src/leftovers.rs`, `app/src-tauri/src/cleanup.rs`, `app/src/api/tauri.ts`, `app/src/components/clean/UninstallSheet.tsx`

**Interfaces:**
- Produces: `apps::run_uninstaller(&UninstallCommand) -> Result<UninstallHandle, String>`, `apps::is_finished(process_ended: bool, entry_present: bool) -> UninstallPhase { Running, Finished, Unresolved }`, `leftovers::for_app(app: &AppRow, places, claims, facts) -> Vec<LeftoverRow>`; commands `uninstall_start`, `uninstall_state`, `uninstall_leftovers`.

- [ ] **Step 1: Write the failing tests:** `finished_needs_the_process_ended_and_the_entry_gone`, `process_ended_with_entry_still_there_is_unresolved_after_the_wait`, `for_app_goes_at_most_two_levels_deep`, `leftovers_after_uninstall_include_the_install_folder_and_shortcuts_and_nothing_is_ticked`. UI: `the_sheet_offers_its_finished_and_cancel_when_unresolved`, `a_running_app_shows_the_warning_before_it_starts`.
- [ ] **Step 2:** Run. Expected: FAIL.
- [ ] **Step 3:** Implement. Run the app's normal uninstaller visibly, through `ShellExecuteExW` with its process handle, retrying with `runas` only on `ERROR_ELEVATION_REQUIRED`. MSI runs `msiexec.exe /x{GUID}`. Poll the registry entry every second, for up to 5 minutes after the process ends, before `Unresolved`.
- [ ] **Step 4:** Run `cargo test -p seek-clean -p seek-app` and `pnpm test`. Expected: PASS.
- [ ] **Step 5:** Orchestrator's real-window check with the user: they choose a small real app and uninstall it.

### Task 14: Admin removal

**Files:**
- Create: `crates/seek-clean/src/elevated.rs`
- Modify: `app/src-tauri/src/main.rs`, `app/src-tauri/src/cleanup.rs`

**Interfaces:**
- Consumes: Tasks 3, 4, 10, 11.
- Produces: `elevated::check_item(&JobItem, &Places, &dyn ClaimCheck, data_dir_devices: &HashSet<DeviceId>) -> Result<(), String>`, `elevated::run_main(job_file: &Path) -> i32`, `cleanup::UacElevator` implementing `Elevator`.

- [ ] **Step 1: Write the failing tests:** `a_path_outside_program_files_and_seek_data_is_refused`, `a_lookalike_path_is_refused`, `a_traversal_path_is_refused`, `a_claimed_folder_is_refused`, `a_protected_folder_is_refused`, `a_live_index_file_is_refused`, `devices_json_is_refused`, `a_stale_file_is_accepted`, `a_different_account_is_refused` (the job's SID differs from the elevated token's user), `a_malformed_job_file_refuses_everything`.
- [ ] **Step 2:** Run `cargo test -p seek-clean elevated`. Expected: FAIL.
- [ ] **Step 3:** Implement. `main.rs` checks for `--clean-elevated <job file>` as its first act, before `seek_app_lib::run()`, and exits with no window. The job file and result file live in a new per-run folder under `%LOCALAPPDATA%\Seek`, and are deleted afterwards. The elevated process trusts nothing in the job but paths, and re-reads every fact itself, including `devices.json` in `%ProgramData%\Seek`.
- [ ] **Step 4:** Implement `UacElevator`: `ShellExecuteExW` with `runas`, wait for exit, read per-item results. A declined prompt means each item stays, with "Windows asked for admin and it was declined".
- [ ] **Step 5:** Run `cargo test -p seek-clean elevated`, `cargo test -p seek-app`. Expected: PASS.

### Task 15: Protect, and Seek's own uninstaller

**Files:**
- Modify: `app/src-tauri/windows/hooks.nsh`, `app/src/components/clean/*`, `app/src/components/SettingsPanel.tsx`, `app/src/test/clean.test.tsx`

- [ ] **Step 1: Write the failing tests:** `always_protect_from_the_menu_locks_the_row_and_appears_in_settings`, `removing_it_in_settings_unlocks_the_row`.
- [ ] **Step 2:** Run `pnpm test clean`. Expected: FAIL.
- [ ] **Step 3:** Implement *Always protect* and *Reveal in Explorer* through `ContextMenu`, on Duplicates and Leftovers rows, persisting to `protectedPaths`. Both engine paths read the live list.
- [ ] **Step 4:** Add to `hooks.nsh` a `NSIS_HOOK_POSTUNINSTALL` macro: a Yes/No prompt defaulting to No, "Also delete Seek's index and settings? This is permanent." Yes removes `$COMMONAPPDATA\Seek` and `$APPDATA\Seek`. Skip it when the uninstall is silent or part of an upgrade (`${If} $UpdateMode <> 1`, confirmed by reading Tauri's generated template).
- [ ] **Step 5:** Run `pnpm test`, then `.\scripts\build-installer.ps1`. Expected: PASS, and the installer builds. Installing and uninstalling is on the user's real-window list.

### Task 16: Recently removed and Put back

**Files:**
- Create: `crates/seek-clean/src/history.rs`
- Modify: `crates/seek-clean/src/engine.rs`, `app/src-tauri/src/cleanup.rs`, `app/src/api/tauri.ts`, `app/src/components/clean/RemovedSheet.tsx`

**Interfaces:**
- Produces: `history::History::open(path) -> History`, `History::append(&[Entry]) -> Result<(), String>`, `History::list() -> Vec<Entry>` (newest first), `trait BinRestore { fn restore(&self, bin_path: &str, original: &Path) -> Result<(), String>; fn in_bin(&self, bin_path: &str) -> bool; }`, with the real one on Task 1's `restore`, `history::put_back(entry, bin: &dyn BinRestore) -> PutBackResult`; `Entry { id, time_ms, original_path, size, why, bin_path, left_reason: Option<String> }`; commands `clean_history`, `clean_put_back`.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn every_removal_is_recorded_with_its_reason_and_bin_location() {}
#[test] fn a_failed_item_keeps_its_reason() {}
#[test] fn a_corrupt_file_is_set_aside_and_a_new_one_started() {}       // Review Focus 5
#[test] fn a_removal_still_succeeds_if_history_cant_be_written() {}     // says so in the result
#[test] fn two_writers_lose_nothing() {}                                 // append under a lock file, then rename
#[test] fn history_is_capped_at_ten_thousand_entries_keeping_the_newest() {}
#[test] fn put_back_says_no_longer_in_the_recycle_bin_when_it_was_emptied() {}
#[test] fn put_back_refuses_when_something_new_is_at_the_original_path() {}
```

- [ ] **Step 2:** Run `cargo test -p seek-clean history`. Expected: FAIL.
- [ ] **Step 3:** Implement. The engine records after each commit. `%APPDATA%\Seek\cleanup-history.json` is written atomically (temp then rename). Restoring uses Task 1's shell-verb path, matching original path and deletion date. Uninstall steps are recorded as `Why::Uninstall`, and can't be put back.
- [ ] **Step 4:** Add `#[ignore] real_put_back_restores_to_its_folder`.
- [ ] **Step 5:** Run `cargo test -p seek-clean`, `cargo test -p seek-clean --test recycle_real -- --ignored`, `pnpm test`. Expected: PASS.

### Task 17: End-to-end on the real engine

**Files:**
- Create: `crates/seek-clean/tests/flows.rs`

- [ ] **Step 1:** Write `#[ignore]` flows using `ShellRecycler` on a temp folder of a fixed drive: duplicate group of three, remove two, one stays, history has two, put both back and all three match. Leftover folder with a nested file and an inner junction to a second temp folder: folder goes to the bin, the junction's target is untouched, put back restores it.
- [ ] **Step 2:** Run `cargo test -p seek-clean --test flows -- --ignored`. Expected: PASS.
- [ ] **Step 3:** Search the crate to confirm the engine's only route to deletion: `DeleteFile`, `RemoveDirectory`, `remove_file` and `remove_dir` appear only in tests, and in the job-folder cleanup of Task 14. Report any others.

### Task 18: Final review

- [ ] **Step 1:** Run the full check. Expected: all pass, clippy clean.
- [ ] **Step 2:** Run `.\scripts\perf.ps1` and compare idle RAM with the last run. Expected: no increase.
- [ ] **Step 3:** Orchestrator's review in the real window, light and dark, on each tab, the sheet and the motion table. Beam colour judged here.
- [ ] **Step 4:** Whole-branch review with a fresh reviewer on the strongest model, given the spec and this plan. Fix what it confirms.
- [ ] **Step 5:** Give the user the spec's real-window list: USB stick shows locked with its reason; a Program Files leftover shows one admin prompt; a small real app uninstalls with its leftovers listed; *Put back* restores a file to its folder; the beam looks right; Seek's own uninstaller prompt.
- [ ] **Step 6:** Replace the "Next task" section of `docs/NEXT_STEPS.md` with the new state, shortening rather than adding. Delete stray empty files.

---

## Order and what can run together

1 → 2 → 3 → 4 is one chain, and nothing later starts until the spike's answer is in. 5 is independent. 6 needs only the Shared interfaces and ends with the design review. After that: 7 → 8 → 9, then 10 → 11, 12 → 13, and 14, 15, 16, 17, 18. Tasks touching `cleanup.rs`, `tauri.ts` or `protocol.rs` run one at a time.
