# Builds the Seek installer: app/src-tauri/target-dir .../bundle/nsis/Seek_<version>_x64-setup.exe
# The installer ships seek-service.exe next to the app and registers SeekService (runs as SYSTEM).
# Native tools (cargo, pnpm) write progress to stderr; failures are detected via $LASTEXITCODE.
$ErrorActionPreference = "Continue"
# Shipped builds get the fully optimised profile; everyday builds keep the fast one in Cargo.toml.
$env:CARGO_PROFILE_RELEASE_LTO = "thin"
$env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = "1"
$env:CARGO_PROFILE_RELEASE_INCREMENTAL = "false"
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    cargo build --release -p seek-service
    if ($LASTEXITCODE -ne 0) { throw "service build failed" }
    # Tauri's externalBin expects the target triple in the file name; it installs it as seek-service.exe.
    $bin = Join-Path $root "app\src-tauri\binaries"
    New-Item -ItemType Directory -Force $bin | Out-Null
    Copy-Item "target\release\seek-service.exe" (Join-Path $bin "seek-service-x86_64-pc-windows-msvc.exe") -Force -ErrorAction Stop
    Push-Location app
    try {
        # The installer-only config adds externalBin, so normal builds never need the copied binary.
        pnpm tauri build --config src-tauri/tauri.installer.conf.json
        if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }
    } finally {
        Pop-Location
    }
    Get-ChildItem "target\release\bundle\nsis\*.exe" | ForEach-Object { "installer: $($_.FullName)" }
} finally {
    Pop-Location
}
