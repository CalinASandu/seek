# Measures every Seek performance metric and compares with the previous run.
#
#   .\scripts\perf.ps1                      # synthetic 10M entries + D:\ scan + service round trips
#   .\scripts\perf.ps1 -Entries 2000000 -Scan C:\,D:\
#
# Results are saved to perf-results\<timestamp>.json; the newest earlier file is used as the baseline.
param(
    [int]$Entries = 10000000,
    [int]$Runs = 5,
    [string[]]$Scan = @("D:\"),
    [switch]$NoRemote
)
# Native tools (cargo, pnpm) write progress to stderr; failures are detected via $LASTEXITCODE.
$ErrorActionPreference = "Continue"
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    cargo build --release -p seek-cli
    if ($LASTEXITCODE -ne 0) { throw "build failed" }
    $dir = Join-Path $root "perf-results"
    New-Item -ItemType Directory -Force $dir | Out-Null
    $baseline = Get-ChildItem $dir -Filter *.json | Sort-Object LastWriteTime | Select-Object -Last 1
    $out = Join-Path $dir ("{0:yyyy-MM-dd_HH-mm-ss}.json" -f (Get-Date))
    $cliArgs = @("perf", "--entries", $Entries, "--runs", $Runs, "--json", $out)
    foreach ($s in $Scan) { $cliArgs += @("--scan", $s) }
    if (-not $NoRemote) { $cliArgs += "--remote" }
    if ($baseline) { $cliArgs += @("--compare", $baseline.FullName) }
    & "$root\target\release\seek.exe" @cliArgs
} finally {
    Pop-Location
}
