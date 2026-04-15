# Thumbrella

Media inspection and thumbnailing as a service. The intended product is both structured file information and an opinionated thumbnail for images, video, documents, 3D assets, and arbitrary files.

## Quickstart

```bash
cargo run -p thumbrella-server
```

Requires Rust (stable) and ffmpeg system libraries for full media support.
For dev container usage, reopen in container to get the full native dependency stack.

## Development CLI

Quick local pipeline test for files on disk:

```bash
# writes ./<input-stem>.thumb.jpg
cargo run -p thumbrella-server --bin thumbrella-dev -- <input-path>

# explicit output path
cargo run -p thumbrella-server --bin thumbrella-dev -- <input-path> <output-path>
```

The CLI prints computed metadata and render details as JSON and writes the generated JPEG thumbnail.

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
