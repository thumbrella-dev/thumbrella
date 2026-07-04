# Download static FFmpeg for Windows (MSVC-compatible).
#
# Uses BtbN/FFmpeg-Builds prebuilt archives.
# https://github.com/BtbN/FFmpeg-Builds
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File download_ffmpeg_windows.ps1
#
# Sets FFMPEG_DIR env var for the current session.  For permanent use,
# add it to your system environment or .cargo/config.toml.

param(
    [string]$InstallDir = "C:\ffmpeg-static",
    [string]$Version = "7.1"
)

$ErrorActionPreference = "Stop"

$url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n${Version}-latest-win64-lgpl-${Version}.zip"
$zip = "$env:TEMP\ffmpeg-static.zip"

Write-Host "[ffmpeg-static] Downloading FFmpeg ${Version} for Windows..." -ForegroundColor Cyan
Invoke-WebRequest -Uri $url -OutFile $zip

Write-Host "[ffmpeg-static] Extracting to ${InstallDir}..." -ForegroundColor Cyan
if (Test-Path $InstallDir) {
    Remove-Item -Recurse -Force $InstallDir
}
Expand-Archive -Path $zip -DestinationPath $env:TEMP\ffmpeg-extract

# BtbN layout: ffmpeg-n7.1-latest-win64-lgpl-7.1/
#   bin/  include/  lib/  share/
$extracted = Get-ChildItem "$env:TEMP\ffmpeg-extract" | Select-Object -First 1
Move-Item $extracted.FullName $InstallDir

Remove-Item $zip
Remove-Item -Recurse -Force "$env:TEMP\ffmpeg-extract" -ErrorAction SilentlyContinue

# Verify key .lib files exist.
$libs = @("avcodec.lib", "avformat.lib", "avutil.lib", "swscale.lib", "swresample.lib")
foreach ($lib in $libs) {
    $path = Join-Path $InstallDir "lib\$lib"
    if (-not (Test-Path $path)) {
        Write-Error "Missing: $path"
        exit 1
    }
}

Write-Host "[ffmpeg-static] Done. FFmpeg ${Version} installed to ${InstallDir}" -ForegroundColor Green
Write-Host ""
Write-Host "Set the environment variable before building tier2:" -ForegroundColor Yellow
Write-Host "  `$env:FFMPEG_DIR = `"${InstallDir}`"" -ForegroundColor White
Write-Host "Or add to .cargo/config.toml:" -ForegroundColor Yellow
Write-Host "  [env]" -ForegroundColor White
Write-Host "  FFMPEG_DIR = `"${InstallDir}`"" -ForegroundColor White
