# Seek — Status & Next Steps

_Last updated: 2026-10-06 (wave 4 part 1: review fixes, perf, installer). Read this first when starting a new session in this repo._

## Where we are

| Wave | Plan tasks | What | State |
|---|---|---|---|
| 0 | 1 | Workspace scaffold, shared model + IPC types | ✅ committed on `main` (`7a935fb`) |
| 1 | 2–4 | `seek-core`: index, persistence, registry, query parser, search, bench | ✅ done, reviewed + fixed, **uncommitted** |
| 1 | 5–6 | `seek-scan`: volumes, MFT parser/scanner, walker, USN follower, watcher | ✅ done, reviewed + fixed, **uncommitted** |
| 1 | 9 | `app/` UI (React + Vite) against a **mock** API | ✅ done, reviewed + fixed, **uncommitted** |
| 2 | 7–8 | `seek-ipc` framing/client/server, `seek-service`, `seek-cli` | ✅ done, reviewed + fixed, **uncommitted** |
| 3 | 10 | Tauri backend: real window ↔ service, tray, autostart, shares, launcher `Seek.bat` | ✅ done, reviewed + fixed, **uncommitted** |
| 4 | 11 | Review + fixes ✅, perf suite ✅, walker memory ✅, NSIS installer ✅ (built, not yet installed) · elevated verification ⏳ (user) · visual pass + README ⏳ | 🔶 in progress |

- Branch: `feat/v1`. Everything since the scaffold is **uncommitted on purpose** — the user reviews before committing.
- Spec: `docs/superpowers/specs/2026-10-04-seek-local-design.md`
- Plan (task-by-task): `docs/superpowers/plans/2026-10-04-seek-v1.md`
- Execution ledger + agent reports: `.superpowers/sdd/2026-10-04-seek-v1/` (git-ignored; `progress.md`, `agent-*-report.md`)

### Verified on the real machine (2026-10-06, non-elevated, walker fallback)

| Measure | Result | Target |
|---|---|---|
| Index C: (2,082,182 entries) | 60 s (walker) | NTFS MFT path: < 10 s / 1M — **not yet measured** (needs admin) |
| Index D: (717,994) / G: (25,832) | 16.7 s / 11.9 s | — |
| Service RAM, 2.8M entries | ~372 MB | — |
| Typical query over 2.8M | 1.7–7.5 ms | < 50 ms ✅ |
| Empty query (all 2.8M, sorted) | 117 ms | — |
| Reload C: index from disk | 327 ms (45 MB file) | < 1 s ✅ |
| Synthetic bench, 10M entries, `report` | 17.8 ms | < 50 ms ✅ |

Tests: `cargo test --workspace` → 60 passed, 2 ignored (admin-only); `cargo clippy --workspace --all-targets -- -D warnings` clean; `pnpm -C app test` → 13 passed; `pnpm -C app build` passes.

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

Admin-only checks still to run **from an elevated terminal** (stop the service first):
```powershell
cargo test -p seek-scan -- --ignored --nocapture
.\target\release\seek.exe scan C:\        # expect "via NTFS MFT"; target < 10 s per 1M entries
```

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
- **Not fixed — needs a decision (6):** the pipe lets any logged-in user search every file name on the PC
  (service is SYSTEM). Fine for a single-user PC (Everything works the same); on shared PCs other users'
  private file names are visible. Options: keep + document · restrict the pipe to Administrators + the
  installing user · filter results per caller (expensive).

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

## Next step (needs the user)

1. **Decide finding 6** (pipe access on shared PCs) — see above.
2. **Elevated verification**: run `Seek.bat`, accept the admin prompt → service log shows `journal: true` and C:
   in seconds; create/rename/delete a file → visible in search within ~2 s. Then, elevated:
   `cargo test -p seek-scan -- --ignored --nocapture` and `.\target\release\seek.exe perf --scan C:\`.
3. **Installer test**: stop dev Seek (`Stop Seek.bat`), run the setup exe, check `SeekService` is running
   (`sc query SeekService`), the window finds files, then uninstall from Settings → Apps and check the service is gone.
4. Visual pass on the real window (light + dark, tray, menus, add-share dialog), then the **design session**.
5. README: what Seek is, install/run, architecture, query syntax.

## Open findings / backlog (ranked)

1. Finding 6 (above). Also: an interrupted `MuxClient` write could desync framing (no caller cancels today).
2. **Elevated path unverified**: raw-MFT scan, USN live updates, `OpenFileById` metadata.
3. Watched volumes (non-NTFS, non-elevated NTFS) rescan the whole volume on change (rate-limited). Better: re-walk
   only the changed folders (notify gives paths). Every service start also rescans walked drives (C: ≈ 60 s).
4. Path sort builds every hit's full path: 2M hits ≈ 555 ms. Could sort folders once and order hits by
   (folder rank, name) — "tree order", slightly different from plain string order for names with `~`, `{`.
5. Empty query sorted by name over 10M ≈ 0.5 s (consider a pre-sorted name order per index).
6. UI: no inline query syntax highlighting (spec §8.2).
7. Google Drive (`G:`, virtual FAT32) is treated as a removable drive; classify cloud-sync drives separately.
8. Uninstall keeps `%ProgramData%\Seek` (indexes); add an opt-in "delete my index" step. MSI target.
9. Later specs: LAN sync → self-hosted server sync → macOS/Linux → content search / duplicates.

## Code map

```
crates/seek-core     OS-agnostic: model.rs, index.rs (DeviceIndex), persist.rs (.seekidx), registry.rs (devices.json),
                     query.rs (parser), matcher.rs, search.rs (parallel search + materialize), synth.rs, benches/search.rs
crates/seek-scan     volumes.rs (list_volumes, scan_volume), ntfs.rs + mft.rs (raw MFT), walk.rs (jwalk, private pool),
                     usn.rs (UsnFollower), watch.rs (DirWatcher, 2 s debounce)
crates/seek-ipc      protocol.rs (Request/Response/Event, camelCase JSON), frame.rs, client.rs (Client), server.rs (pipe + SDDL)
crates/seek-service  lib.rs (run loop), state.rs, monitor.rs (volumes, scans, journals), server.rs (requests), svc.rs + main.rs
crates/seek-cli      `seek` binary: volumes, scan, query, remote, devices, perf (perf.rs)
app/                 React UI (src/api/{types,mock,tauri,index}.ts, components/, styles/tokens.css)
app/src-tauri        Tauri backend (pipe, commands, shares, tray, settings, service_ctl, shell)
Seek.bat, Stop Seek.bat, scripts/   one-click launcher / stopper
```
