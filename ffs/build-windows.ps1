# build-windows.ps1 — Builds a static FFmpeg via vcpkg for thumbrella.
# Run from the project root:  powershell -File ffs\build-windows.ps1
#
# This clones vcpkg into ffs/vcpkg/, bootstraps it, and installs a minimal
# static FFmpeg with only the decoders/demuxers/parsers thumbrella needs.
# On success, writes VCPKG_ROOT to .cargo/config.toml so cargo picks it up.

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Split-Path -Parent $ScriptDir
Set-Location $ProjectRoot

$vcpkgDir = Join-Path $ScriptDir "vcpkg"

# ---- Clone vcpkg if not already present ----
if (-not (Test-Path $vcpkgDir)) {
    Write-Host "==> Cloning vcpkg into $vcpkgDir ..."
    git clone --depth 1 https://github.com/Microsoft/vcpkg.git $vcpkgDir
}

# ---- Bootstrap vcpkg ----
Write-Host "==> Bootstrapping vcpkg ..."
Push-Location $vcpkgDir
try {
    .\bootstrap-vcpkg.bat
} finally {
    Pop-Location
}

# ---- Apply our patches to the vcpkg port ----
Write-Host "==> Applying port patches ..."
Push-Location $vcpkgDir
try {
    foreach ($patch in Get-ChildItem (Join-Path $ScriptDir "ports/ffmpeg/*.patch")) {
        Write-Host "  Applying $($patch.Name)"
        git apply $patch.FullName
    }
} finally {
    Pop-Location
}

# ---- Install ffmpeg (static, overlay port: selective build, no hw accel) ----
Write-Host "==> Installing ffmpeg via vcpkg (~10-15 min, builds from source) ..."
$vcpkgExe = Join-Path $vcpkgDir "vcpkg.exe"
& $vcpkgExe install ffmpeg[avcodec,avdevice,avfilter,avformat,swresample,swscale,zlib,bzip2,lzma] `
    --overlay-triplets="$ScriptDir/triplets" `
    --triplet=x64-windows-static

Write-Host ""
Write-Host "============================================"
Write-Host "FFmpeg built successfully!"
Write-Host "Location: $vcpkgDir"
Write-Host ""

# Uncomment and set VCPKG_ROOT in .cargo/config.toml so all build scripts
# (including ffmpeg-sys-next) can find FFmpeg.
$vcpkgRoot = ($vcpkgDir -replace '\\', '/')
$configDir = Join-Path $ProjectRoot ".cargo"
if (-not (Test-Path $configDir)) { New-Item -ItemType Directory -Path $configDir | Out-Null }
$configPath = Join-Path $configDir "config.toml"
$configContent = if (Test-Path $configPath) { Get-Content $configPath -Raw } else { "" }

if ($configContent -match '(?m)^#?\s*VCPKG_ROOT\s*=') {
    # Replace existing line (commented or not) with uncommented value
    $configContent = $configContent -replace '(?m)^#?\s*VCPKG_ROOT\s*=.*', "VCPKG_ROOT = `"$vcpkgRoot`""
} else {
    $configContent = "[env]`nVCPKG_ROOT = `"$vcpkgRoot`"`n`n$configContent"
}
Set-Content -Path $configPath -Value $configContent

Write-Host "VCPKG_ROOT added to .cargo/config.toml"
Write-Host "Override with FFMPEG_DIR for a custom FFmpeg build."
Write-Host "Now run:  cargo build"
Write-Host "============================================"
