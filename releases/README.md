<img src="thumbrella.png" alt="Thumbrella Logo" width="224" height="224" align="right" />

# [Thumbrella](https://thumbrella.dev)


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

Start the server with `thumbrella serve`. The default output is designed to be a
helpful starting point for operating a Thumbnail server.

Users can also run `thumbrella check` to get quick feedback the server
configuration and defaults.

By default the server listens on port `3114`. This can be controlled by setting
the `TBR_PORT` environment variable.

See the [Server Documentation](https://thumbrella.dev/docs/server/) 
for more commands and configurations.

## Formats

The server executable comes with a significant number of formats built in
statically. This can be examined by running `thumbrella formats`.

Thumbrella optionally uses external applications to handle the more 
complicated file formats. If the commands aren't available those file
formats will get a simple placeholder thumbnail.

## Alternates

The Thumbrella server is available from several sources. Use the most
convenient starting point for your environment and tools. The server
executable is available on Windows and Linux. 
(MacOS still in development)

- Docker `docker run -p 3114:3114 -it --rm thumbrella/server`
- Npx `npx @thumbrella/server serve`

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
curl http://localhost:3114/thumb.jpeg \
  --data-urlencode "url=https://demo.thumbrella.dev/media/math-guide.odt" \
  --output thumb.jpeg
```

The best and easiest functionality comes from using one of the 
[Client Libraries](https://thumbrella.dev/docs/client/) for
[Javascript](https://npmjs.com/thumbrella/client),
[Python](https://pypi.org/thumbrella-client), or
[Rust](https://crates.io/thumbrella-client). .
