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
npx thumbrella/server
uvx thumbrella/server
```

The server is configured through environment variables, like `TBR_PORT=3114`
and `TBR_LOG=full`. This simple server doesn't configure a persistent cache, 
which is an important feature for any production release. 

Clients need a connection string to define the server (and potential authentication).
For this simple server the url is the only value needed. All clients read from
the environment variable, `TBR_CONNECT=http://localhost:3114`

The server has clean and helpful output that should help further onboarding
links and suggestions.

## Build

Thumbrella provides tools to build a bundled static FFmpeg, or use an external
build.  The build scripts write `.cargo/ffs.toml` (gitignored) with the
install paths; `cargo build` picks them up automatically — no environment
variables needed.

### Linux / macOS

```bash
# 1. Install prerequisites (one-time)
#    - Rust: https://rustup.rs
#    - Build tools: gcc, make, curl, pkg-config
#      (Ubuntu/Debian: apt install build-essential curl pkg-config)

# 2. Build FFmpeg and the server
git clone https://github.com/thumbrella-dev/thumbrella
cd thumbrella
bash ffs/build-linux.sh                   # ~10 min, one-time
cargo build --release -p tier3
```

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

The server is partitioned into three individual crates. Most users will simply
use the highest level (tier3) crate as the project. But it is also possible to
create a standalone server based on the lower level "tier1" and "tier2" 
crates with reduced functionality.

- `tier1/` lowest level of the project which defines most of the common data
  structures and most simple format handling. This level of the project is able
  to build and run with wasm.
- `tier2/` adds support for formats with additional static dependencies. The
  tier2 server builds a completely staatic and standalone executable. The most
  notable dependency is a static, minimal  `ffmpeg` built with no external dependencies.
- `tier3/` is the fully functional server. It uses optional external applications
  and libraries, discovered at startup time. The server will work without these
  optional dependencies, enabling support for whatever formats it can discover.
- `docker/` builds an easy to maintain and share docker image based on the
  tier3 binary and a prebuilt media docker image. This does not enable full
  support for all Thumbrella formats, but makes an easy to maintain starting
  point for anyone needing a mostly-featured server.

## Cloud

Thumbrella Cloud makes a fully featured Thumbrella server available for
developers to use for free. Register for a free account at
[thumbrella.dev](https://thumbrella.dev/account) with no payment info or
subscriptions.

Even self-hosted users can fall back on Thumbrella Cloud to add support for
complicated file formats and a globally distributed cache for your
application's users.
