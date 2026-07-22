# Self-hosted thumbnail service. Ships tier3 on top of a production ffmpeg
# image for broad format coverage.
#
# Build:  ./build.sh
# Run:    podman run --rm -p 3114:3114 local/thumbrella-server:dev
#
# For persistent caching, use the included docker-compose.yml.

FROM lscr.io/linuxserver/ffmpeg:latest

ARG VERSION=dev
LABEL org.opencontainers.image.title="Thumbrella"
LABEL org.opencontainers.image.description="Self-hosted online thumbnailer"
LABEL org.opencontainers.image.url="https://thumbrella.dev"
LABEL org.opencontainers.image.source="https://github.com/thumbrella-dev/thumbrella"
LABEL org.opencontainers.image.version="${VERSION}"
LABEL org.opencontainers.image.licenses="Apache 2"

# PS1, MOTD, release info, and bashrc setup.
# Written to /root/.bashrc because the base image's /etc/bash.bashrc gets
# overridden by its own skel files.
RUN echo 'export PS1="\[\033[1;35m\]thumbrella\[\033[0m\] \w $ "' > /root/.bashrc && \
    echo 'cat /etc/motd 2>/dev/null' >> /root/.bashrc && \
    printf 'docker thumbrella-server:%s (tier3)\n' "${VERSION}" > /etc/thumbrella-release && \
    printf '\n\
\033[1;35m  Thumbrella %s\033[0m\n\
\033[0;90m  github.com/thumbrella-dev/thumbrella\033[0m\n\
\033[0;90m  https://thumbrella.dev/docs/\033[0m\n\
\n\
\033[0;90m  Quickstart:\033[0m\n\
\033[0;90m    thumbrella serve            start the server (port 3114)\033[0m\n\
\033[0;90m    thumbrella check            show capability report\033[0m\n\
\033[0;90m    thumbrella formats          list supported formats\033[0m\n\
\n' "${VERSION}" > /etc/motd

HEALTHCHECK --interval=30s --timeout=5s --start-period=2s --retries=2 \
    CMD curl -f "http://localhost:${TBR_PORT:-3114}/health"

EXPOSE 3114

# Clear the base image's ffmpeg entrypoint so CMD runs our binary.
ENTRYPOINT []
CMD ["thumbrella", "serve"]

COPY --chmod=755 thumbrella /usr/local/bin/thumbrella
