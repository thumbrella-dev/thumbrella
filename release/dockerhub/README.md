# Thumbrella Docker Hub packaging

This directory contains the release packaging flow for Docker Hub.

Use `build.sh` to:

- fetch the Linux release archive from GitHub Releases, or use a staged archive
- extract the release binary plus the release README and LICENSE
- build a Docker image tagged as `thumbrella/server:<version>`
- optionally push `:version` and the moving `:prerelease` channel tag
- pass `--no-channel-tag` for odd one-off or backfill releases when you do not want the moving tag updated

Example:

```bash
cd release/dockerhub
./build.sh v0.5.1 --push
```

If you already downloaded the archive manually, set `ARCHIVE_PATH` instead.