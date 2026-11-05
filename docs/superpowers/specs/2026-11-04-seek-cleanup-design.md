# Seek Decimator: duplicates, leftovers and uninstalling apps

Status: approved by the user, 2026-11-05, including the name and leaving registry cleaning out. Decisions the user made are marked **(user)**. Everything else is the designer's call and can be changed.

The area is called **Decimator** in the app, with the tooltip "Cleans up files" **(user)**. Code and this document say "Cleanup".

## Why

Seek already knows every file on the PC, and the Space view shows what fills each drive. The next question is what can go. The user wants one place to remove duplicate files, remove what uninstalled apps left behind, and uninstall apps properly, like Revo Uninstaller. The panel must be as polished as the rest of the app: Apple-style, clean, with smooth animations.

This is the first time Seek deletes anything. A search tool that loses someone's file has broken its one promise, so safety decides the design wherever it conflicts with convenience.

## Goals

- A third view beside Search and Space, with tabs for Duplicates, Leftovers and Apps. What was removed is part of the same view, with no tab of its own **(user)**.
- Find files with identical contents on this PC, and remove the copies the user chooses.
- Find folders in Program Files and AppData that no installed app owns, and remove the ones the user chooses.
- Uninstall desktop apps by running their own uninstaller, then offer what they left behind.
- Every selector in the app uses the shared `Segmented` control, with a darker beam.
- A small polish of Seek's own uninstaller.

## Non-goals (first version)

- Registry cleaning **(user)**.
- Store (MSIX) apps **(user)**: they appear in a later version.
- Deleting anything on a linked PC **(user)**, or comparing contents across PCs.
- Temp-file, browser-cache or "junk" cleaning. Nobody asked for it, and it invites the guessing this design avoids.
- A hash cache between scans.
- A scheduled or automatic cleanup. Cleanup only ever acts when the user presses a button.

## Anti-goals: this has failed if

- A file is permanently deleted when the user was told it could be put back.
- The last copy of a file is removed.
- Something is removed that the user was not shown first.
- An item is ticked that the user did not tick. Suggestions are labelled, never pre-selected.
- A running or installed app breaks because Seek called its folder a leftover.
- Idle RAM goes up. The service's near-zero idle (see `docs/NEXT_STEPS.md`) stays: nothing in Cleanup keeps indexes loaded while the window is hidden, and the app process drops scan results as described under *Duplicates*.

## The safety rules

These are rules of the removal engine in Rust, not of the UI. The UI explains them, but a bug in the UI must not be able to break them.

1. **You see what goes, every time (user).** Every removal passes through one review sheet that lists each item with its full path and size. A folder can be expanded to its files. Any item can be unticked. There is no route around the sheet: no "clean all", no shortcut, no keyboard path. The engine enforces this with two steps. `prepare` takes a fresh look at each item on disk and returns what the sheet shows, with a token. `commit` accepts only that token and a subset of its items.
2. **Recycle Bin only (user).** The engine has no permanent-delete call. Seek's own uninstaller is the single exception, described under *Seek's own uninstaller*.
3. **If an item can't be recycled, it stays.** Windows silently deletes for good when a drive has no bin or a file is too big for it, so the engine checks first and aborts per item (see *The removal engine*).
4. **One copy always survives.** A duplicate group can't have every local copy selected. At removal time the engine compares each file byte for byte with a copy that stays, and removes it only if they match.
5. **Nothing changed since you looked.** At removal time each item's size and modified time must match what `prepare` read from disk, not the index, which can lag. A leftover folder must still be unclaimed, with no running program inside it. Otherwise the item is left in place and the result says why.
6. **Runs with the user's permissions (user).** Deletes run in the window process, never in the SYSTEM service. Program Files and Seek's own folder in ProgramData are the cases that need more, and they ask with one admin prompt for that removal only.
7. **This PC only (user).** Rows from a linked PC can't be selected, and the engine refuses them.
8. **Protected places.** See the table below. `Always protect` lets the user add any file or folder, and the engine enforces that list too.

| Place | Duplicates | Leftovers |
|---|---|---|
| Windows folder, `$Recycle.Bin`, `System Volume Information`, drive roots | Locked | Never listed |
| Program Files, Program Files (x86) | Locked **(user)** | Selectable with an admin prompt **(user)** |
| Other users' profile folders | Locked **(user)** | Never listed |
| Your AppData, ProgramData | Locked | AppData listed; ProgramData never, apart from Seek's own stale files, with an admin prompt |
| Inside `.git`, `node_modules`, `target`, `.venv` | Locked | Not applicable |
| Drives with no Recycle Bin **(user)** | Locked, and the row says why | Not applicable |
| Anything the user marked *Always protect* | Locked | Locked |

A locked copy still counts as a copy. A file in Documents that matches one in Program Files can be removed; the Program Files copy stays.

## How the removal engine works

A new crate, `crates/seek-clean`, holds duplicates, leftovers, app listing and removal. It is the only code that can delete. `seek-core` stays free of Windows dependencies, and the crate is used only by `app/src-tauri`.

**Recycling** uses `IFileOperation` with `FOFX_RECYCLEONDELETE`, plus a progress sink, through the `windows` crate. The workspace only has `windows-sys`, which has no COM wrappers, so `windows` is added for this crate alone. `SHFileOperationW` is not used because it can't report per item or stop per item.

**Before each batch**, for every item:
- The drive must be fixed and local (`GetDriveTypeW`), after resolving links to the real volume.
- `SHQueryRecycleBinW` must succeed for that drive.
- The user's bin settings for that volume (`BitBucket\Volume\{guid}`) must not have `NukeOnDelete` on.
- The batch's size on that volume must fit `MaxCapacity`.
- The path must be short enough for the bin.
- Cloud placeholders (OneDrive, Google Drive files not on disk) are refused, and never read.

**During the batch**, `PreDeleteItem` aborts the item if Windows reports it won't be recycled. `PostDeleteItem` must return a new location in the bin, which is logged. An item Seek can't confirm in the bin is reported as a failure at the top of the result, and the rest of the batch stops. Stopping costs a retry, while carrying on could cost a file.

**First implementation task, a spike:** confirm on this PC what `PreDeleteItem` and `PostDeleteItem` really report, with a test marked `#[ignore]` that recycles a temp file and restores it. The pre-checks must stand on their own, in case the sink's signal is weaker than documented. If `PostDeleteItem` proves unreliable, confirm through the Recycle Bin shell folder instead. If nothing can confirm an item, stop and bring it back to the user rather than loosening rule 3.

**Undo.** Every removal appends to `%APPDATA%\Seek\cleanup-history.json`: time, original path, size, why (duplicate, leftover, uninstall) and bin location. *Put back* restores through the Recycle Bin shell folder's own restore verb, matching original path and deletion date, so Windows' bookkeeping stays right. If the user has since emptied the bin, the item says "No longer in the Recycle Bin". An uninstaller's own work can't be put back, and the UI says so before it runs.

**Admin removal.** Recycling from Program Files or Seek's ProgramData folder starts `seek-app.exe --clean-elevated <job file>` with one UAC prompt, handled before Tauri starts, with no window. The window waits for it to exit and reads its per-item result. A same-user process could edit the job file, so the elevated process trusts nothing in it: it re-checks that every path is inside Program Files and unclaimed and unprotected, or is a stale file in Seek's folder, judged from `devices.json` there. If the UAC prompt was answered with a different account, it refuses, since the files would land in that account's bin where the user can't put them back.

## Spike result

Observed on this PC (Windows 11 26300) with `cargo test -p seek-clean --test recycle_real -- --ignored --nocapture`, recycling a temp file on `C:` through `IFileOperation` with `FOFX_RECYCLEONDELETE | FOF_NO_UI | FOFX_EARLYFAILURE` and a sink passed only to `Advise`.

**A successful recycle** (normal temp file): `PreDeleteItem` once, `dwFlags` `642` (`0x282`: `TSF_DELETE_RECYCLE_IF_POSSIBLE` 0x80, `TSF_COPY_LOCALIZED_NAME` 0x200, `TSF_OVERWRITE_EXIST` 0x2). `PostDeleteItem` once, `hrDelete` `0x00270008` (`COPYENGINE_S_DONT_PROCESS_CHILDREN`, a success code, so test `>= 0`, never `== S_OK`), and `psiNewlyCreated` non-null with a file system path in the bin, `C:\$Recycle.Bin\<user SID>\$R<6 random chars>.<original extension>` (the parsing name is the same string).

**A permanent delete** (observed for real, on files the tests created and then destroyed):
- A sparse file of 100,410 MB, with the bin's `MaxCapacity` at 49,693 MB, was recycled through the normal path with the sink not refusing. Windows deleted it for good. `PreDeleteItem` was identical to the success case (`642`, with the 0x80 recycle bit set). `PostDeleteItem` reported `hrDelete = 0x00270008`, **the same success code**, with `psiNewlyCreated` **null**. `PerformOperations` returned no error and nothing was aborted.
- The same through a `\\localhost\C$\...` path: permanently deleted, `PostDeleteItem` `0x00270008` with a null new item. Here `PreDeleteItem` flags were `514` (`0x202`), without the 0x80 bit, which is the only case where Pre said so beforehand. This path is reachable only with admin shares and is not a case Seek's drive checks would let through.
- With `FOF_WANTNUKEWARNING` added, over 8 runs: 5 destroyed the file silently as above, 2 were cancelled by Windows (`PerformOperations` failed with `0x80270000`, `COPYENGINE_E_USER_CANCELLED`) with the file intact, and 1 left a dialog up until the 60 s watchdog ended the process. The flag is therefore not a dependable guard and Seek does not use it.

**Refusing in `PreDeleteItem`** (returning `E_ABORT`) works: `PostDeleteItem` reports `hrDelete = 0x80004004` with a null new item, and the file stays.

**Restore** through the Recycle Bin shell folder (`SHGetKnownFolderItem(FOLDERID_RecycleBinFolder)`, enumerated with `BHID_EnumItems`, matched on the `$R` path, then the item's `IContextMenu` `undelete` verb with `CMIC_MASK_FLAG_NO_UI` after a `QueryContextMenu`) put the file back at its original path with identical contents, including when the original folder had since been deleted. The bin item's `SIGDN_NORMALDISPLAY` name was the full original path on this PC, not a file name, so Seek compares the full original path (`System.Recycle.DeletedFrom` plus the file name, case-insensitive) and never trusts the display name alone.

**Decision:**
1. *What confirms a successful recycle:* `PostDeleteItem` with `hrDelete >= 0` **and** a non-null `psiNewlyCreated` whose path contains `$Recycle.Bin`. The bin's shell folder is not needed to confirm, only to put back. `recycle_one` returns an error when no such location was reported, whatever else happened.
2. *What a permanent delete looks like:* a success `hrDelete` with a null `psiNewlyCreated`, reported after the file is already gone. Nothing in the sink distinguishes a permanent delete **before** it happens: `PreDeleteItem` flags are identical to a normal recycle for an item too large for the bin. The after-the-fact signal (null new item) exists but arrives too late to save the file.
3. *What prevents one:* only Seek's own pre-checks (fixed local drive, bin enabled, `NukeOnDelete` off, batch fits `MaxCapacity`, no cloud placeholders, no UNC paths). The sink can stop a batch after the first destroyed item but cannot stop that item, so a missing location is reported as a possible permanent deletion, not a retryable failure.
4. *Not observed:* a permanent delete of an item that fit the bin's capacity but was refused for another reason (path too long, `NukeOnDelete` on, a drive with no bin), because creating those cases would need changed bin settings or a non-fixed drive. They are assumed to behave like the capacity case.

**More pre-checks added after review** (each locks the item with its own reason and fails closed when the fact can't be read). The lock names: `PathTooLong` (inner paths), `UnsupportedPath` (volume and root), `BinFull` (what is already in the bin leaves no room), `NoRecycleBin` (drive type, file system, policies, `NukeOnDelete`, a batch that alone exceeds the capacity, an unreadable bin size):
- *Inner paths.* A folder is walked and its longest inner path measured. Locked when the full path exceeds 259 UTF-16 units, or when 80 (`BIN_PREFIX_UNITS`, the bin's own prefix) plus the path relative to the selected folder, including the folder's name, exceeds 259. A single file counts its file name.
- *Volume and root.* The real location's volume mount point must be exactly the drive's `x:\` (no folder-mounted volumes), and the written path's drive letter must equal the real one (no `subst`, no junction to another volume). The top-level length is the longer of the two spellings.
- *File system.* Only NTFS and ReFS pass; any other format, or a failed query, locks.
- *Policies.* `NoRecycleFiles` (non-zero) under `Policies\Explorer` in HKCU or HKLM locks. `RecycleBinSize` caps the capacity at that percent of the volume. A policy value that exists but can't be read as a DWORD locks.
- *Bin occupancy.* What is already in the bin (`SHQueryRecycleBinW`'s size) plus the batch must fit 90% of the capacity. A zero capacity locks. The reason tells the user to empty the Recycle Bin first.
- *Confirming a recycle.* At least one `PostDeleteItem`, and every successful one must name a location under `$Recycle.Bin`. A real folder with a nested folder and three files gave one Post with one location.

**Long inner paths, observed** (Windows 11 26300, `real_folder_with_too_long_inner_path_outcome`, temp folders the test made, 67-unit top folder): inner paths of 241 and of 378 units (full paths 309 and 446) were both recycled normally. One Post each, success `hrDelete`, a location in the bin, the folder gone and restorable. So this Windows did not delete them permanently, and the long-path refusal is a deliberate precaution for other Windows versions or settings, not a rule this PC proved.

## Duplicates

**Finding them.** A new read-only service request, `SameSizeFiles { min_size, device_ids, cursor, limit }`, returns files grouped by exact size, biggest first, with the caller's access rule applied. It is computed by a new function in `seek-core` over the size column. Paging 5M rows through search (500 per page) would take about 10,000 round trips and could be cleared whenever indexes unload; this request avoids that. The cursor is simply the next size to return, so the service stores nothing between pages and unloading can't break a scan. Anything that changes between pages is caught by the later checks. `PROTOCOL_VERSION` goes up.

The window then narrows each same-size group: compare the first and last 64 KB, then the full BLAKE3 hash. Only identical contents form a group **(user)**. Name and size alone never do. Files are also skipped if:
- They are the same file by volume serial and file ID. Hard links aren't duplicates, and removing one frees nothing.
- They are reparse points or cloud placeholders.
- They are under 1 MB in this version. It's a fixed minimum, because small files are many and save little.

**Cost.** Largest groups first, a small fixed buffer, one thread per physical drive, background I/O priority. Groups appear as they are confirmed, so the most useful results come first. Cancel keeps what was found.

**Hiding the window.** The candidates are fetched once at the start, in seconds. After that the scan reads files and needs nothing from the service, so its indexes unload on the usual 60 s timer. The scan lives in the app's Rust process and keeps running while the window is hidden, and the view picks it back up on return. Once the scan has finished and the window has been hidden for 5 minutes, its results are dropped and the tab offers *Scan* again.

**What counts.** A group is shown only if at least one copy can be removed. The headline and the group's figure count only space that can really be freed: removable copies, leaving one copy if none is locked.

**The suggestion (user).** Seek labels one copy *Suggested keep*, and nothing is ticked. It prefers a copy outside Downloads and temp folders, then the shortest path, then the oldest. *Use suggestion* on a group, or *Use suggestions* for all, ticks the others, and those are the user's clicks. Everything still goes through the review sheet.

**Other PCs (user).** Under the local groups, a quiet section lists files with the same name and size on a linked PC, labelled "Same name and size, contents not compared". They are read-only, add nothing to the space total and never count as a surviving copy. Seek can't read those files, so it doesn't claim they are duplicates.

## Leftovers

Candidates are the top-level folders of Program Files, Program Files (x86), `%LOCALAPPDATA%`, `%APPDATA%` and `LocalLow` **(user)**, plus Seek's own stale data.

A folder is **claimed**, and not shown, if any of these point inside it or match it:
- an uninstall entry's install location, icon or uninstall command
- an installed app's display name, matched exactly as whole words
- a Windows service's image path, a `Run` key or `App Paths`
- a Start Menu shortcut target
- a running process

Well-known system and shared folders (`Microsoft`, `Windows`, `Common Files`, `WindowsApps`, `Packages`, `Programs`, `Temp` and the like) are always claimed. The list lives in one table with its own tests.

### The confidence meter

This can't be exact. Portable tools have folders in AppData with no uninstall entry. So every other folder gets a **confidence meter (user)**, an idea taken from Bulk Crap Uninstaller (BCU, Apache-2.0). There, each piece of evidence adds or subtracts points with a written reason, the total becomes a level, and the reasons stay visible. Seek writes its own version of this.

| Evidence | Points | Applies to |
|---|---|---|
| Inside the uninstalled app's recorded install folder | +4 | Apps |
| Folder name matches the uninstalled app's name exactly | +2 | Apps |
| Parent folder matches the uninstalled app's publisher | +4 | Apps |
| Name only roughly matches the app | -2 | Apps |
| Folder is named after the company, not the product | -2 | Apps |
| Each level of depth below the top (at most 2 levels) | -2 | Apps |
| Another installed app has a similar name | -2 | Apps |
| Empty folder, or nothing left but empty folders | +4 | Both |
| No subfolders | +2 | Both |
| Nothing inside changed for a year | +2 | Both |
| Generic name (`Data`, `Common`, `Shared`, `Bin`, `Config`) | -3 | Both |
| Contains programs (`.exe`, `.dll`) | -4 | Both |
| Many files | -2 | Both |
| The publisher still has an app installed | -4 | Both |
| Changed in the last 30 days | -3 | Both |

Names of 4 letters or fewer never count as a match. That rule and the "similar name" penalty come from BCU, which guards against `AppX Extended` being matched as junk from `AppX`.

| Total | Level shown | Meter |
|---|---|---|
| 5 or more | Very likely left over | 4 of 4 |
| 2 to 4 | Likely left over | 3 of 4 |
| 0 to 1 | Not sure | 2 of 4 |
| Below 0 | Probably still in use | 1 of 4 |

- Rows are sorted by confidence. "Probably still in use" starts collapsed under its own heading.
- The level is always written out, never colour alone. Hovering the meter, or expanding the row, lists the reasons in plain sentences with their direction, for example "Contains programs" or "Nothing inside changed for a year".
- **The meter never ticks anything.** A high score is a suggestion, and every item still needs the user's click and the review sheet. The review sheet shows the same level beside each folder.
- The points are one table in `seek-clean` with tests, so they can be tuned once they've been seen on real folders.

The list never says "safe to delete".

**Seek's own data (user):** index and journal files in `%ProgramData%\Seek` that no known drive owns. Only the service knows the drives, so a second read-only request, `StaleData`, returns these files, and removing them takes the admin prompt because the SYSTEM service owns that folder. Live indexes are never listed. `%LOCALAPPDATA%\Seek-dev` is in the always-claimed table, since a window can't reliably tell whether a dev service is using it.

## Apps (the uninstaller)

**The list.** Apps come from the three `Uninstall` registry locations (HKLM, HKLM WOW6432Node, HKCU). Each row shows icon, name, publisher, version, size and install date, with a filter box and sorting. Hidden: entries with `SystemComponent=1`, a `ParentKeyName`, no uninstall command, and Seek itself. Runtimes and drivers (Visual C++ Redistributable, .NET, and the like) stay listed, with a note that other apps may need them.

**Its folder.** `InstallLocation` first, else the folder of the `DisplayIcon` or uninstall executable. A derived folder is rejected if it is under Windows, `Windows\Installer`, a temp folder or `ProgramData\Package Cache`, or if it is a Program Files root itself. With no folder the app is still listed: step 1 says Seek couldn't find its folder, and leftovers come from name and publisher matches only.

**The flow**, one sheet with three steps:

1. **Before.** Shows the app and its install folder. It says plainly: "This app's own uninstaller runs next, and that can't be undone (user)." It warns if the app is running. No restore point is made **(user)**.
2. **Uninstalling.** Seek runs the app's normal uninstaller, visibly, not its quiet one. It handles quoted paths and `MsiExec /X{GUID}`, and asks for admin only if Windows says it must. Many uninstallers start a temp copy and exit, so "finished" means the process ended *and* the registry entry is gone. If it doesn't resolve, the sheet offers *It's finished* and *Cancel*.
3. **What's left.** Lists the leftovers: anything still in the install folder, folders in Program Files and AppData found by name and publisher up to two levels deep, and Start Menu shortcuts. Each has its confidence level and reasons. Nothing is ticked. This list goes through the same review sheet and removes to the Recycle Bin.

**What "you see what gets removed" covers (user).** It means the leftovers Seek removes. What the app's own uninstaller removes is that uninstaller's business, and Seek doesn't list it.

## Seek's own uninstaller

The small polish **(user)**: `NSIS_HOOK_POSTUNINSTALL` in `app/src-tauri/windows/hooks.nsh` adds a step, off by default, "Also delete Seek's index and settings". It removes `%ProgramData%\Seek` and that user's `%APPDATA%\Seek`. This is permanent, because the installer runs outside the user's session and the app is gone afterwards, so the prompt says so. It is the one permanent delete in this design, and only ever of Seek's own data.

## The UI

### Layout

`ViewSwitch` gains a third item: `Search | Space | Decimator`, with the tooltip "Cleans up files (Ctrl+Shift+D)". `Ctrl+Shift+D` toggles it, the way `Ctrl+Shift+S` toggles Space. The view is kept across switching, like Space. The sidebar stays; clicking a drive narrows Duplicates to it.

The tabs are a `Segmented` control, as in Space: **Duplicates | Leftovers | Apps**.

Each tab has the same frame:
- A head with one headline number ("38.2 GB in duplicates") and its one action (*Scan*, *Rescan*).
- A thin scan line while working, plus what it's doing now and *Cancel*.
- The list.
- A bottom bar that appears once anything is ticked: "12 items, 4.1 GB" and *Review…*.

**Duplicates** is one row per group: icon, name, copies, size that can be freed. Expanding shows every copy with its full folder, modified date, a tick, *Suggested keep* and any lock with its reason. Reveal in Explorer and *Always protect* are on the context menu, reusing `ContextMenu`.

**Leftovers** is one row per folder: name, parent, size, last changed, confidence meter, shield if it needs admin. Expanding shows the reasons and its contents.

**Apps** is the list above, with *Uninstall…* on the selected row.

**Recently removed** is not a tab **(user)**. A quiet button in the head of all three tabs opens a sheet listing past removals, newest first, with *Put back* and *Open Recycle Bin*. A failed or skipped item keeps its reason.

### The review sheet

Built on `Dialog`, used by every tab, wider than the Link dialog, and tall enough to read a real list (virtualized for thousands of items).

- Title says the action and count: "Move 12 items to the Recycle Bin?" It updates as things are unticked, and so does the total.
- Rows grouped by folder, each with a tick, full path and size. Folders expand to their files.
- Program Files items have a shield, and the button reads "Move 12 items… (asks for admin)".
- Focus starts in the list, never on the action button, so Enter can't remove anything by accident. Esc cancels.
- The result replaces the list in the same sheet: "Moved 12 items, 4.1 GB", then anything not moved with its reason, and *Put back*.

### Motion

Following `emil-design-eng` and the app's tokens (`--spring`, `--t`, `--t-spring`):

| What | How | Why |
|---|---|---|
| Sheet opens | Existing `materialize`, from center | Consistent with other dialogs |
| Group expands | `grid-template-rows` 0fr to 1fr over 200 ms, chevron rotates | Shows where the rows came from |
| Removed rows | Opacity, then height collapses, 200 ms, stagger capped at 30 ms and only for visible rows | Shows what went; the list closes up instead of jumping |
| Bottom bar appears | Rises 8 px with fade, 220 ms `--spring` | Occasional, and it answers a tick |
| Group found during a scan | `rise` once, no stagger, only if inserted in view | Results arriving; never moves what the user is reading |
| Scan line | `transform: scaleX`, linear | Constant motion |
| Tick and buttons | `scale(0.97)` on press, 100 ms | Immediate feedback |
| Tab change, keyboard moves in lists | None | Done dozens of times; animation would slow them down |

Under reduced motion, movement is dropped and fades stay, matching the existing tokens. Numbers change in place with tabular figures, without counting up.

### Selectors

- The dark-mode beam changes from `#f5f5f7` to **`#aeaeb2`**. The user asked for darker than the `#c7c7cc` first suggested. Label ink stays `#1d1d1f` (contrast about 8:1), and the white glow becomes a soft dark shadow. Light mode keeps its dark beam. The colour is one line in `.seg-face`, to be judged in the real window.
- `Segmented` gains optional icons on items (the theme selector has them) and a full-width style for dialogs.
- Settings' theme selector and the Link dialog move to it. The old `.segmented` CSS and `.link-tabs` are deleted.
- Link dialog **(user)**: steadier spacing, a dialog height that doesn't change between tabs, tabs ordered Show a code, Enter a code, Hub. The current two-column override is what wraps Hub onto its own row.

### Settings

A *Protected from Cleanup* section lists the *Always protect* entries, with remove buttons.

## Testing

Everything that decides what may be removed lives in Rust with no UI, so it can be tested directly.

- **`seek-core`:** same-size grouping, including hidden rows, folders and deleted rows.
- **`seek-service`:** `SameSizeFiles` paging across an idle unload, `StaleData` listing only files no known drive owns.
- **`seek-clean`, on temp folders:**
  - Grouping: same size with different contents isn't a group, nor are hard links or files under the minimum. A match in the first and last 64 KB that differs in the middle isn't a group.
  - Engine refusals, one test per rule: commit without a token, item not in the token, last copy, changed since review, protected path, linked PC's file, Program Files without the elevated path, path outside the allowed places in an elevated job, a live index file in an elevated job.
  - Claim rules: each signal claims a folder, whole-word matching doesn't over-match, the always-claimed table.
  - Confidence: each piece of evidence moves the score with its reason, the level boundaries, names of 4 letters or fewer never match, similar-named apps are penalized.
  - Uninstall command parsing, including quoted paths and MSI. Install folder derivation, including each rejected location.
  - Freed-space figure with locked copies.
- **Real recycling, `#[ignore]`, run by hand:** recycle a temp file, confirm the bin location, put it back. Nothing in the normal test run touches the user's Recycle Bin.
- **UI (`pnpm test`, mock API):** nothing starts ticked at any confidence level, the last copy can't be ticked, the action button isn't focused, the sheet total follows unticking, every tab reaches removal only through the sheet.
- **Real window, by the user:** a USB stick shows locked with its reason; a Program Files leftover shows one admin prompt; a small real app uninstalls, with its leftovers listed; *Put back* restores a file to its folder; the beam looks right.

Done means `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` and `pnpm test` in `app/` pass, and the real-window list above works.

## Build order

1. Engine spike and safety rules, in `seek-clean` with tests.
2. Selectors and beam. Small, and visible straight away.
3. UI against the mock API in `pnpm dev`, for a design review before anything is wired to real files.
4. Duplicates: core, service request, window scan.
5. Leftovers, then Apps.
6. Recently removed, Put back, *Always protect*, Seek's own uninstaller.
7. Final UI review in the real window.

## Left to the builder

Exact grid columns and spacing within the existing tokens; file layout inside `seek-clean`; how the app's icons are extracted, with a generic icon as fallback; the wording of empty states and errors, keeping to plain sentences; the event channel for scan progress, which has no precedent in the app apart from `seek://event`.
