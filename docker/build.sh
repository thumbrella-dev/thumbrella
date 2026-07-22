#!/bin/bash
# Build a straightforward Thumbrella Docker image.
# By default this creates a local docker image 'local/thumbrella-server:dev'
# This can be pushed to Docker Hub using the version in the relative Cargo.toml
# to `thumbrella/server`, see https://hub.docker.com/r/thumbrella/server
#
# This must be able to find `docker` or `podman` on the path, or set $DOCKER
# This must also be able to find `cargo` and build for linux
#
# Usage:
#   ./build.sh          # tag :latest
#   ./build.sh --push   # tag + push $VERSION to docker hub
#   ./build.sh --latest # tag + push 'latest' to docker hub
#   IMAGE=my.reg/thumbrella ./build.sh

set -euo pipefail


# Detect docker/podman availability
if command -v docker &> /dev/null; then
    DOCKER="${DOCKER:-docker}"
elif command -v podman &> /dev/null; then
    DOCKER="${DOCKER:-podman}"
else
    echo "Error: Set `DOCKER` to the docker (or podman) executable, not found in PATH" >&2
    exit 1
fi

if command -v cargo &> /dev/null; then
    CARGO="${CARGO:-cargo}"
else
    echo "Error: Set `CARGO` to the cargo executable, not found in PATH" >&2
    exit 1
fi


WORKSPACE=".."
VERSION="${VERSION:-$(sed -n 's/^version = "\(.*\)".*/\1/p' "${WORKSPACE}/Cargo.toml" | head -1)}"

DOCKPATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DEVIMAGE="${DEVIMAGE:-thumbrella-server:dev}"
IMAGE="${IMAGE:-thumbrella/server}"


# Compile and update release executable
#

BINARY="${BINARY:-${WORKSPACE}/target/release/thumbrella}"
(cd "${WORKSPACE}" && cargo build --release --package tier3)
cp "${BINARY}" "${DOCKPATH}/thumbrella"


# Create image
#

${DOCKER} build --format docker --pull --build-arg "VERSION=${VERSION}" -t "${DEVIMAGE}" "${DOCKPATH}"


# Push versioned image
#

if [ "${1:-}" = "--push" ]; then
    ${DOCKER} tag "${DEVIMAGE}" "${IMAGE}:${VERSION}"
    ${DOCKER} push "${IMAGE}:${VERSION}"
fi


# Push 'latest' tagged image
if [ "${1:-}" = "--local" ]; then
    ${DOCKER} tag "${IMAGE}:${VERSION}" "${IMAGE}:latest"
    ${DOCKER} push "${IMAGE}:latest"
fi

