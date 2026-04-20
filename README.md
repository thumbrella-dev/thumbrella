# Thumbrella

Media inspection and thumbnailing as a service. The intended product is both structured file information and an opinionated thumbnail for images, video, documents, 3D assets, and arbitrary files.

## Quickstart

```bash
cargo run -p thumbrella-tier1
```

Requires Rust (stable) and ffmpeg system libraries for full media support.
For dev container usage, reopen in container to get the full native dependency stack.

## Development CLI

Quick local pipeline test for files on disk:

```bash
# writes ./<input-stem>.thumb.jpg
cargo run -p thumbrella-tier1 --bin thumbrella-dev -- <input-path>

# explicit output path
cargo run -p thumbrella-tier1 --bin thumbrella-dev -- <input-path> <output-path>
```

The CLI prints computed metadata and render details as JSON and writes the generated JPEG thumbnail.

## Media Server

Quick HTTP server for browsing test media files:

```bash
./mediaserver
```

Starts an HTTP server on `http://localhost:8001` serving the `media/` directory.

## Development Servers

Run tier1 and/or tier2 servers locally for testing. VS Code tasks are configured for easy rebuild and restart:

**Build and run (manual workflow):**
1. Press `Ctrl+Shift+B` (or `Cmd+Shift+B` on Mac) to build
2. Press `Ctrl+Shift+D` (or select "Run Tier1 Server" / "Run Tier2 Server" from the Command Palette)
3. Edit files and repeat

**Terminal commands:**
```bash
# Build and run Tier1
cargo build -p thumbrella-tier1
cargo run -p thumbrella-tier1

# Build and run Tier2 (requires Tier1 route handlers and dispatch registration)
cargo build -p thumbrella-tier2
cargo run -p thumbrella-tier2

# Automatic rebuild on file changes (requires cargo-watch)
cargo watch -x "build -p thumbrella-tier1"
cargo watch -x "run -p thumbrella-tier1"
```

Default ports:
- Tier1: `http://localhost:8000`
- Tier2: `http://localhost:8001`

## API

- `GET /health` — healthcheck
- `POST /describe` — upload a file, get structured file information
- `POST /describe/url?url=...` — fetch a URL and describe it
- `POST /thumbnail` — upload a file, get a thumbnail back
- `POST /thumbnail/url?url=...` — fetch a URL and thumbnail it
- `POST /batch` — mixed inspection and thumbnail orchestration with streaming results

_Simple endpoints and streaming are not yet wired; milestone 1 is in progress._

## Testing

```bash
cargo test
```

## Docker

```bash
docker build -t thumbrella .
docker run -p 8000:8000 thumbrella
```

## Cog (Replicate)

The Cog entry point (`predict.py`) is currently a placeholder. It will be implemented as a thin Python wrapper over the compiled Rust binary once the Tier 2 server is stable.
