# [Thumbrella](https://thumbrella.dev)

<img src="thumbrella.png" alt="Thumbrella Logo" width="224" height="224" align="right" />

[Thumbrella](https://thumbrella.dev) brings fast, beautiful thumbnails to any
online gallery. Supporting 100+ formats: photos, video, documents, 3D
models, and other media.

Run the server with one command and zero config. The open-source server
comes with all the features and functionality. Then connect with 
[client packages](https://thumbrella.dev/docs/client/) for the browser or any 
of the other supported languages. 

Also check out the [Thumbrella Cloud](https://thumbrella.dev/docs/cloud/) for a
distributed server and caching system. This genuine free tier is built for real
every day projects; connected in two clicks.

## Quickstart

The easiest way to run the server is from one of the prebuilt release packages.
This can be done through package managers like ``npm`` or ``uv``. There is also
a ``docker`` image ready to start.

Use one of these commands to get a server running locally.

```bash
docker run --rm -it --name tbr --publish 3114:3114 thumbrella/server
npx @thumbrella/server serve
```

The server is configured through environment variables, like `TBR_PORT=3114`
and `TBR_LOG=full`. This simple server doesn't configure a persistent cache,
which is an important feature for any production release.

Clients need a connection string to define the server (and authentication).
For this simple server the URL is the only value needed. All clients read from
the environment variable `TBR_CONNECT=http://localhost:3114`.

The server prints helpful output with onboarding links and suggestions
at startup.

## Connect an application

Once the server is running, connect it to an application with one of the
[Thumbrella client packages](https://thumbrella.dev/docs/client/). For browser
galleries and file browsers, start with the `<tbr-thumb>` web component:

```html
<script type="module">
  import { tbrSetup } from "https://js.thumbrella.dev/1.4/tbr.js";
  tbrSetup("http://localhost:3114");
</script>

<tbr-thumb src="https://example.com/media/photo.heic"></tbr-thumb>
```

For applications that need direct control over requests, caching, batching, or
streaming, use the [JavaScript client on npm](https://www.npmjs.com/package/@thumbrella/client).
The client also provides the browser component for bundled applications.

- [Web component documentation](https://thumbrella.dev/docs/components/)
- [Client library documentation](https://thumbrella.dev/docs/client/)
- [JavaScript client package](https://www.npmjs.com/package/@thumbrella/client)
- [Client packages and examples repository](https://github.com/thumbrella-dev/clients)
- [HTTP API documentation](https://thumbrella.dev/docs/http-api/) for lower-level integrations

The runnable client examples live in the separate
[clients repository](https://github.com/thumbrella-dev/clients), including
[browser examples](https://github.com/thumbrella-dev/clients/tree/main/typescript/examples),
[Python examples](https://github.com/thumbrella-dev/clients/tree/main/python/examples),
and [Rust examples](https://github.com/thumbrella-dev/clients/tree/main/rust/examples).

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

### Pinned thumbnails

Native servers publish a nullable top-level `pin` field in thumbnail results:

```json
{ "pin": "pin/i123456789012.jpeg" }
```

Resolve this relative URL against the server base URL. `GET /pin/<id>.jpeg`
returns only JPEG bytes, never result or media metadata. A missing, expired,
malformed, or disabled pin redirects to its kind's placeholder. Pin responses
are not HTTP-cacheable, so their retention and updated content remain
server-controlled. Existing `TBR_HANDSHAKE` protection also applies to pins.

`TBR_PIN` is a sliding TTL in seconds; `0` disables pinning. By default it
uses `TBR_CACHE_MAX_TTL` (seven days). Ordinary thumbnail requests refresh
pin retention, including backend cache hits and client freshness checks when
the server has a matching entry. Debounce hits reuse their pin without backend
reads or retention updates. Pin fetches do not refresh it. Pin retention
can outlive ordinary cache retention and keeps the shared entry available.
No cache backend (`TBR_CACHE=none`) means no pin URLs.
`check` reports the effective `TBR_PIN` TTL in seconds, whether it is the
default, and whether pinning is enabled or disabled. Unset or blank values use
the maximum cache TTL. Explicit values must be whole seconds from 0 through
4294967295; `0` disables pinning. Invalid values fail `check` and startup.

Pins follow the latest thumbnail for a source URL. Identifiers use a kind
letter followed by the first 12 URL-safe base64 characters of an HMAC-SHA-256
over the cache identity, media kind, cache format version, and collision counter.
Each local backend owns a secret of 32 bytes from OS cryptographic randomness,
so knowing a source URL does not reveal its pin. Collisions
are checked atomically by the backend and retried with a new hash input.
The kind letters are `i` image, `v` video, `a` audio, `s` vector, `d` document,
`g` geometry, `r` archive, `t` text, `b` binary, and `u` unknown.

Memory aliases disappear on restart or eviction; SQLite aliases persist with
the cached entry. Memory backends generate a fresh secret at construction;
SQLite stores its secret in `thumbrella_metadata`, initializes it atomically
when absent, and reuses it across opens and cache eviction. Invalid stored
secrets fail setup rather than silently rotating the pin identity.
In cache chains the final (coldest) backend owns both pin derivation and the
alias index, and the other layers share its refreshed pin lifetime. Pins are
best-effort under cache capacity limits, not permanent storage. Missing-handler
placeholders are not pinned or stored durably; their client cache tokens and
five-second debounce remain unchanged.

The native `cloud:` backend requests remote pin issuance rather than receiving
a cloud secret or deriving pins locally. Its new issue/resolve endpoints
require the separate cloud pin implementation, which owns the secret and
account/token namespace. Pins are bearer links, not a replacement for access
control on the thumbnail-generation API.

For v2, local-file and localhost access is enabled with `TBR_LOCAL=1`.
This replaces `TBR_ALLOW_LOCAL`, which is no longer read.

Filesystem configuration paths support `~` and `~/...` for the current user's
home directory, `$VAR` and `${VAR}` for environment variables, and `%VAR%`
on Windows (where `~\...` also works). This applies to `TBR_SCRATCH`,
`TBR_TRACE=ndjson:<path>`, and the path in `TBR_CACHE=sqlite:<path>[,size]`.
Unset variables, malformed `${VAR}` references, and empty expanded paths
produce configuration errors. Use `$$` for a literal dollar sign, or `%%`
for a literal percent sign on Windows. Relative paths remain relative;
directories need not already exist for expansion itself to succeed.

Expansion is single-pass and never invokes a shell. Substituted values are
literal, and cache directives are parsed before expanding the SQLite path.
Cloud tokens, handoff URLs, and media request URLs are not expanded.
Quote values to defer expansion to Thumbrella, for example:

```sh
export TBR_SCRATCH='${DATA_DIR}/thumbrella'
export TBR_TRACE='ndjson:~/trace.ndjson'
export TBR_CACHE='mem:100mb+sqlite:${DATA_DIR}/cache.db,1gb'
```

## Cloud

Thumbrella Cloud makes a fully featured Thumbrella server available for
developers to use for free. Register for a free account at
[thumbrella.dev](https://thumbrella.dev/account) with no payment info or
subscriptions.

Even self-hosted users can fall back on Thumbrella Cloud to add support for
complicated file formats and a globally distributed cache for your
application's users.
