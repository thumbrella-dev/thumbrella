# Release Docker image for Thumbrella.
# Build context is a staging directory produced by build.sh.

FROM lscr.io/linuxserver/ffmpeg:latest

ARG VERSION=dev
ARG REVISION=unknown
ARG BUILD_DATE=unknown

# Labels: this image derives from lscr.io/linuxserver/ffmpeg, whose labels are
# inherited and CANNOT be removed in a child Dockerfile, only overridden. We
# override the ones that should describe Thumbrella and deliberately leave
# build_version untouched so the upstream linuxserver version/build date is
# still visible. base.name records the upstream this image is built on.
LABEL org.opencontainers.image.title="Thumbrella" \
      org.opencontainers.image.description="Self-hosted online thumbnailer" \
      org.opencontainers.image.url="https://thumbrella.dev" \
      org.opencontainers.image.documentation="https://thumbrella.dev/docs/" \
      org.opencontainers.image.source="https://github.com/thumbrella-dev/thumbrella" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.ref.name="${REVISION}" \
      org.opencontainers.image.created="${BUILD_DATE}" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.authors="Thumbrella" \
      org.opencontainers.image.vendor="Thumbrella" \
      org.opencontainers.image.base.name="lscr.io/linuxserver/ffmpeg:latest" \
      maintainer="Thumbrella <pete@thumbrella.dev>"

# Optional external renderers used by tier3. Install only the two CLI packages
# and their hard dependencies. --no-install-recommends keeps out the heavy
# optional pull-ins (ghostscript, dbus, cups, avahi, X libs, fonts, poppler-data)
# that nearly double the package count. Then drop the apt indexes and cached
# .debs so this layer stays small and does not pollute the image.
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y \
        --no-install-recommends --no-install-suggests \
        graphicsmagick \
        poppler-utils && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /root

RUN echo 'export PS1="\[\033[1;35m\]thumbrella\[\033[0m\] \w $ "' > /root/.bashrc && \
    echo 'cat /etc/motd 2>/dev/null' >> /root/.bashrc && \
    printf 'docker thumbrella/server:%s (release)\n' "${VERSION}" > /etc/thumbrella-release && \
    printf '\n\
\033[1;35m  Thumbrella %s\033[0m\n\
\033[0;90m  github.com/thumbrella-dev/thumbrella\033[0m\n\
\033[0;90m  https://thumbrella.dev/docs/server/\033[0m\n\
\n\
\033[0;90m  Quickstart:\033[0m\n\
\033[0;90m    thumbrella serve           start the server (port 3114)\033[0m\n\
\033[0;90m    thumbrella check           show capability report\033[0m\n\
\033[0;90m    thumbrella formats         list supported formats\033[0m\n\
\n' "${VERSION}" > /etc/motd

HEALTHCHECK --interval=30s --timeout=5s --start-period=2s --retries=2 \
    CMD curl -f "http://localhost:${TBR_PORT:-3114}/health"

EXPOSE 3114

ENTRYPOINT []
CMD ["thumbrella", "serve"]

COPY --chmod=755 thumbrella /usr/local/bin/thumbrella
COPY README.md /root/README.md
COPY LICENSE /root/LICENSE  