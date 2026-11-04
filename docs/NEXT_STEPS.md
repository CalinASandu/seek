# Seek — Status & Next Steps

_Last updated: 2026-10-16 (Space view implemented, plan tasks 1–8). Read this first when starting a new session in this repo._

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
| 5 | sync 1–16 | **Sync between PCs** (`docs/superpowers/plans/2026-10-11-seek-sync.md`): seek-sync crate, service runtime, IPC, UI, perf, firewall · two-PC smoke test ✅ (user, 2026-10-14) | ✅ committed (`3df1d0e`, `e5fd9b9`) |
| 6 | space 1–8 | **Space view** (`docs/superpowers/plans/2026-10-16-seek-space-view.md`): folder totals in seek-core, 3 IPC requests, sunburst + list UI · real-window check ⏳ (user) | ✅ uncommitted |

- Branch: `feat/v1`, pushed to `origin` (`github.com/CalinASandu/seek`). Last commit `e5fd9b9`.
  The Space view (wave 6) is **uncommitted** — commit only when the user says so.
- UI: Apple-style; the Space view had a screenshot critique pass (light, dark, 900×560) in the browser preview.
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

Tests (2026-10-16): `cargo test --workspace` → 141 passed, 2 ignored (admin-only); `cargo clippy --workspace --all-targets -- -D warnings` clean; `pnpm -C app test` → 43 passed; `pnpm -C app build` passes. (2026-10-12: 123 passed / 18 app tests.)

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
- **Install**: `seek-service install` adds the firewall rule `Seek sync` (TCP 47321, **all network profiles,
  remote IP = local subnet only** — Windows marks new networks Public, and a Private-only rule blocked sync,
  2026-10-12); `uninstall` removes it. Console mode syncs by default and (re)adds the rule when elevated;
  `--no-sync` turns it off. The higher host id dials too after 10 s without a session (was 2 min); failed
  dials are logged once per distinct error in `service.log`.
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

## UI redesign + drive filter (2026-10-12)

- **Same-named drives**: clicking a drive whose name another drive shares (every PC has `Local Disk (C:)`) adds
  `drive:"PC/Drive"` (exact PC name + drive name); unique names stay `drive:"Name"`. Core: `drive_on_pc` in
  `matcher.rs`. Mock laptop C: renamed to `System (C:)` to show the case.
- **Apple-style pass** (user's request): Inter Variable (bundled, `@fontsource-variable/inter`, opsz) with
  size-specific tracking, Apple system colors, translucent sidebar over Mica + solid rounded content sheet,
  Finder-style list (stripes, accent selection when focused / gray when not, blue folders), macOS menus
  (thick material, accent highlight), floating Settings sheet, no-overshoot spring easing (`--spring`),
  press feedback on pointer-down, reduced motion / transparency / more-contrast support. Windows caption
  buttons stay on the right. Tests made robust against the mock's scanning drive (re-query rows).

## Done in wave 6: Space view (2026-10-16)

Spec `docs/superpowers/specs/2026-10-16-seek-space-view-design.md`, plan `docs/superpowers/plans/2026-10-16-seek-space-view.md`.

- **Core** `crates/seek-core/src/space.rs`: `FolderTotals::compute` (child lists + one post-order pass, ~21 B/row,
  a visible-row bitset), `space_tree` (sorted; level 1 capped at 500 + "smaller items", deeper levels merge
  items < 0.5 %), `largest_files`, `type_totals`, `type_of`. Hidden rows (other users) are summed into one
  *Other users* node and never named. `DeviceIndex::version()` (bumped by `apply`) keeps cached totals honest.
- **Service**: `SpaceTree` / `SpaceLargest` / `SpaceTypes` (path = full folder path, mount = drive root); totals
  cached for the 2 most recent (device, user) pairs; a missing or hidden folder → "That folder no longer exists".
- **App**: Search | Space segmented control centered in the title bar (`Ctrl+Shift+S`); `components/space/*`:
  sunburst (3 rings, spring drill animation, hover links chart ↔ list ↔ capacity bar), Finder path bar,
  capacity bar in the chart's colours (+ "not indexed" + free), Contents / Largest / By type. Colour always
  means "this folder"; file kinds use the search-result icons with neutral bars. Mock data:
  `api/mockSpace.ts` (realistic trees; `pnpm dev` → Space).
- **Perf** (2M entries): totals 113 ms, tree 0.8 ms, largest 4.2 ms, types 29 ms (`seek perf`, `== space ==`).
- Screenshots: `app/docs/screenshots/11-space-*.png`, `12-space-folder-dark.png`, `13-space-types-light.png`.

## Done in wave 7: Seek Hub (2026-10-24 .. 2026-10-25)

A self-hosted hub (`crates/seek-hub`, binary `seek-hub`) that PCs on different networks sync through, reachable
only via Tailscale Funnel. Spec `docs/superpowers/specs/2026-10-24-seek-hub-design.md`, plan
`docs/superpowers/plans/2026-10-24-seek-hub.md`. Packaging: `docker/` (Dockerfile, compose, Funnel serve config,
tailnet policy example, README with the setup flow and the Pi checklist), `scripts/build-hub-image.ps1`.

- How to run (no Docker on the Windows PC): `.\scripts\build-hub-image.ps1` builds `docker\dist\{arm64,amd64}\seek-hub`;
  `scp -r docker pi@<pi>:seek-hub`; on the Pi `docker build --build-arg TARGETARCH=arm64 -t
  ghcr.io/calinasandu/seek-hub:0.1.0 .`, then follow `docker/README.md`. One-shot commands are `docker compose run
  --rm --no-deps seek-hub <cmd>`; `init` runs only while the hub is stopped (`docker compose stop seek-hub; docker
  compose run --rm --no-deps seek-hub init; docker compose start seek-hub`); `init --keys` and `rotate-key` run with
  the hub stopped via `docker run --rm -v "$PWD/data:/data" -v "$PWD/keys:/keys" <image> <cmd>`; `link`, `status`,
  `pcs`, `unlink`, `doctor` use `docker exec seek-hub seek-hub <cmd>`. `data/` and `keys/` need `chown 10001:10001`.
  A value in `docker/.env` (`SEEK_HUB_PUBLIC`, `SEEK_HUB_LAN`) overrides what `init` saved.
- Linux tests (done 2026-10-27, 79 unit + 11 CLI pass on Alpine under WSL): `wsl -d seek-alpine --cd
  /mnt/d/Programming/seek -- sh -c ". /root/.cargo/env && CARGO_TARGET_DIR=/root/seek-target cargo test -p seek-hub"`.
  The release binaries cross-build here (zig + cargo-zigbuild, both musl targets; arm64 about 4.4 MB).
- Not run yet: the image build (`docker build` on the Pi), everything on the Pi, and the Funnel serve config against a
  live tailscaled.

**Pi checkpoint (user)** - the commands are in `docker/README.md`, last section, in this order: copy `docker/` to the
Pi, `docker build`, `mkdir -p data keys tailscale && sudo chown 10001:10001 data keys && sudo chown root:root tailscale`, fill `.env`, `init --keys`,
optional `init`, `compose up -d`, `compose logs`, `openssl s_client` (the certificate can take a few minutes: retry),
`doctor --public`, link two PCs (one on a phone hotspot without Tailscale), search across them, `seek-hub status`.

## Quality of life pass (2026-11-02)

- **Mouse side buttons + Alt+←/→**: Space keeps a browser-style history (`hooks/useSpaceNav.ts`): every folder and drive
  change is a step, a new move drops the steps ahead, a vanished folder replaces its entry instead of adding one. It lives
  in `Shell`, so the folder, history and tab survive switching to Search. In Search, back only returns to Space after
  "Search in this folder". Buttons do nothing behind a dialog. `hooks/useMouseNav.ts` listens on the window.
  **Not yet pressed with a real mouse in the Tauri window**: tests dispatch events for buttons 3 and 4.
- **Search box holds only typed text**: `SearchInput { text, filters }` in `lib/query.ts`. `composeQuery` puts filters first,
  so an unfinished quote can't swallow them; parse errors are shifted back to the typed text. Files/Folders/Online/Offline
  light up; drive and folder filters also get a removable pill. X and Esc clear only the text.
- **Several drives now mean "any of these"** (`drive:"A"|drive:"B"`): spaces AND terms, so two selected drives used to
  return nothing.
- **Beam switch** (the user's pick from a five-way test page, since deleted): `components/Segmented.tsx` is the Search/Space
  switch and the Space tabs. A capsule on two spring edges, with the labels repeated in its ink colour and clipped to it.
  **Not yet seen in the real window or at the app's 32 px title bar** (the dev server was stopped for low memory). Settings
  and the Link dialog keep the older `.segmented` control.
- Maybe later (user): a shortcuts page in Settings.

## Next task: Cleanup and uninstaller (asked for 2026-11-04, nothing built yet)

The user wants the Cleanup area and uninstaller built next, with a panel as polished as the rest of the app: Apple-style, clean,
smooth animations (read the `emil-design-eng` skill first). Suggested setup: Opus orchestrates and owns design and final UI review,
Sonnet agents code. Write a design spec first, since this is the first time Seek deletes anything.

Before starting: the RAM and "seen" fixes below may not be running yet (needs `Rebuild Seek.bat`; check with `seek memory`), and
that work is uncommitted. Ask the user about committing it first; agents should not build on a large uncommitted tree.

**Proposed, not yet confirmed by the user:**

- A cleanup area beside Search and Space. Files only go to the Recycle Bin, after review of a list, never permanently deleted.
- Deletes run from the window with the user's permissions, never from the SYSTEM service.
- A duplicate means identical contents, checked by comparing the files; a name and size match is never enough.
- Only this PC's files can be deleted.
- Every selector uses `Segmented.tsx` (Link a device, Settings theme, Search/Space, Space tabs). This also fixes Hub wrapping onto
  its own row in the Link dialog. The gray beam applies to dark mode (now `#f5f5f7`).

**Ask together, numbered, before designing (suggested defaults):**

1. "Professional uninstaller": (a) Seek uninstalls other apps and removes what they leave behind, like Revo, or (b) a more polished
   uninstaller for Seek itself? Default: a, plus the small version of b.
2. What counts as a leftover? Default: Program Files and AppData folders no installed app owns, plus Seek's own data. No registry
   cleaning in the first version.
3. Duplicates across PCs? Default: this PC's drives first; a linked PC's duplicates can be listed but not deleted.
4. What is protected? Default: Windows, Program Files and other users' folders can't be selected. Seek suggests which copy to keep
   and never picks for the user.
5. Beam gray? Default: `#c7c7cc`, dark label text kept, glow softened.
6. More than the new selector on the Link panels? Default: steadier spacing, a dialog that keeps its height when switching tabs,
   tabs ordered Show a code, Enter a code, Hub.

## RAM investigation (2026-11-02 to 11-04, idle RAM fixed; see "Not explained" below)

The user wants `seek-service` near zero while idle in the tray, using more only while loading. Task Manager showed
593 MB; the process had 611 MB working set, 1,181 MB private, 1,259 MB peak. Measured by loading a copy of the real data:

- 5 indexes, 4.84M rows: **600 MB** (130 B/row), held in RAM permanently. This is what Task Manager's number mostly is.
- 2 journals, 112 MB on disk: **269 MB** as heap objects. **Fixed, uncommitted, not yet running on this PC**:
  `seek_sync::DeviceJournal` keeps only file offsets, 13 MB on the same data. Needs the elevated rebuild + service restart.
- About 310 MB of the 1,181 MB is **still unattributed**, and so is the 1,259 MB peak. Not measured yet; don't assume a cause.
- Stored searches are small (8 B per hit, about 39 MB for an all-files search, 2 kept per connection).
- Dropping everything returned the process to **3 MB**; reloading all 5 indexes takes **449 ms** in parallel, 864 ms in series.

**Idle unloading is built (uncommitted, not yet running on this PC)**, `crates/seek-service/src/parking.rs`. A device is in
`State::indexes` or `State::parked` (its saved file plus newer journal entries). The service starts with everything
parked; `snapshot()` (searches, Space, fetching rows) and `Request::Wake` load what is visible, in parallel; `idle_tick`
(every 5 s) parks anything unused for 60 s (`IDLE_UNLOAD`). Own devices park without saving: USN changes keep going to
the journal and the next load replays them. Other PCs' copies have no journal here: they are saved before parking, and a
delta for one loads it. `fold_parked` writes a backlog over 25 MB to its file so the journal never trims what a parked
device needs. The app's `keep_service_awake` (`app/src-tauri/src/lib.rs`) sends `Wake` when the window shows and every
20 s while it does. Parking clears stored searches, so a late `FetchRows` answers "results changed; search again" and the UI
re-searches. `PROTOCOL_VERSION` is now 2: the app and service must be rebuilt together.

Measured with the real service code on a copy of the real data (no volumes attached, so no live USN following):

| Moment | Working set | Private |
|---|---|---|
| Started, nothing searched | 29 MB | 24 MB |
| After a search (window open) | 634 MB | 949 MB |
| 75 s idle | 31 MB | 27 MB |

First search from idle 517–543 ms, later ones 18.5 ms. The ~310 MB gap between private and working set reappears on load
and goes away on unload; what holds it is still not attributed.

Limits: a linked PC that keeps sending changes keeps its drive loaded; a parked drive's entry count updates at its next
load.

**Measuring it: `seek memory`** asks the running service. It prints working set, committed memory, the heap (a counting
`#[global_allocator]` in `seek-service/src/memory.rs`), every drive (in memory or on disk, rows, RAM, idle time, loads, unloads,
last user, why it was refused unloading), the heap by owner and by column, and which code asked for indexes how often. Loads,
unloads and refusals are in `service.log` at INFO with their reason. `seek hub` shows the hub link and each drive's dates.

**Found with it (2026-11-04, live service, 4.96M rows):**

- Idle in the tray or minimized: **21 MB working set, 12 MB committed**, 8 MB heap, of which 7.5 MB is journal offsets. Opening the
  window loads everything in about 0.6 s; 60 s after it is hidden it is back at 21 MB.
- **Every column held exactly twice what it needed** (`ids` 16 B/row for an 8 B value): a loaded column has no spare room, so the
  first change after loading made each `Vec` double. That was the ~310 MB "unattributed" gap: committed, mostly never touched, so
  it did not show in Task Manager. `DeviceIndex::apply` now reserves what a batch needs and grows by 1/32; building and compacting
  end with `trim()`.
- **Not explained:** the run before this diagnostic stayed at 700 MB for 20+ minutes with the window hidden, on the same unloading
  logic. It did not recur on the next build. If it does, `seek memory` names the cause (last user, or `stays in memory: ...`).

In-use RAM by column, if it ever needs to go lower (window open, after the doubling fix): names and lowercase names about 110 MB
each, `id_to_row` 136 MB (a `hashbrown::HashTable<u32>` would need about 45 MB), sizes/ids/modified/created 38 MB each. The load
peak (950 MB) is the decompressed file plus the columns; decoding columns as a stream would about halve it.

**"Seen 2 days ago" on another PC's drives (fixed 2026-11-04):** the owner stamps `last_seen` only when a drive comes or goes, so a
drive that stays plugged in showed the time its service started, while the data was up to date. A drive reported online by its PC
now counts as seen when that PC last synced (`apply_remote_devices`), and when a direct session ends. The date moving alone is not
saved or announced.

## Next step

0. **Space view in the real window (user)** — the release binaries were locked while the session ran:
   elevated: `Stop-Service SeekService`; close the Seek window; `cargo build --release -p seek-service -p seek-cli`;
   `cd app; pnpm tauri build --no-bundle`; `Start-Service SeekService`; open Seek → Space. Check C: totals look
   right next to Explorer's "used", drilling feels instant, and the laptop's drives open while it is off.
   Then: **installer for sharing** (code signing, GitHub Releases, Tauri updater) — discussed 2026-10-16.

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
     `netsh advfirewall firewall show rule name="Seek sync"` → one rule, Profiles Domain,Private,Public,
     RemoteIP LocalSubnet, TCP 47321.
   - ⏳ Two-PC smoke test (see *Try it on two PCs*): link, search the other PC's files, create a file there → appears
     here within a minute (or after Sync now), switch it off → its files stay searchable (offline).
   - ⏳ Look over the new UI in the window (sidebar groups, Link a PC dialog, Settings → Linked PCs); the browser
     preview `cd app; pnpm dev` shows a mock linked "Laptop" (code `482913`).
   - 2026-10-12 first two-PC try (desktop on Ethernet, laptop ROCKET on Wi-Fi, both Public): linking worked,
     sessions never connected — the Private-only rule blocked the incoming side. Fixed as above.
   - ✅ 2026-10-14 retest on two PCs (user): sync works end to end.

## Open findings / backlog (ranked)

H. ~~Linux check (hub)~~ **DONE 2026-10-27**: native `cargo test -p seek-hub` passes on Alpine under WSL (79 unit + 11 CLI) after
   a test-only fix in `tests/cli.rs`. Rerun command is in the wave-7 section above.

Hub (from the final review of wave 7; the first-link race, pre-auth lock, OOM caps, metadata caps and compaction
   memory were fixed in the final fix pass):
   - I6 append-only journal: `put_delta` rewrites and re-encrypts the whole journal per batch under the store lock (SD wear, latency).
   - Noise msg-1 replay kills the live session before a frame proves the key.
   - `pc_by_key` / `sessions.insert` race with unlink (a just-unlinked PC can briefly hold a session).
   - Admin socket: bind-then-chmod gap (set a umask before binding).
   - Global 60/min handshake limiter: behind Funnel everything comes from localhost, so a stranger can lock PCs out; check PROXY protocol.
   - Idle trickle (a peer sending 1 byte per 3 min keeps a session alive).
   - Minor: `PairingCode` digits 0, `HubAddr` trailing dot, `SEEK_HUB_LAN` only accepts "1", `commit_devices` after a `commit_pcs` failure,
     split `store.rs` per R11 (copy-file helpers), serve the stored copy decrypted outside the store lock, count PC metadata toward its bytes.

Hub, PC side (final review; I1-I4, M1, M2, M5, M6 were fixed in the final fix pass):
   - M4 the upload debounce is a throttle: a busy drive sends hub traffic about every 2 s (consider a 10-15 s minimum gap).
   - M8 `PairInfo.user_name` is never filled (needs `LookupAccountSid`), so the hub's PC list shows no user names.
   - `start_lan` failing after a hub-initiated unlink is only logged; `link_hub` has no re-entry guard (two links at once).
   - Catalog upsert merges `linked_users` forever (a user the hub dropped stays linked here).
   - The hub's `last_seen_ms` is compared with the local clock (skew makes a PC look offline/online wrongly).
   - `seek-cli` links `seek-hub`, which makes `seek.exe` bigger.

0. Space (from the wave-6 self-review): computing totals holds the device's read lock (~0.1 s per 2M rows,
   once per index change); the live refresh refetches the shown folder ~1 s after every index change, even when
   the change was elsewhere on the drive; Space has no delete yet (Cleanup feature, reversible only).

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
                     query.rs (parser), matcher.rs, search.rs (parallel search + materialize), synth.rs, benches/search.rs,
                     space.rs (folder totals, Space trees, largest files, file kinds)
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
app/                 React UI (src/api/{types,mock,mockSpace,tauri,index}.ts, components/ (+ space/ for the Space view),
                     lib/sunburst.ts, styles/{tokens,app,space}.css)
app/src-tauri        Tauri backend (pipe, commands, shares, tray, settings, service_ctl, shell)
Seek.bat, Stop Seek.bat, scripts/   one-click launcher / stopper
```
