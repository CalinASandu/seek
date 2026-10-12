# Seek — Status & Next Steps

_Last updated: 2026-10-12 (sync between PCs implemented, plan tasks 1–16). Read this first when starting a new session in this repo._

## Where we are

| Wave | Plan tasks | What | State |
|---|---|---|---|
| 0 | 1 | Workspace scaffold, shared model + IPC types | ✅ committed on `main` (`7a935fb`) |
| 1 | 2–4 | `seek-core`: index, persistence, registry, query parser, search, bench | ✅ committed on `feat/v1` |
| 1 | 5–6 | `seek-scan`: volumes, MFT parser/scanner, walker, USN follower, watcher | ✅ committed on `feat/v1` |
| 1 | 9 | `app/` UI (React + Vite) against a **mock** API | ✅ committed on `feat/v1` |
| 2 | 7–8 | `seek-ipc` framing/client/server, `seek-service`, `seek-cli` | ✅ committed on `feat/v1` |
| 3 | 10 | Tauri backend: real window ↔ service, tray, autostart, shares, launcher `Seek.bat` | ✅ committed on `feat/v1` |
| 4 | 11 | Review + fixes ✅, perf suite ✅, walker memory ✅, NSIS installer ✅, Google Drive ✅, access rule ✅, changed-folder rescans ✅, README ✅, elevated MFT/USN verified ✅ · window live-update + installer check ⏳ (user) | 🔶 almost done |
| 5 | sync 1–16 | **Sync between PCs** (`docs/superpowers/plans/2026-10-11-seek-sync.md`): seek-sync crate, service runtime, IPC, UI, perf, firewall · two-PC smoke test ⏳ (user) | ✅ uncommitted |

- Branch: `feat/v1`, pushed to `origin` (`github.com/CalinASandu/seek`). Last commit `0e49231` (2026-10-11).
  Wave 4 part 2 and the whole sync feature (below) are **uncommitted** — commit only when the user says so.
- UI: the user considers it finished for now ("very clean"); no visual pass planned.
- Spec: `docs/superpowers/specs/2026-10-04-seek-local-design.md`
- Plan (task-by-task): `docs/superpowers/plans/2026-10-04-seek-v1.md`
- Execution ledger + agent reports: `.superpowers/sdd/2026-10-04-seek-v1/` (git-ignored; `progress.md`, `agent-*-report.md`)

### Verified on the real machine (2026-10-06, non-elevated, walker fallback)

| Measure | Result | Target |
|---|---|---|
| Index C: (2,082,182 entries) | 60 s (walker) · **3.9 s via NTFS MFT** (2,095,271, 543 k/s, 309 MB index, 517 MB peak — elevated, 2026-10-11) | < 10 s / 1M ✅ |
| Index D: (717,994) / G: (25,832) | 16.7 s / 11.9 s | — |
| Service RAM, 2.8M entries | ~372 MB | — |
| Typical query over 2.8M | 1.7–7.5 ms | < 50 ms ✅ |
| Empty query (all 2.8M, sorted) | 117 ms | — |
| Reload C: index from disk | 327 ms (45 MB file) | < 1 s ✅ |
| Synthetic bench, 10M entries, `report` | 17.8 ms | < 50 ms ✅ |

Tests (2026-10-12): `cargo test --workspace` → 123 passed, 2 ignored (admin-only); `cargo clippy --workspace --all-targets -- -D warnings` clean; `pnpm -C app test` → 18 passed; `pnpm -C app build` passes. (2026-10-11: 75 passed / 13 app tests.)

## Working rules for this repo (from the user)

1. **Never `git commit`** (and never let subagents commit) unless the user explicitly says so. Leave changes in the working tree for review.
2. **Ask all clarifying questions at once**, numbered, each with a suggested default.
3. Windows-first; keep `seek-core` free of OS-specific dependencies.
4. The UI must be professional and polished (Windows 11 native feel, keyboard-first, dense but calm).
5. After agents run, check the repo root for stray empty files created by bad shell redirects (seen: `Expr`, `return`, `offset`, `PathBuf`, `false`, …) and delete them.

## How to run Seek today

**Double-click `Seek.bat`** in the repo root. It:
1. builds `seek-service`, `seek` and `seek-app` if they are missing (first run only, a few minutes);
2. starts the indexing service if it isn't running — **one admin prompt** (Yes = fast NTFS/MFT indexing + live
   USN updates; No = falls back to a non-admin service with the slower walker and no live updates). If the
   Windows service `SeekService` is installed, it starts that instead;
3. opens the Seek window (or brings it to the front). Closing the window keeps Seek in the tray; quit from the tray.

**`Stop Seek.bat`** closes the window and stops a console-mode service.

After changing code, rebuild what changed:
```powershell
cargo build --release -p seek-service -p seek-cli          # service + CLI
cd app; pnpm tauri build --no-bundle                         # window (frontend + seek-app.exe)
```

Manual / dev alternatives:
```powershell
.\target\release\seek-service.exe --console --data-dir "$env:LOCALAPPDATA\Seek-dev"   # service in foreground
.\target\release\seek.exe remote "ext:pdf" --sort size --desc                          # CLI search via the service
cd app; $env:SEEK_SERVICE_PATH="..\target\release\seek-service.exe"; pnpm tauri dev  # window with hot reload
cd app; pnpm dev   # browser preview with MOCK data → http://localhost:1420
```

Admin-only checks (run **from an elevated terminal**, `Stop-Service SeekService` first; last passed 2026-10-11):
```powershell
cargo test -p seek-scan -- --ignored --nocapture   # 2 passed
.\target\release\seek.exe perf --scan C:\          # C: 2.1M entries via NTFS MFT in 3.9 s
```
The installed `SeekService` runs `target\release\seek-service.exe` and locks it: stop the service (elevated)
before rebuilding the service or CLI, then `Start-Service SeekService`.

## Done in wave 3 (Task 10)

- `app/src-tauri/src/`: `pipe.rs` (ServiceLink: request connection + event subscription → `seek://event`,
  `serviceState` connected/connecting/notInstalled), `commands.rs` (all 14 commands of `SeekApi`),
  `service_ctl.rs` (SCM check, elevated `seek-service install`), `shell.rs` (open / reveal in Explorer / runas),
  `shares.rs` (network shares scanned in the user session, pushed with `PushShareIndex`, `DirWatcher` + 10-min
  refresh), `settings.rs` (`%APPDATA%\Seek\settings.json`, autostart plugin), `lib.rs` (tray, hide-on-close,
  `--minimized` for autostart).
- Window: frameless + transparent + Mica (`tauri.conf.json`), 1200×760 (min 900×560). App icon:
  `app/src-tauri/icons/seek.svg` → generated set via `pnpm tauri icon`.
- UI timestamps fixed to unix **milliseconds** (`app/src/lib/format.ts`, `app/src/api/mock.ts`, tests).
- Verified: real window shows the real drives; `witcher ext:pdf` → 39 results in 2.0 ms
  (screenshots `app/docs/screenshots/real-01-start.png`, `real-02-search.png`).

## Done in wave 4, part 1 (2026-10-06)

**Code review** (`/code-review high` over everything uncommitted) → 10 findings, 9 fixed with tests:
1. USN de-dup could emit a new folder's files before the folder → files silently dropped. `DeviceIndex::apply`
   now defers upserts whose parent is unknown and retries them until the batch stops making progress.
2. Stored searches kept raw row numbers; after compaction/rescan they pointed at other files. Every
   `DeviceIndex` has an `epoch()` (changes when rows are renumbered); `fetch_rows` refuses stale hits.
3. Non-elevated NTFS (walker fallback) got no live updates at all → now watched, like non-NTFS volumes.
4. `fetch_rows` dropped rows → pages shifted on the client. Now all-or-nothing ("results changed; search again").
5. Changes during a watcher-triggered rescan were lost → `rescan_pending` + cooldown (10× last scan time,
   10 s – 15 min), so a burst of changes = one rescan, and nothing is lost.
7. A USN follower survived a rescan that fell back to the walker and corrupted the new index → removed.
8. The app serialized every request on one mutex (no search cancellation, uploads blocked searches) →
   `seek_ipc::MuxClient` multiplexes by envelope id; share uploads use their own connection.
9. Search ids restarted at 1 per connection → UI showed cached pages of an old result set. Ids are now
   unique across connections and restarts.
10. Every delete swept all rows repeatedly → sweeps only when a folder dies, in one memoised O(n) pass.
- Finding 6 (any logged-in user could search every file name): **decided and fixed in part 2**, see below.

**Sorting by size / modified / device** used a comparator that re-read index columns for every comparison and,
on ties (all folders have size 0), built two full path strings per comparison. Now keys are extracted once
into a flat array and only runs of equal keys are tie-broken. 2M entries, empty query: size 279 → 33 ms,
device 1001 → 63 ms; "report" by size 24.8 → 4.4 ms.

**Walker memory**: the walker now builds the `DeviceIndex` directly while walking (jwalk is strictly
depth-first, so only the current chain of ancestor folders is kept) instead of a `Vec<RawEntry>` with one heap
`String` per file plus a `HashMap<PathBuf>` of every folder. D: (735k entries): peak 188 → 113 MB, 17.6 → 14.3 s.
`ScanResult` now carries `index: DeviceIndex`. (Periodic saving / resume was considered: it would not lower
memory, since the finished index must be in RAM to be searched; the bulk was the temporary copy.)

**Perf suite**: `seek perf` (or `.\scripts\perf.ps1`) measures synthetic build/memory, every query × sort on two
devices, a results page, save/load, a live-change batch, real scans (`--scan D:\`, each in a child process so
peak memory is its own) and service round trips (`--remote`). `--json` saves, `--compare` prints deltas.
`perf.ps1` saves to `perf-results\` (git-ignored) and compares with the previous run automatically.

**Installer**: `.\scripts\build-installer.ps1` → `target\release\bundle\nsis\Seek_0.1.0_x64-setup.exe` (3.6 MB).
Per-machine (Program Files — the SYSTEM service must not live in a user-writable folder). Hooks
(`app/src-tauri/windows/hooks.nsh`): stop the service before files are replaced, `seek-service install`
after (now idempotent: updates an existing registration), `seek-service uninstall` on uninstall; the index in
`%ProgramData%\Seek` is kept. `externalBin` lives in `tauri.installer.conf.json` so plain cargo builds don't need
the copied binary. Bundle id is now `com.calinsandrei.seek`. MSI not built (needs a WiX custom action for the service).

## Done in wave 4, part 2 (2026-10-08 … 2026-10-11)

**Google Drive / session-only volumes** (committed in `0e49231`): drives only the user's session sees (Google
Drive's `G:`) are reported by the app with `AddUserVolume`, walked by the app and pushed like network shares.
**No more endless re-searching** (same commit): the UI searches only when the query changes; index updates mark
the results stale and the status bar offers a manual refresh.

**Access rule (finding 6), decided by the user 2026-10-11** — `crates/seek-service/src/access.rs`:
- A client running **elevated** ("Run as administrator", `TokenIsElevated`) sees every file. Membership in the
  Administrators group alone does **not** count (user's choice) — the normal, non-elevated window is a standard client.
- Every other client sees everything **except other users' profile folders** (from the registry ProfileList, e.g.
  `C:\Users\<other>`, plus the system profiles). The client is identified per connection:
  `GetNamedPipeClientProcessId` → process token → elevation + user SID; unidentifiable clients see the least.
- Filtering happens after the search with a per-(device, user) row mask (`DeviceIndex::subtree_mask`), cached until
  the index epoch/row count changes; network devices are never filtered. Tests:
  `other_users_folders_are_hidden_unless_elevated`, `access::tests`.
- Known gap: app-fed devices (network shares, Google Drive) pushed by one user are visible to every user.

**Changed-folder rescans (backlog 3)** — watched volumes (non-NTFS, and NTFS without admin) no longer rescan the
whole volume on every change:
- `DirWatcher` now reports **which folders changed** (`Changed::Dirs` = the parent folder of every event path,
  per 2-s burst) or `Changed::Everything` (lost events / buffer overflow, or > 2000 folders in one burst).
- The service queues them per device (`state::Pending`, merged across bursts) and `monitor::start_update` runs
  `seek_scan::rescan_dirs` on a background thread **without any cooldown**: each folder is listed once
  (non-recursively), diffed by name (case-insensitive) against its children in the index → deletes, metadata
  updates, new files, and new folders walked completely (`scan_walk` of the subtree, ids above `max_id()`).
  Folders not in the index yet climb to their nearest indexed ancestor. The changes go through
  `DeviceIndex::apply` (id-based, so compaction in between is harmless). On error → falls back to a full rescan.
- Full rescans keep the old cooldown (10× last scan time, 10 s–15 min) and are now only for `Everything`.
- Cost per burst ≈ 3 linear passes over the index (find folders, children, max id) — ~tens of ms on C:.
- Tests: `rescanning_changed_folders_matches_a_full_walk` (real filesystem: add/modify/delete/rename/file↔folder
  /new nested folders → identical to a fresh walk), `changed_folders_are_rescanned_without_a_full_scan`,
  `watch::tests`. Not yet watched end-to-end on a real drive with the release build.
- The app's share scanner still re-walks a whole share on change (it pushes whole indexes).

**README** rewritten: animated hero (`docs/assets/hero-{dark,light}.svg`, generated by
`python scripts/make-readme-hero.py`), features, screenshots, query syntax, performance, install, mermaid
architecture, roadmap.

## Done in wave 5: sync between PCs (2026-10-11 … 2026-10-12)

Spec `docs/superpowers/specs/2026-10-11-seek-sync-design.md`, plan + per-task notes
`docs/superpowers/plans/2026-10-11-seek-sync.md` (progress ledger at the top).

- **`crates/seek-sync`** (no Windows deps): per-device change journal (`journal.rs`, ~50 MB cap, generations),
  frames, Noise IK transport (`noise.rs`), SPAKE2 pairing with 6-digit codes (2 min, 3 attempts), `peers.json` +
  `host.key`, and the transport-agnostic sync session (`session.rs`, `SyncHost` trait): device list, cursors,
  full copies (zstd level 1) and deltas, acks, Sync now, unlink, group join.
- **Service**: own devices journal every change (`journal_ops.rs`); `sync_host.rs` serves own devices and keeps
  copies of linked PCs' devices as ordinary remote devices (cursor saved in the index header, so a restart
  resumes with deltas); `peers.rs` = TCP listener/dialer on **47321** (lower host id dials), mDNS
  `_seek._tcp.local.`, pairing host/guest, group join (a PC joining through B is linked with A too), unlink (also
  delivered later to a PC that was off), reinstalled PC → **Needs re-linking**, never re-trusted silently.
- **Search**: `pc:laptop`; `device:` also matches the PC name; the same USB stick known to two PCs shows once
  (connected local record wins, else the newest index). Access: another PC's profile folders are hidden unless
  that user is linked with you (the users on both sides of a pairing count as the same person); elevated sees all.
- **IPC/app**: 9 new requests (pairing, peers, unlink, rename, Sync now, sharing). UI groups the sidebar by PC (only
  when a PC is linked), Link a PC dialog (show / enter a code, PCs found on the network), Settings → Linked PCs,
  remote rows say `PC › device` and can't be opened here, status bar `N PCs · synced …`.
- **Install**: `seek-service install` adds the firewall rule `Seek sync` (TCP 47321, Private profile only);
  `uninstall` removes it. Console mode syncs by default; `--no-sync` turns it off.
- **Perf** (`seek perf`, `== sync ==`): 2M-entry full copy over encrypted loopback 350 ms (target < 2 s; 69 MB, so
  ~0.6 s more on gigabit); applying 1,000 changes 0.4 ms (target < 50 ms).
- Limitations: LAN only (no relay/server), remote files can't be opened from here, user links exist only between
  the two PCs where a code was typed.

### Try it on two PCs
1. Install (or run) Seek on both PCs, on the same network (Windows network profile **Private**).
2. PC A: Settings → Linked PCs → **Link a PC** → *Show a code*.
3. PC B: Settings → Linked PCs → **Link a PC** → *Enter a code* → pick PC A (or type `ip:47321`) → type the code.
4. Within seconds both sidebars show the other PC's devices; search finds their files (`pc:<name>` to narrow).

- **Naming (2026-10-12, user's choice):** in the UI a *drive* is a disk/stick/share on one PC and a *device* is a PC.
  `drive:` matches drive names only, `device:` matches a device or a drive name, `pc:` still works. The sidebar says
  *Drives* until a device is linked, then *Devices* grouped per device; **Link a device** sits in the sidebar footer.

## Next step

1. Verification status (2026-10-11):
   - ✅ **Elevated**: both admin-only tests pass; C: 2.1M entries via MFT in 3.9 s; the installed service (rebuilt
     with wave 4 part 2) resumes the USN journal on C: and D:.
   - ✅ **Non-elevated changed-folder rescans** on D: (748k entries, release build, test service on its own pipe):
     new folder with 27 files → 29 changes in 16 ms; rename + delete → 7 changes in 13 ms; no full rescan.
   - ⏳ **Window live update** (user): create / rename / delete `D:\zqxtest.txt` → stale banner, refresh shows it.
   - ⏳ **Installer** (user, optional): setup exe → `sc qc SeekService` points to Program Files, window finds files;
     uninstall → service gone; then restore the dev service with elevated `.\target\release\seek-service.exe install`.
2. **Sync checklist (user)** — automated tests cover two to six services over loopback; still to see on real hardware:
   - ⏳ Rebuild + reinstall the service elevated (`Stop-Service SeekService`, `cargo build --release -p seek-service
     -p seek-cli`, `.\target\release\seek-service.exe install`), then
     `netsh advfirewall firewall show rule name="Seek sync"` → one rule, Private, TCP 47321.
   - ⏳ Two-PC smoke test (see *Try it on two PCs*): link, search the other PC's files, create a file there → appears
     here within a minute (or after Sync now), switch it off → its files stay searchable (offline).
   - ⏳ Look over the new UI in the window (sidebar groups, Link a PC dialog, Settings → Linked PCs); the browser
     preview `cd app; pnpm dev` shows a mock linked "Laptop" (code `482913`).
   - Note: `Seek.bat` runs the console service with sync on; the first run may show a Windows Firewall prompt
     (the rule is created only by `seek-service install`).

## Open findings / backlog (ranked)

1. Empty query sorted by path over 10M ≈ 3.4 s (fix in 3).
2. Every service start rescans walked drives (C: ≈ 60 s without admin); a verify pass would cost the same walk.
   App-fed devices are visible to all users (see access rule). An interrupted `MuxClient` write could desync framing.
3. Path sort builds every hit's full path: 2M hits ≈ 555 ms. Could sort folders once and order hits by
   (folder rank, name) — "tree order", slightly different from plain string order for names with `~`, `{`.
4. Empty query sorted by name over 10M ≈ 0.5 s (consider a pre-sorted name order per index).
5. UI: no inline query syntax highlighting (spec §8.2).
6. Google Drive (`G:`, virtual FAT32) is treated as a removable drive; classify cloud-sync drives separately.
7. Uninstall keeps `%ProgramData%\Seek` (indexes); add an opt-in "delete my index" step. MSI target.
8. Shares: rescan only changed folders too (needs incremental pushes instead of whole indexes).
9. Later specs: self-hosted server sync (and relaying between PCs that can't reach each other) → macOS/Linux →
   content search / duplicates.
10. Sync: open a remote file (fetch it from the other PC); a PC that joined through another member has no user
    links with the third PC (its other users' folders stay hidden there); a reinstalled PC must be unlinked and
    linked again by hand.

## Code map

```
crates/seek-core     OS-agnostic: model.rs, index.rs (DeviceIndex), persist.rs (.seekidx), registry.rs (devices.json),
                     query.rs (parser), matcher.rs, search.rs (parallel search + materialize), synth.rs, benches/search.rs
crates/seek-scan     volumes.rs (list_volumes, scan_volume), ntfs.rs + mft.rs (raw MFT), walk.rs (jwalk, private pool),
                     usn.rs (UsnFollower), watch.rs (DirWatcher: changed folders per 2 s burst), walk.rs also rescan_dirs
crates/seek-sync     OS-agnostic sync: journal.rs, frame.rs, noise.rs (Noise IK), pairing.rs (SPAKE2), peers.rs (peers.json,
                     host.key), wire.rs (Msg), session.rs (SyncHost + run_session)
crates/seek-ipc      protocol.rs (Request/Response/Event, camelCase JSON), frame.rs, client.rs (Client), server.rs (pipe + SDDL)
crates/seek-service  lib.rs (run loop), state.rs, monitor.rs (volumes, scans, journals, folder updates), server.rs (requests),
                     access.rs (caller elevation/SID, hidden profile folders), journal_ops.rs (journaled index changes),
                     sync_host.rs (SyncHost for the service), peers.rs (PeersRuntime: listener, dialer, mDNS, pairing),
                     svc.rs (+ firewall rule) + main.rs
crates/seek-cli      `seek` binary: volumes, scan, query, remote, devices, perf (perf.rs)
app/                 React UI (src/api/{types,mock,tauri,index}.ts, components/, styles/tokens.css)
app/src-tauri        Tauri backend (pipe, commands, shares, tray, settings, service_ctl, shell)
Seek.bat, Stop Seek.bat, scripts/   one-click launcher / stopper
```
