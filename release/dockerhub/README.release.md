<img src="https://thumbrella.dev/thumbrella.png" alt="Thumbrella Logo" width="224" height="224" align="right" />

# [Thumbrella](https://thumbrella.dev)


Fast thumbnail server for online media.

> Thumbrella is still in prerelease. The server functionality is operational,
> but several production components have yet to appear. Recommended for early
> evaluation only.

[Thumbrella](https://thumbrella.dev) is the open source server for online thumbnails.

Serve fast, cached thumbnails from over 100 formats: photographs, video,
documents, even 3D models. Feed it your media libraries and get a thumbnail
back, every time.

## Quickstart

Start a container with docker and an exposed port. The default output is designed to be a
helpful starting point for operating a Thumbnail server.


```bash
docker run --rm -it -p 3114:3114 thumbrella/server
```

See the [Server Documentation](https://thumbrella.dev/docs/server/) 
for more commands and configurations.

## Formats

The server executable comes with a significant number of formats built in
statically. This image is built on `lscr.io/linuxserver/ffmpeg:latest`
which adds an abundant number of formats to the builtin formats
this server already provides.

More advanced formats will still need additional applications like
`f3d`, which aren't included in this straightforward container.

For any commands not available those formats will use a simple placeholder
thumbnail.

## Alternates

The Thumbrella server also is available from several sources. Use the most
convenient starting point for your environment and tools. The server
executable is available on Windows and Linux. 
(MacOS still in development)

- Npx `npx thumbrella/server serve`
- Uvs `uvx thumbrella-server serve`

Or fetch the Rust source and build your own server.
- `git clone https://github.com/thumbrella-dev/thumbrella && cd thumbrella`
- `bash ffs/build-linux.sh`  (or build-windows.ps1, or set your own `FFMPEG_DIR`)
- `cargo run --release`

### Cloud Server

Thumbrella also provides a [Cloud Server](https://thumbrella.dev/docs/cloud/)
with the full featured functionality and usable free tiers. Quick signup with no
payment info required.

## Clients

The server can easily be used with direct http calls.

```bash
curl http://localhost:3114/thumb.jpeg?url=https://demo.thumbrella.dev/media/math-guide.odt --output thumb.jpeg
```

The best and easiest functionality comes from using one of the 
[Client Libraries](https://thumbrella.dev/docs/client/) for
[Javascript](https://npmjs.com/thumbrella/client),
[Python](https://pypi.org/thumbrella-client), or
[Rust](https://crates.io/thumbrella-client). 
There are also higher level component libraries for
[React](https://npmjs.com/thumbrella/react) and
[Astro](https://npmjs.com/thumbrella/astro).
