# Decimator Slimming Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Decimator about a quarter smaller (about 14,100 → 10,800 non-test lines) without changing anything the user sees and without weakening any guard against data loss.

**Architecture:** Most of the extra size is not in the guards themselves. It comes from the same thing being represented several times and from parallel execution paths. Examples:
- the admin helper runs a second review of what was already reviewed;
- two different models of the trusted folders;
- a batch API that is only ever called with one item;
- three owners of scan completion;
- a mock that re-implements the engine.

So the plan merges representations and paths. The guards stay.

**Tech Stack:** Rust (`crates/seek-clean`, `app/src-tauri/src/cleanup.rs`), React/TS (`app/src/components/clean/`, `useCleanup.ts`, `mockClean.ts`).

**Spec:** `docs/superpowers/specs/2026-11-04-seek-cleanup-design.md`. Behaviour must stay as specified. This plan changes structure only.

**Sources:**
- The Codex review of 2026-11-07, which was read-only. Its line references are relative to `feat/v1` at 7620efc.
- The keep/drop discussion with the user.

## Global Constraints

- **No visible change.** That covers every tab, sheet, preview scenario, keyboard path, animation, Scan faster, remembered scan results, hidden-window recovery and the history error notes.
- **No guard against permanent deletion, last-copy loss, unreviewed removal or restore overwrite is removed.** "A few extra lines for an important guard is no problem" (user, 2026-11-07).
- **Tests:** A test is retired only when the mechanism it tests disappears. The safety contract it stood for keeps a test, rewritten if needed. Normal tests never touch the real Recycle Bin.
- **Sequencing:** Start only after the faster-dupes session has landed or been dropped. It edits `dupes.rs`, `cleanup.rs` and `DuplicatesTab.tsx`.
- **`AGENTS.md` rules apply:** no commits unless the user says so; `seek-core` free of Windows code; timestamps are epoch milliseconds.
- **Checks after every step:** `cargo test -p seek-clean` (plus `-p seek-app` when `cleanup.rs` changes) and `pnpm test` when the UI changes.
- **Checks at the end:** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cd app; pnpm test; pnpm build`.

## Guards: keep, simplify, drop

This list replaces my earlier list of drop candidates. Codex weighed each guard by how likely the event is and what happens without the guard. Almost everything I had marked as droppable turned out to protect against permanent deletion, so it stays.

**Keep as is.** These are small, and each one stops a real way to lose data:
- **The 16 `Lock` reasons:**
  - SystemFolder, ProgramFiles, OtherUser, AppData, ToolFolder;
  - NoRecycleBin, BinFull, PathTooLong, CloudPlaceholder;
  - Protected, LinkedPc, UnsupportedPath, ReparsePoint;
  - Claimed, SharedFolder, Unreadable.
- **Review and commit:**
  - single-use token and subset;
  - max 8 open reviews;
  - expected shape and purpose;
  - fresh stats and file identity, re-checked after comparing;
  - byte-for-byte survivor on a different physical file;
  - selected copies never count as survivors;
  - a survivor is never inside a selected folder or the bin;
  - serialized commits;
  - fresh policy, claims and protection;
  - admin items count toward bin capacity;
  - stop on an unconfirmed removal, telling "left in place" apart from "may be gone";
  - history recorded before UAC;
  - device and offline checks;
  - quit waits for removals in flight.
- **Before recycling:**
  - fixed local drive only;
  - real volume, mounted at the drive root;
  - NTFS or ReFS only;
  - the bin must answer;
  - `NukeOnDelete` off, where an unreadable value counts as on;
  - the `NoRecycleFiles` and `RecycleBinSize` policies, where an unreadable value blocks;
  - 90% capacity margin with overflow-safe sums;
  - path-length limits;
  - no cloud or link reads, and the opened handle is validated;
  - hard-link count of 1;
  - full hash plus size and time checked after reading.
- **While recycling:**
  - odd paths rejected before the shell sees them;
  - recycle flags with no UI;
  - PreDelete refuses when the recycle bit is missing;
  - every PostDelete must report a bin location.
- **Admin step:**
  - strict arguments and restricted DLL search;
  - trusted roots from Windows/HKLM;
  - an unreadable protect list refuses everything;
  - same account and same executable;
  - purpose and location allowlist on both the written and the resolved path;
  - stale-index checks, including the live-device check;
  - duplicate IDs refused;
  - private, random, first-instance pipe with a current-user DACL and no remote clients;
  - PID checked both ways;
  - identification-only QoS;
  - byte and item caps;
  - deadlines;
  - results limited to the items sent, with missing ones reported as uncertain;
  - parent folders held open during removal (88 lines).
- **History and put back:**
  - missing, damaged and newer history files told apart;
  - a damaged file is set aside and kept, with a notice;
  - write to a temp file, sync, then rename, with a short retry;
  - a 10,000-entry cap;
  - a write failure is shown as an error, never as "removal failed";
  - refuse put back for items never moved, already back, or admin items;
  - refuse when the original spot is taken (planned early and checked again at the end);
  - match on bin path plus original path plus time;
  - restore through the shell, then check the file is back;
  - 30 s deadlines, with cancel, a "restore in progress" guard and late-finish reconciliation.

**Keep, with less code:**

| Guard | Change |
|---|---|
| PathTooLong | Measure path lengths during the existing folder walk instead of a second walk. |
| UnsupportedPath and odd-path rejection | One shared plain-drive-path validator instead of three. |
| Claimed | Keep every ownership source. Replace the generic `Reader`/`Clock`/`Cached` machinery with one concrete snapshot holder. |
| Fixed-drive check | "Fixed local or refused" instead of a six-way `DriveType` enum. |
| Nested selections | Keep one outer operation, with less bookkeeping. |
| Listing provenance | Resolve requests against typed records the backend keeps, instead of rebuilding them from request paths. |
| Admin re-check after UAC | One shared executor compares against the original reviewed record, instead of a second synthetic review. |
| Pipe deadlines | One shared bounded read/write helper. |
| History lock | Optionally a process-wide mutex instead of a file lock, but only if every writer runs in the app process (the admin helper writes no history today). |

**Drop.** These are duplicates only; the protection stays elsewhere:

| What | Why it can go |
|---|---|
| Second `confirmed_location` call, and the low-level batch stop loop in `recycle.rs` | The engine always sends one item and already stops the batch. |
| Ownership check in the helper's `check_item` | The shared executor runs the same check right after. |
| Aborting the admin step only because environment variables differ from the trusted folders | Once the helper builds its places straight from trusted sources, the environment grants nothing. |
| Rediscovering places and comparing them with the trusted roots | One representation instead of two. |
| Up to 1,000 timestamp names tried when setting aside a damaged history file | Use one random name and a rename that never overwrites. |

**Add.** This is small and fixes a real gap Codex found. The admin job now carries the reviewed file identity (`file_id`). Today a file swapped during the UAC prompt, with the same size and time, becomes the helper's new baseline.

## Steps

Each step leaves the app working and the tests green. The order goes from smallest risk to largest. Line counts are Codex's estimates.

### Task 1: Recycling takes one item
- **Files:** `crates/seek-clean/src/recycle.rs`, `facts.rs` (the `Recycler` trait), `engine.rs:620-625`.
- **Change:** `Recycler` recycles one path and returns one result. Validate the bin location once. Keep the probe-only telemetry out of the production sink state.
- **Tests:**
  - Keep the engine stop and partial tests (`engine/tests.rs` 655, 676, 706, 968).
  - Retire `a_batch_stops_at_the_first_failure_and_marks_the_rest` in `recycle.rs`, because that batch API is gone.
- **Saves:** about 120 lines.

### Task 2: Share the small Win32 and path helpers
- **Files:** `paths.rs`, `protect.rs:499`, `recycle.rs:244`, `elevated/guard.rs:74-87`, `dupes.rs:264`, `engine/support.rs:57`, `elevated/account.rs`.
- **Change:** One function each for:
  - plain drive-path validation;
  - "is this in the system store";
  - final path from a handle;
  - owned handles;
  - COM init.

  Each caller keeps its own policy.
- **Tests:** All odd-path, junction, real-parent and same-account tests stay. None are retired.
- **Saves:** about 100 lines.

### Task 3: One folder walk
- **Files:** `facts.rs:535-583`, `leftovers.rs`, `leftovers/for_app.rs`.
- **Change:** A single walk collects size, count, newest time, cloud and unreadable flags, and the longest inner path. Leftovers use the same walk, with their own link rules passed in. Remove the unused `_facts` parameter.
- **Tests:** Keep `facts.rs` tests 714, 755, 870 and 902, and the leftovers cloud, link and unreadable cases. They may call the new walker.
- **Saves:** about 150 lines.

### Task 4: One shortcut reader
- **Files:** `claims.rs:1232, 1295, 1313`, `leftovers/for_app.rs:178`.
- **Change:** One bounded enumerator that does not follow links, plus one COM target reader. Callers keep their own error handling.
- **Tests:** Keep the claim-source-failure tests, "working shortcut stays" and "unreadable shortcut is not offered".
- **Saves:** about 150 lines.

### Task 5: Simpler claims cache, app dates and app launching
- **Files:**
  - `claims.rs:769-860` and `365-423`;
  - `protect.rs:59-92`;
  - `apps.rs:377-452` and `534-579`;
  - `apps/run.rs:306-418`;
  - `app/src-tauri/src/cleanup.rs:862-941`.
- **Change:**
  - A concrete installed/running snapshot holder replaces the generic cache.
  - Trusted folders are looked up once through `Places`.
  - `YYYYMMDD` dates are converted through `SYSTEMTIME`.
  - One loader for app entries.
  - One launch helper with explicit visible/admin options.
  - The launch command is validated once, when it is built.
- **Tests:**
  - Keep the folder-derivation, registered-command-only, MSI GUID, local-exe and shell-host refusal tests, and the date output tests (688, 696, 712).
  - Retire the civil-date round-trip test (`apps/tests.rs:704`).
- **Saves:** about 450 lines.

### Task 6: The backend keeps typed records
- **Files:**
  - `app/src-tauri/src/cleanup.rs:331-379`, `481-580` and `1278-1304`;
  - `app/src/lib/clean.ts`.
- **Change:**
  - Listings store the actual candidate kind, path, confidence and reasons.
  - Duplicate prepare sends scan, group and copy IDs. The backend resolves them against the results it kept, then refreshes the device check.
- **Tests:**
  - Keep `cleanup/tests.rs` 992, 1013 and 1029 (app provenance) and 767 and 785 (offline or linked drive).
  - Requests change shape; the assertions stay.
- **Saves:** about 200 lines.

### Task 7: One executor for normal and admin removals
- **Files:**
  - `engine.rs:28-65`, `214-327` and `127-139`;
  - `elevated.rs:82-258`;
  - `model.rs:169-177` (`JobItem`).
- **Change:**
  - Merge `Seed`, `Rec` and `ReviewItem` into one reviewed-item record.
  - One function runs "check this reviewed item and recycle it". Both the normal commit and the helper call it.
  - The helper gets the approved records, with `file_id` added, over the existing pipe. It holds parents, compares against the original record, runs the shared checks and recycles.
  - Remove the helper's second prepare, its token and the ID remapping.
  - Remove the duplicate ownership check in `check_item`.
  - Remove the `UacElevator` wrapper (`cleanup.rs:847-852`).
- **Tests:**
  - Keep the elevated scope, account, changed-item, protected, duplicate-ID, stale-device and stop tests.
  - Rewrite `elevated/tests.rs:519` as "the current file differs from the approved record".
  - Add one test: the same size and time but a different `file_id` is refused.
- **Saves:** about 350 lines.

### Task 8: The helper builds trusted places directly
- **Files:** `elevated/trust.rs`, `elevated.rs:331-334`, `protect.rs`.
- **Change:**
  - `Places` is built straight from the trusted roots.
  - Trusted expansion stays for claim strings.
  - Remove the environment rewrite, the rediscovery, the comparison, and the abort when environment variables disagree.
- **Tests:**
  - Retire `an_environment_that_disagrees_is_detected_and_one_that_agrees_is_not`.
  - Replace `places_that_disagree_with_the_trusted_folders_are_refused` with direct-construction tests.
  - The poisoned-environment tests now check that the scope stays unchanged.
- **Saves:** about 80 lines.

### Task 9: Leaner history
- **Files:** `history.rs:240-299` and `403-459`.
- **Change:**
  - A damaged file is set aside under one random name, with a rename that never overwrites.
  - Single and batch restore share one planner.
  - Successful put-backs are written in one history write, not one per item.
  - Optional: a process-wide mutex replaces the file lock (see the decision below).
- **Tests:**
  - Keep the corruption, newer-version, cap, write-error and `two_writers_lose_nothing` tests.
  - Rewrite `an_existing_set_aside_name_is_kept` for the new naming.
  - If the lock changes, replace the file-lock "busy" test (321) with a mutex test.
- **Saves:** about 100 lines.

### Task 10: Scan completion has one owner
- **Files:** `dupes.rs:120-137` and `711-715`, `app/src-tauri/src/cleanup.rs:725-817` and `1162-1219`.
- **Change:**
  - The app supervisor sends the final state once, including panic reports and partial results.
  - The scanner only reports progress, groups and ordinary errors.
  - `read_ends` and `read_all` share one checked opener.
  - The backend alone prunes groups and recalculates suggestions.
- **Tests:**
  - Keep `cleanup/tests.rs` 613, 640, 653 and 694 (cancel, incomplete notice, restart).
  - The scanner's internal panic and Done tests (`dupes.rs:1163-1193`) change or go.
- **Saves:** about 250 lines.

### Task 11: Leaner UI controller and mock
- **Files:**
  - `useCleanup.ts:170-258`;
  - `ReviewSheet.tsx:72-92` and `211-259`;
  - `RemovedSheet.tsx:21`;
  - `LeftoversTab.tsx:34-39`;
  - `mockClean.ts:606-714`.
- **Change:**
  - The controller adopts backend snapshots and keeps only selection and animation.
  - Commit returns the history IDs, so the immediate Put back no longer searches history.
  - Sort history and leftovers once, in the backend.
  - The mock gets explicit scripted outcomes instead of a second engine, keeping a small token/subset model.
  - Fix the mock's divergence where `probablyInUse` blocks removal; Rust does not do that.
- **Tests:** Keep `clean.test.tsx`, `clean.fix1`, `clean.wiring`, `clean.uninstall`, `clean.history` and `clean.faster`. No last-copy, focus, review-only, protect-save-failure or timeout-retry test is removed.
- **Saves:** about 650 lines.

### Task 12: Final check
- Run the full checks from Global Constraints.
- Run `.\scripts\perf.ps1` and compare with the last run.
- Re-measure the line counts.
- Update `docs/NEXT_STEPS.md` by replacing old text.
- Redo the real-window checklist items that touch removal, admin removal and Put back.

## Expected size

| Area | Now | Target |
|---|---:|---:|
| `seek-clean` | ~9,200 | 6,600–7,300 |
| `cleanup.rs` | 1,398 | 900–1,050 |
| UI components, controller and mock | 3,497 | 2,700–3,000 |
| **Total** | **~14,100** | **~10,200–11,350** (midpoint 10,800, −23%) |

My earlier estimate of about 6,000 lines assumed dropping guards that, checked one by one, protect against permanent deletion. Keeping them puts the realistic floor near 10,800 lines. Going lower means cutting features or those guards.

## Decisions for the user

1. **History lock.** Should the cross-process file lock become an in-process mutex, saving about 30 lines? Default: **yes**. Only the app process writes history.
2. **App icons.** `apps.rs` extracts icons, but the Apps tab shows letter tiles, and the spec asks for icons. Default: **wire the icons into the Apps rows** in Task 11. The alternative is to drop the extraction and amend the spec.
3. **Execution.** Default: **subagent-driven, on Sonnet**, one task at a time with a review after each, because Tasks 6, 7 and 10 cross the engine/app boundary.
