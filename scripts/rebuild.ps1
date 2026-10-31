# Rebuilds Seek on this PC from the current source and runs the new version: closes the window, stops the service
# (one admin prompt), builds service, CLI and app, then starts the service and reopens the window.
# Used by "Rebuild Seek.bat" in the repo root. If the build fails, the service is started again anyway.
# Native tools (cargo, pnpm) write progress to stderr; failures are detected via $LASTEXITCODE.
$ErrorActionPreference = 'Continue'
$root    = Split-Path -Parent $PSScriptRoot
$release = Join-Path $root 'target\release'

& (Join-Path $PSScriptRoot 'stop.ps1')

# Running programs lock their .exe, so the service has to stop before the build. Only that needs admin, not the build:
# one elevated helper stops it and starts it again once the flag file is gone (or after an hour, if this window closes).
$service = Get-Service -Name SeekService -ErrorAction SilentlyContinue
$flag = Join-Path $env:TEMP 'seek-rebuild.flag'
if ($service) {
    New-Item -ItemType File -Force $flag | Out-Null
    $helper = "sc.exe stop SeekService | Out-Null; `$end = (Get-Date).AddHours(1); " +
              "while ((Test-Path '$flag') -and (Get-Date) -lt `$end) { Start-Sleep 1 }; sc.exe start SeekService | Out-Null"
    try {
        Write-Host 'Stopping the Seek service (admin prompt)...'
        Start-Process powershell.exe -Verb RunAs -WindowStyle Hidden -ErrorAction Stop `
            -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-Command', $helper
        $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(60))
        Wait-Process seek-service -Timeout 15 -ErrorAction SilentlyContinue
    } catch {
        Remove-Item $flag -ErrorAction SilentlyContinue
        Write-Host "Could not stop the Seek service, nothing was built: $($_.Exception.Message)"
        exit 1
    }
}

$failed = $null
Push-Location $root
try {
    cargo build --release -p seek-service -p seek-cli
    if ($LASTEXITCODE -ne 0) { $failed = 'service build failed' }
    if (-not $failed) {
        Push-Location app
        try {
            pnpm install
            if ($LASTEXITCODE -ne 0) { $failed = 'pnpm install failed' }
            if (-not $failed) {
                pnpm tauri build --no-bundle
                if ($LASTEXITCODE -ne 0) { $failed = 'app build failed' }
            }
        } finally {
            Pop-Location
        }
    }
} finally {
    Pop-Location
    Remove-Item $flag -ErrorAction SilentlyContinue
}

if ($service) {
    try { $service.WaitForStatus('Running', [TimeSpan]::FromSeconds(60)) }
    catch { Write-Host 'The Seek service did not start within a minute. Check it in services.msc.'; exit 1 }
}

if ($failed) {
    Write-Host "$failed. The Seek service is running again; the window was not reopened."
    exit 1
}

$env:SEEK_SERVICE_PATH = Join-Path $release 'seek-service.exe'
Start-Process (Join-Path $release 'seek-app.exe')
Write-Host 'Seek rebuilt and running.'
