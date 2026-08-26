<img src="https://thumbrella.dev/thumbrella.png" alt="Thumbrella Logo" width="224" height="224" align="right" />

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

Start a container with Docker and an exposed port. The default output is designed to be a
helpful starting point for operating a thumbnail server.


```bash
docker run --rm --publish 3114:3114 thumbrella/server
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

## Native

The Thumbrella server is also available from several sources. Use the most
convenient starting point for your environment and tools. The server
executable is available on Windows and Linux. This will listen on port `3114`
by default.
(macOS still in development)

- npx `npx @thumbrella/server serve`

Or fetch the Rust source and build your own server.

### Cloud Server

Thumbrella also provides a [Cloud Server](https://thumbrella.dev/docs/cloud/)
with the full featured functionality and usable free tiers. Quick signup with no
payment info required.

## Clients

The server can easily be used with direct http calls.

```bash
curl -OG 
  http://localhost:3114/thumb.jpeg \
  --data-urlencode \
  url=https://demo.thumbrella.dev/media/math-guide.odt
```

The best and easiest functionality comes from using one of the 
[Client Libraries](https://thumbrella.dev/docs/client/) for
[Javascript](https://npmjs.com/thumbrella/client),
[Python](https://pypi.org/thumbrella-client), or
[Rust](https://crates.io/thumbrella-client). 
