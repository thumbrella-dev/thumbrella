# [Thumbrella](https://thumbrella.dev)

<img src="thumbrella.png" alt="Thumbrella Logo" width="224" height="224" align="right" />

[Thumbrella](https://thumbrella.dev) brings fast, beautiful thumbnails to 
online galleries. Supporting 100+ formats: photos, video, documents, 3D
models, and other media.

Run the server for yourself with one command. The open-source, zero-config 
server comes with all the features and functionality. Then connect with 
[client packages](https://thumbrella.dev/docs/client/) for the browser or any 
of the other supported languages. 

Also check out the [Thumbrella Cloud](https://thumbrella.dev/docs/cloud/) for a
distributed server and caching system. This genuine free tier is built for real
every day projects; get connected in two clicks.

## Quickstart

The easiest way to run the server is from one of the prebuilt release packages.
This can be done through package managers like ``npm`` or ``winget``. The
Docker image from Docker Hub is ready to run out of the box.

```bash
npx @thumbrella/server serve
docker run --rm -it --name tbr --publish 3114:3114 thumbrella/server
```

The server is configured through environment variables, like `TBR_PORT=3114`
and `TBR_LOG=full`. Explore the [server documentation](/docs/server/) 
for details on all the features, configurations, and troubleshooting.

## Connect an application

Once a server is running you can connect browsers to it 
[directly from html](https://thumbrella.dev/docs/components/) using the hosted
`tbr.js` with it's custom `<tbr-thumb>` html element.

```html
<script type="module">
  import { tbrSetup } from "https://js.thumbrella.dev/2.0/tbr.js";
  tbrSetup("http://localhost:3114");
</script>

<tbr-thumb src="https://example.com/media/photo.heic"></tbr-thumb>
```

Thumbrella provides [high level client libraries](https://thumbrella.dev/docs/client/)
for **Typescript**, **Python**, and **Rust**. These provide advanced caching, 
streaming, and batched results.

The documentation contains shared information for 
[each of the client libraries](https://thumbrella.dev/docs/client/).
The server can also be comfortably used through 
[direct html](https://thumbrella.dev/docs/http-api/) requests. Each client
library also comes with simple examples to get you started.

* [Javascript examples](https://github.com/thumbrella-dev/clients/tree/main/typescript/examples),
* [Python examples](https://github.com/thumbrella-dev/clients/tree/main/python/examples),
* [Rust examples](https://github.com/thumbrella-dev/clients/tree/main/rust/examples).

## Development

The Rust project is ready to build with a `cargo build --release`.

Much of Thumbrella's functionality comes from a builtin, static subset of
ffmpeg. This needs to be built ahead of time or referenced by environment
variable. The project provides build time instructions and tooling to do
this for Windows and Linux. Read more details in the 
[build instructions](https://thumbrella.dev/docs/server/#build-yourself).

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

The [developer documentation](https://thumbrella.dev/docs/development/)
has more details on the architecture, contribution guidelines, and more.

## Cloud

Thumbrella Cloud makes a fully featured Thumbrella server available for
developers to use for free. Register for a free account at
[thumbrella.dev](https://thumbrella.dev/account) with no payment info or
subscriptions.

Even self-hosted users can fall back on Thumbrella Cloud to add support for
complicated file formats and a globally distributed cache for your
application's users. The [cloud documentation](https://thumbrella.dev/docs/cloud/)
has more details on using and configuring Cloud accounts.
