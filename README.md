# [Thumbrella](https://thumbrella.dev)

<img src="thumbrella.png" alt="Thumbrella Logo" width="224" height="224" align="right" />

Fast thumbnail server for online media.

[Thumbrella](https://thumbrella.dev) is the open source server for online thumbnails.

Serve fast, cached thumbnails from over 100 formats: photographs, video,
documents, even 3D models. Feed it your media libraries and get a thumbnail
back, every time.

One command runs it locally or in Docker. Our [Thumbrella Cloud](https://thumbrella.dev/account)
is efficient enough to offer a genuinely useful free tier.

Start with [client packages](https://thumbrella.dev/docs/client/) for the
languages you already use. [Docs](https://thumbrella.dev/docs/) and
examples get you streaming thumbnails immediately.

## Quickstart

The easiest way to run the server is from one of the prebuilt release packages.
This can be done through package managers like ``npm`` or ``uv``. There is also
a ``docker`` image ready to start.

Use one of these commands to get a server running locally.

```bash
docker run --rm -it --name tbr --publish 3114:3114 thumbrella/server
npx @thumbrella/server
```

The server is configured through environment variables, like `TBR_PORT=3114`
and `TBR_LOG=full`. This simple server doesn't configure a persistent cache,
which is an important feature for any production release.

Clients need a connection string to define the server (and authentication).
For this simple server the URL is the only value needed. All clients read from
the environment variable `TBR_CONNECT=http://localhost:3114`.

The server prints helpful output with onboarding links and suggestions
at startup.

## Build

Thumbrella provides tools to build a bundled static FFmpeg, or use an external
build.  The build scripts write `.cargo/ffs.toml` (gitignored) with the
install paths; `cargo build` picks them up automatically, no environment
variables needed.

### Linux / macOS

```bash
# 1. Install prerequisites (one-time)
#    - Rust >= 1.85: https://rustup.rs
#    - Build tools: gcc, make, curl, pkg-config
#      (Ubuntu/Debian: apt install build-essential curl pkg-config)

# 2. Build FFmpeg and the server
git clone https://github.com/thumbrella-dev/thumbrella
cd thumbrella
bash ffs/build-linux.sh                   # ~10 min, one-time
cargo build --release -p tier3
```

### Release packaging

When you are ready to assemble a GitHub draft release from already-built
Linux and Windows binaries, use:

```bash
scripts/release.sh --tag v1.0.0 --open
```

The script expects both git trees to be clean, exactly on the release tag,
and to already contain `target/release/thumbrella` plus
`target/release/thumbrella.exe`. It uses `release/README.release.md` for
the archive README when present, falling back to the project `README.md`.
If a working tree is slightly dirty during prerelease work, the script
will warn and continue.

- `thumbrella-v1.0.0-linux-x86_64.tar.gz`
- `thumbrella-v1.0.0-windows-x86_64.zip`

Each archive includes the binary, `README.md`, and `LICENSE`.

### Windows

A bundled static FFmpeg is built automatically via vcpkg.  The only
prerequisites are Git, Rust, and MSVC Build Tools.

```powershell
# 1. Install prerequisites (one-time)
winget install Git.Git Rustlang.Rustup Microsoft.VisualStudio.2022.BuildTools `
    --override "--wait --add Microsoft.VisualStudio.Workload.VCTools"
rustup default stable

# 2. Build FFmpeg and the server
git clone https://github.com/thumbrella-dev/thumbrella
cd thumbrella
powershell -File ffs/build-windows.ps1    # ~15 min, one-time
cargo build --release -p tier3
```

## Project Structure

The server is organized into three tiers of increasing capability:

- `tier1/` — Core data structures and basic format handling. Compiles to
  WASM for Cloudflare Workers deployment. Handles cache, routing, and
  light decode work.
- `tier2/` — Adds formats with native dependencies: video keyframes,
  audio waveforms, HDR images, SVG rendering, and camera raw formats.
  Links a minimal static FFmpeg with no external dependencies.
- `tier3/` — The fully functional server. Adds subprocess-based renderers
  for 3D geometry (F3D), USDZ extraction, advanced document formats
  (libreoffice, pdfium), and arithmetic JPEG support (ImageMagick).
  Backends are discovered at startup and compiled-in Python scripts
  handle data sanitization.

The binary output (from any tier) is always named `thumbrella`.
Build with `cargo build -p tier3` for a full-featured server.

## Cloud

Thumbrella Cloud makes a fully featured Thumbrella server available for
developers to use for free. Register for a free account at
[thumbrella.dev](https://thumbrella.dev/account) with no payment info or
subscriptions.

Even self-hosted users can fall back on Thumbrella Cloud to add support for
complicated file formats and a globally distributed cache for your
application's users.
