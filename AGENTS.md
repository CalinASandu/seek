# Seek

Instant file-name search for Windows that remembers offline devices. Rust workspace (`crates/`) + Tauri 2 / React UI (`app/`).

**Start every session by reading `docs/NEXT_STEPS.md`** — current status, how to run, the next task, and the backlog.
Design: `docs/superpowers/specs/2026-10-04-seek-local-design.md`. Plan: `docs/superpowers/plans/2026-10-04-seek-v1.md`.

## Rules
- Never `git commit` (or let subagents commit) unless the user explicitly says so.
- Ask all clarifying questions at once, numbered, with suggested defaults, and only when the answer changes what gets built.

## Keep it simple
- Pick the smallest change that meets the goal. If 200 lines can be 50, write 50.
- Build only what was asked. No extra layers, options or "for later" code.
- Logic used more than once becomes one function. Keep code readable and organized.
- Plans fit the task: a small change gets a few steps, not a multi-task plan.
- Before sending a plan or reply, ask: can this be half as long? If yes, cut it.
- If a spec asks for something costly (background work, a new protocol, new service code), propose the cheaper way first.
- `seek-core` must stay free of Windows dependencies. Timestamps are unix epoch **milliseconds** everywhere.
- After running agents, delete stray empty files they leave in the repo root (from bad shell redirects).
- x64 only (seek-clean refuses to build for 32-bit).

## Commands
- `cargo test --workspace` · `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo build --release -p seek-service -p seek-cli`
- Dev service: `.\target\release\seek-service.exe --console --data-dir "$env:LOCALAPPDATA\Seek-dev"`
- Query it: `.\target\release\seek.exe remote "<query>"`
- Run everything: double-click `Seek.bat` (service + window); `Stop Seek.bat` to stop
- Rebuild and run the latest source on this PC: double-click `Rebuild Seek.bat`
- UI: `cd app; pnpm test; pnpm build; pnpm tauri build --no-bundle` (`pnpm dev` = browser preview with mock data)
- Perf (every metric, compares with last run): `.\scripts\perf.ps1` or `.\target\release\seek.exe perf --scan D:\ --remote`
- Installer: `.\scripts\build-installer.ps1` → `target\release\bundle\nsis\Seek_<ver>_x64-setup.exe`
