# Builds Seek Hub: static musl seek-hub for arm64 + amd64 (cross-compiled with cargo-zigbuild) into
# docker\dist\{arm64,amd64}\seek-hub. Docker is NOT needed for that; it is only needed with -Load or -Push.
# Nothing is pushed unless you pass -Push.
#   .\scripts\build-hub-image.ps1                 build both binaries, then print how to build the image on the Pi
#   .\scripts\build-hub-image.ps1 -Load arm64     (needs Docker here) also write docker\seek-hub-arm64.tar for the Pi
#   .\scripts\build-hub-image.ps1 -Load amd64     (needs Docker here) load an amd64 image (docker run --rm seek-hub:dev status)
#   .\scripts\build-hub-image.ps1 -Push           (needs Docker here) push ghcr.io/calinasandu/seek-hub:<version> (your call)
# After installing zig with winget, refresh PATH in this window first:
#   $env:Path = [Environment]::GetEnvironmentVariable("Path","Machine") + ";" + [Environment]::GetEnvironmentVariable("Path","User")
param(
    [ValidateSet("", "arm64", "amd64")][string]$Load = "",
    [switch]$Push,
    [string]$Version = "0.1.0",
    [string]$Repo = "ghcr.io/calinasandu/seek-hub"
)
$ErrorActionPreference = "Continue"
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    # --- tools -------------------------------------------------------------------------------------------------
    $needDocker = ($Load -ne "") -or $Push
    $missing = @()
    if (-not (Get-Command zig -ErrorAction SilentlyContinue)) { $missing += "zig             winget install zig.zig   (or https://ziglang.org/download/; then refresh PATH, see the header)" }
    if (-not (Get-Command cargo-zigbuild -ErrorAction SilentlyContinue)) { $missing += "cargo-zigbuild  cargo install --locked cargo-zigbuild" }
    if ($needDocker -and -not (Get-Command docker -ErrorAction SilentlyContinue)) { $missing += "docker          needed for -Load / -Push (needs buildx); or leave them out and build the image on the Pi" }
    $targets = @{ arm64 = "aarch64-unknown-linux-musl"; amd64 = "x86_64-unknown-linux-musl" }
    $have = (rustup target list --installed) -join "`n"
    foreach ($t in $targets.Values) {
        if ($have -notmatch [regex]::Escape($t)) { $missing += "rust target      rustup target add $t" }
    }
    if ($missing.Count -gt 0) {
        Write-Host "Missing tools:" -ForegroundColor Red
        $missing | ForEach-Object { Write-Host "  $_" }
        throw "install the tools above and run again"
    }

    # --- supply chain checks (warn only when the tools are absent) ---------------------------------------------
    if (Get-Command cargo-deny -ErrorAction SilentlyContinue) {
        cargo deny check
        if ($LASTEXITCODE -ne 0) { throw "cargo deny check failed" }
    } else { Write-Warning "cargo-deny not installed (cargo install --locked cargo-deny): licences/advisories not checked" }
    if (Get-Command cargo-audit -ErrorAction SilentlyContinue) {
        cargo audit
        if ($LASTEXITCODE -ne 0) { throw "cargo audit found advisories" }
    } else { Write-Warning "cargo-audit not installed (cargo install --locked cargo-audit): advisories not checked" }

    # --- binaries ----------------------------------------------------------------------------------------------
    # Shipped builds get the fully optimised profile; the hub is small, so this is quick.
    $env:CARGO_PROFILE_RELEASE_LTO = "thin"
    $env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = "1"
    $env:CARGO_PROFILE_RELEASE_INCREMENTAL = "false"
    $env:CARGO_PROFILE_RELEASE_STRIP = "symbols"
    foreach ($arch in "arm64", "amd64") {
        $triple = $targets[$arch]
        cargo zigbuild --release -p seek-hub --target $triple
        if ($LASTEXITCODE -ne 0) { throw "build for $triple failed" }
        $out = Join-Path $root "docker\dist\$arch"
        New-Item -ItemType Directory -Force $out | Out-Null
        Copy-Item "target\$triple\release\seek-hub" (Join-Path $out "seek-hub") -Force -ErrorAction Stop
        "{0}: {1:N1} MB  ->  {2}" -f $arch, ((Get-Item (Join-Path $out "seek-hub")).Length / 1MB), (Join-Path $out "seek-hub")
    }

    # --- image -------------------------------------------------------------------------------------------------
    $tags = @("-t", "${Repo}:$Version", "-t", "seek-hub:dev")
    if ($Load -ne "") {
        # --load takes a single platform. Tagged with the compose file's image name, so `compose up` finds it.
        docker buildx build --platform "linux/$Load" --load @tags -f docker/Dockerfile docker
        if ($LASTEXITCODE -ne 0) { throw "image build failed" }
        if ($Load -eq "arm64") {
            $tar = Join-Path $root "docker\seek-hub-arm64.tar"
            docker save -o $tar "${Repo}:$Version"
            if ($LASTEXITCODE -ne 0) { throw "docker save failed" }
            "image: $tar  (scp it to the Pi, then: docker load -i seek-hub-arm64.tar)"
        }
    } elseif ($Push) {
        docker buildx build --platform linux/arm64,linux/amd64 @tags --push -f docker/Dockerfile docker
        if ($LASTEXITCODE -ne 0) { throw "image build failed" }
    } else {
        "Binaries are in docker\dist (git-ignored). Next, build the image on the Pi:"
        "  scp -r docker pi@<pi>:seek-hub"
        "  ssh pi@<pi>, then:  cd ~/seek-hub && docker build --build-arg TARGETARCH=arm64 -t ${Repo}:$Version ."
        "(the tag must match the image in compose.yaml, or compose will try to pull it)"
    }
} finally {
    Pop-Location
}
