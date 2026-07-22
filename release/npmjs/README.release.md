<img src="https://thumbrella.dev/thumbrella.png" alt="Thumbrella Logo" width="224" height="224" align="right" />

# [Thumbrella](https://thumbrella.dev)

Fast thumbnail server for online media.

> Thumbrella is still in prerelease. The server functionality is operational,
> but several production components have yet to appear. Recommended for early
> evaluation only.

`@thumbrella/server` is the npm launcher package for the Thumbrella server
binary.

## Quickstart

Install globally:

```bash
npm install -g @thumbrella/server
thumbrella serve
```

The no-install `npx`/`npm exec` path for scoped launcher packages can be
inconsistent across npm versions. For this prerelease, global install is the
recommended path.

Users can also run `thumbrella check` for quick configuration feedback.

By default the server listens on port `3114`.
Set `TBR_PORT` to choose a different port.

See the [Server Documentation](https://thumbrella.dev/docs/server/) for full
commands and configuration.

## Formats

The server executable includes many formats built in statically. You can view
available support with:

```bash
thumbrella formats
```

Some advanced formats use external applications from your environment.
If those commands are unavailable, Thumbrella returns a placeholder thumbnail.

## Alternates

Thumbrella server is available from multiple channels:

- Docker: `docker run -p 3114:3114 -it --rm thumbrella/server`


Or build from source:

- `git clone https://github.com/thumbrella-dev/thumbrella && cd thumbrella`
- `bash ffs/build-linux.sh` (or `build-windows.ps1`, or set your own `FFMPEG_DIR`)
- `cargo run --release`

### Cloud Server

Thumbrella also provides a [Cloud Server](https://thumbrella.dev/docs/cloud/)
with usable free tiers.

## Clients

Direct HTTP use:

```bash
curl http://localhost:3114/thumb.jpeg?url=https://demo.thumbrella.dev/media/math-guide.odt --output thumb.jpeg
```

See [Client Libraries](https://thumbrella.dev/docs/client/) for:

- [Javascript](https://npmjs.com/thumbrella/client)
- [Python](https://pypi.org/thumbrella-client)
- [Rust](https://crates.io/thumbrella-client)
- [React](https://npmjs.com/thumbrella/react)
- [Astro](https://npmjs.com/thumbrella/astro)
