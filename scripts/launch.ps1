# Starts the Seek service (if it isn't running) and opens the Seek window.
# Used by Seek.bat in the repo root.
$ErrorActionPreference = 'Stop'
$root    = Split-Path -Parent $PSScriptRoot
$release = Join-Path $root 'target\release'
$service = Join-Path $release 'seek-service.exe'
$app     = Join-Path $release 'seek-app.exe'
$dataDir = Join-Path $env:LOCALAPPDATA 'Seek-dev'

function Test-ServicePipe {
    # Connect to Seek's pipe directly (enumerating all pipes can throw on unusual pipe names).
    $client = New-Object System.IO.Pipes.NamedPipeClientStream('.', 'seek-v1', [System.IO.Pipes.PipeDirection]::InOut)
    try { $client.Connect(300); return $true } catch { return $false } finally { $client.Dispose() }
}

# 1. Build whatever is missing (first run only; takes a few minutes).
if (-not (Test-Path $service)) {
    Write-Host 'Building seek-service (first run)...'
    Push-Location $root; cargo build --release -p seek-service -p seek-cli; Pop-Location
}
if (-not (Test-Path $app)) {
    Write-Host 'Building the Seek app (first run)...'
    Push-Location (Join-Path $root 'app'); pnpm install; pnpm tauri build --no-bundle; Pop-Location
}

# 2. Make sure the service is running.
if (-not (Test-ServicePipe)) {
    $installed = Get-Service -Name SeekService -ErrorAction SilentlyContinue
    try {
        if ($installed) {
            Write-Host 'Starting the installed Seek service (admin prompt)...'
            Start-Process sc.exe -ArgumentList 'start SeekService' -Verb RunAs -WindowStyle Hidden
        } else {
            Write-Host 'Starting the Seek service as administrator (admin prompt) - fast NTFS indexing...'
            Start-Process $service -ArgumentList "--console --data-dir `"$dataDir`"" -Verb RunAs -WindowStyle Minimized
        }
    } catch {
        Write-Host 'Admin prompt declined - starting the service without admin (slower indexing, no live updates).'
        Start-Process $service -ArgumentList "--console --data-dir `"$dataDir`"" -WindowStyle Minimized
    }
    for ($i = 0; $i -lt 40 -and -not (Test-ServicePipe); $i++) { Start-Sleep -Milliseconds 250 }
}

# 3. Open the window (or bring the running one to the front).
$running = Get-Process seek-app -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 }
if ($running) {
    (New-Object -ComObject WScript.Shell).AppActivate($running[0].Id) | Out-Null
} else {
    $env:SEEK_SERVICE_PATH = $service
    Start-Process $app
}
