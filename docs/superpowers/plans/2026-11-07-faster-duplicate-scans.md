# Faster Duplicate Scans Plan

> **For agentic workers:** use superpowers:subagent-driven-development. Read `AGENTS.md` → *Keep it simple* first.

**Goal:** scans finish fast: same-name quick scan by default, *Scan faster* during a scan, and files already read aren't read again.

**Spec:** `docs/superpowers/specs/2026-11-04-seek-cleanup-design.md` → *Duplicates → Faster scans*.

**Constraints:** safety rule 4 unchanged (no change to `engine.rs`/`recycle.rs`); contents are always compared; tooltip exactly `Uses more of your PC's disk and processor while it scans.`; Duplicates head keeps one button; `PROTOCOL_VERSION` 3 → 4; nothing kept in memory after a scan.

### Task 1: UI on mock data (show the user before Task 4)
`DupScanState` gains `mode: "quick" | "renamed"` and `fast: boolean`; API: `cleanScanStart(ids, renamed)`, `cleanScanFaster(on)`, `rememberedInfo(): {bytes}`, `clearRemembered()`.
- *Include renamed copies*: quiet `link-btn` in the idle placeholder, the results toolbar and the "none found" placeholder. *Rescan* repeats the last mode.
- *Scan faster*: `link-btn` toggle (`aria-pressed`) in the scan line before *Cancel*; off for each new scan.
- Settings → Decimator: "Remembered scan results: 14.2 MB" + *Clear* (disabled at 0).
- Tests: Scan calls `(ids, false)`, the link `(ids, true)`; the head has one button; tooltip text; Clear calls the API.

### Task 2: Same-name request
- `SameSizeFiles` gains `same_name: bool`. With it, the service counts `(size, name_lower)` across the chosen drives and returns only files whose pair is held by two or more. Reuse the existing size paging. Bump `PROTOCOL_VERSION` to 4.
- Tests: `Report.PDF` + `report.pdf` (2 MB, two drives) group; `other.pdf` at 2 MB doesn't; 3 MB `report.pdf` doesn't.

### Task 3: Scan faster and remembered results in `dupes.rs`
- `scan(..., fast: &AtomicBool, cancel, ...)`. Readers check `fast` before each file: on = normal priority, and on an SSD (`IOCTL_STORAGE_QUERY_PROPERTY` seek penalty = false) up to 4 readers; a hard drive or unknown keeps 1. All readers end when the scan ends.
- New `crates/seek-clean/src/remember.rs`: a file in `%LOCALAPPDATA%\Seek\dupe-prints.txt`, one line per file: path, size, modified, head/tail hash, full hash. Loaded at scan start, saved (merged) at scan end or cancel, then dropped. A file whose size and modified time match its line isn't read; its hashes come from the line. `clear()` deletes the file; `bytes()` returns its size.
- Tests: a remembered file isn't opened and still groups; a changed modified time means it's read again; wrong hashes on a stale line aren't believed; toggling `fast` mid-scan ends with one `Done`; SSD → 4 readers, HDD/unknown → 1.

### Task 4: Wire the window, docs
- `cleanup.rs`: `clean_scan_start(device_ids, renamed)` sends `same_name: !renamed`; `clean_scan_faster(on)`; `clean_remembered_info`, `clean_remembered_clear`. `fast` resets per scan.
- Update the spec's *Faster scans* part 3 and `docs/NEXT_STEPS.md` (replace text). Real-window checks for the user: quick scan finds same-name copies in seconds; *Include renamed copies* finds a renamed one; *Scan faster* raises disk use; a second scan is much faster; editing a file makes it read again; *Clear* empties it.

Done: `cargo test --workspace`, clippy `-D warnings`, `pnpm test`, `pnpm build` green. Commit each task on `feat/v1`; never push.
