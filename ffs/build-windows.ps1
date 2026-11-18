# build-windows.ps1, Builds a static FFmpeg via vcpkg for thumbrella.
# Run from the project root:  powershell -File ffs\build-windows.ps1
#
# This clones vcpkg into ffs/vcpkg/, bootstraps it, and installs a minimal
# static FFmpeg with only the decoders/demuxers/parsers thumbrella needs.
# Software AV1/AVIF decode comes from the dav1d vcpkg port.
# On success, writes VCPKG_ROOT to .cargo/ffs.toml so cargo picks it up.
#
# Safe to re-run: the vcpkg working tree is reset to the pinned commit before
# the port patches are applied, so editing ffs/ports/ffmpeg/*.patch does not
# leave the tree in a state where patching fails.

param(
    # Delete cached ffmpeg build output so vcpkg rebuilds it from scratch.
    [switch]$Clean
)
$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Split-Path -Parent $ScriptDir
Set-Location $ProjectRoot

$vcpkgDir = Join-Path $ScriptDir "vcpkg"
$vcpkgCommit = "e456309491fd875487bf02b8a0dca76c1b1b00cc" # FFmpeg 9.0.2, port revision 1

# ---- Clone vcpkg if not already present ----
if (-not (Test-Path $vcpkgDir)) {
    Write-Host "==> Cloning vcpkg into $vcpkgDir ..."
    git init $vcpkgDir
    if ($LASTEXITCODE -ne 0) { throw "Could not initialize vcpkg" }
    git -C $vcpkgDir remote add origin https://github.com/Microsoft/vcpkg.git
    if ($LASTEXITCODE -ne 0) { throw "Could not configure vcpkg remote" }
    git -C $vcpkgDir fetch --depth 1 origin $vcpkgCommit
    if ($LASTEXITCODE -ne 0) { throw "Could not fetch pinned vcpkg commit" }
    git -C $vcpkgDir checkout --detach $vcpkgCommit
    if ($LASTEXITCODE -ne 0) { throw "Could not check out pinned vcpkg commit" }
}

$head = git -C $vcpkgDir rev-parse HEAD
if ($LASTEXITCODE -ne 0 -or $head -ne $vcpkgCommit) {
    throw "Existing ffs/vcpkg is not the FFmpeg 9.0.2 baseline. Move it aside or remove it, then rerun this script."
}
$port = Get-Content (Join-Path $vcpkgDir "ports/ffmpeg/vcpkg.json") -Raw | ConvertFrom-Json
if ($port.version -ne "9.0.2") { throw "Pinned vcpkg port must provide FFmpeg 9.0.2" }

# ---- Restore the port tree to the pinned baseline ----
# The patches below are applied to a git working tree, so every run has to
# start from the pristine commit. A previous run, or a previous version of
# these patches (for example before libdav1d was added), leaves the tree
# modified, and then `git apply` fails with "patch does not apply" while the
# reverse check also fails because the tree matches an older patch.
Write-Host "==> Restoring tracked files to $vcpkgCommit ..."
git -C $vcpkgDir checkout -- .
if ($LASTEXITCODE -ne 0) { throw "Could not reset the vcpkg working tree" }

# ---- Bootstrap vcpkg ----
Write-Host "==> Bootstrapping vcpkg ..."
Push-Location $vcpkgDir
try {
    .\bootstrap-vcpkg.bat
    if ($LASTEXITCODE -ne 0) { throw "vcpkg bootstrap failed" }
} finally {
    Pop-Location
}

# ---- Apply our patches to the vcpkg port ----
Write-Host "==> Applying port patches ..."
foreach ($patch in Get-ChildItem (Join-Path $ScriptDir "ports/ffmpeg/*.patch")) {
    Write-Host "  Applying $($patch.Name)"
    git -C $vcpkgDir apply --check $patch.FullName
    if ($LASTEXITCODE -ne 0) {
        throw "Port patch $($patch.Name) does not apply to the pinned vcpkg tree. Regenerate it against $vcpkgCommit."
    }
    git -C $vcpkgDir apply $patch.FullName
    if ($LASTEXITCODE -ne 0) { throw "Could not apply $($patch.Name)" }
}

# ---- Optional clean rebuild ----
# The portfile options are part of vcpkg's ABI hash, so a changed
# portfile.patch already forces a ffmpeg rebuild on its own. -Clean is only
# needed to discard a build tree that was interrupted or otherwise wedged.
if ($Clean) {
    Write-Host "==> -Clean: removing cached ffmpeg build output ..."
    Remove-Item -Recurse -Force (Join-Path $vcpkgDir "buildtrees/ffmpeg") -ErrorAction SilentlyContinue
    Get-ChildItem (Join-Path $vcpkgDir "packages") -Filter "ffmpeg*" -ErrorAction SilentlyContinue |
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -Force (Join-Path $vcpkgDir "packages/vcpkg-abi-sha512.txt") -ErrorAction SilentlyContinue
}

# ---- Install ffmpeg (static, overlay port: selective build, no hw accel) ----
# --recurse is required whenever the port patches change: vcpkg otherwise
# reports the existing ffmpeg install as already up to date and refuses to
# rebuild it unless you pass --recurse.
Write-Host "==> Installing ffmpeg via vcpkg (~10-15 min, builds from source) ..."
$vcpkgExe = Join-Path $vcpkgDir "vcpkg.exe"
& $vcpkgExe install ffmpeg[avcodec,avdevice,avfilter,avformat,swresample,swscale,zlib,bzip2,lzma,dav1d] `
    --recurse `
    --overlay-triplets="$ScriptDir/triplets" `
    --triplet=x64-windows-static
if ($LASTEXITCODE -ne 0) { throw "vcpkg FFmpeg install failed" }

# ---- Verify the static libdav1d landed ----
$installedLib = Join-Path $vcpkgDir "installed/x64-windows-static/lib"
$dav1dLib = Get-ChildItem $installedLib -Filter "*dav1d*" -ErrorAction SilentlyContinue
if (-not $dav1dLib) {
    throw "FFmpeg installed but no dav1d library was found in $installedLib"
}
Write-Host "==> libdav1d: $($dav1dLib[0].Name)"

Write-Host ""
Write-Host "============================================"
Write-Host "FFmpeg built successfully!"
Write-Host "Location: $vcpkgDir"
Write-Host ""

# Write .cargo/ffs.toml so cargo finds vcpkg FFmpeg without env vars.
#
# This file is also where extra link args for this particular FFmpeg layout
# belong.  libavcodec.lib references dav1d_* symbols, but dav1d is a separate
# static archive next to the libav ones, so the linker needs both its search
# directory and the archive name.  .cargo/config.toml includes this file and
# cargo merges rustflags from an included config, so these are added to the
# existing target flags (crt-static) rather than replacing them.
$cargoDir = Join-Path $ProjectRoot ".cargo"
if (-not (Test-Path $cargoDir)) { New-Item -ItemType Directory -Path $cargoDir | Out-Null }
$ffsToml = Join-Path $cargoDir "ffs.toml"
$absVcpkg = (Resolve-Path $vcpkgDir).Path -replace '\\', '/'
$absLib = "$absVcpkg/installed/x64-windows-static/lib"
@"
# Generated by ffs/build-windows.ps1, do not edit
[env]
VCPKG_ROOT = { value = "$absVcpkg", force = true }

[target.x86_64-pc-windows-msvc]
rustflags = [
    "-C", "link-arg=/LIBPATH:$absLib",
    "-C", "link-arg=dav1d.lib",
]
"@ | Set-Content -Path $ffsToml

Write-Host "Wrote .cargo/ffs.toml (VCPKG_ROOT=$absVcpkg, links dav1d.lib from $absLib)"
Write-Host "Now run:  cargo build --release -p tier3"
Write-Host "============================================"
