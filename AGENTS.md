# Seek

Instant file-name search for Windows that remembers offline devices. Rust workspace (`crates/`) + Tauri 2 / React UI (`app/`).

**Start every session by reading `docs/NEXT_STEPS.md`** — current status, how to run, the next task, and the backlog.
Design: `docs/superpowers/specs/2026-10-04-seek-local-design.md`. Plan: `docs/superpowers/plans/2026-10-04-seek-v1.md`.

## Rules
- Never `git commit` (or let subagents commit) unless the user explicitly says so.
- Ask all clarifying questions at once, numbered, with suggested defaults.
- `seek-core` must stay free of Windows dependencies. Timestamps are unix epoch **milliseconds** everywhere.
- After running agents, delete stray empty files they leave in the repo root (from bad shell redirects).

## Commands
- `cargo test --workspace` · `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo build --release -p seek-service -p seek-cli`
- Dev service: `.\target\release\seek-service.exe --console --data-dir "$env:LOCALAPPDATA\Seek-dev"`
- Query it: `.\target\release\seek.exe remote "<query>"`
- Run everything: double-click `Seek.bat` (service + window); `Stop Seek.bat` to stop
- UI: `cd app; pnpm test; pnpm build; pnpm tauri build --no-bundle` (`pnpm dev` = browser preview with mock data)
