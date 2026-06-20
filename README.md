# Thumbrella

Fast thumbnail server for online media.

https://thumbrella.dev

Thumbrella is the open source thumbnail server. Get fast, cached thumbnails from
large files and obscure media formats . Many client libraries integrate your
stack with streaming and batching out of the box. Self-host or use a public
service for free. 

This project represents the Thumbrella server. See the 
[clients package]() for
a set of simple libraries for various languages and environments.

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
