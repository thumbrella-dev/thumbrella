# Thumbrella

<img src="thumbrella.png" alt="Thumbrella Logo" width="224" height="224" align="right" />

Fast thumbnail server for online media.

https://thumbrella.dev

Thumbrella is the open source thumbnail server. Get fast, cached thumbnails from
large files and obscure media formats . Many client libraries integrate your
stack with streaming and batching out of the box. Self-host or use a public
service for free. 

This project represents the Thumbrella server. See the 
[clients package]() for
a set of simple libraries for various languages and environments.

Thumbrella is focused on a clean and simple developer experience. Get started
immediately and grow into features as needed.

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

To build and run from source a Rust build and C++ environment is needed.
This will download and build a static ffmpeg with minimal dependencies. The
server itself is written in Rust.

```bash
cargo run serve
```

Development on the Thumbrella server normally happens inside a 
dev container. 

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

## Service

The online Docker service makes a fully featured Thumbrella server available for
developers to use for free. Register for a free account at
[thumbrella.dev](http://thumbrella.dev/account) with no payment info or
subscriptions.

Even self hosted users can fallback on the online service to add support for
complicated file formats and a globally distributed cache for your
application's users.
