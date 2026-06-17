# Self-hosted thumbnail service. Ships tier3 on top of a production ffmpeg
# image for broad format coverage: images, video, audio, HDR, JPEG XL, EXR, SVG,
# and more.
#
# This builds on the linnuxserver.io ffmpeg image. This provides a good subset
# of the functionality a thumbrella server can utilize while rendering
# thumbnails.
#
# Build:  ./build.sh
# Run:    podman run --rm -p 3411:3411 local/thumbrella-server:dev

FROM lscr.io/linuxserver/ffmpeg:latest


ARG VERSION=dev
LABEL org.opencontainers.image.title="Thumbrella"
LABEL org.opencontainers.image.description="Self-hosted online thumbnailer"
LABEL org.opencontainers.image.url="https://thumbrella.dev"
LABEL org.opencontainers.image.source="https://github.com/PeterShinners/thumbrella"
LABEL org.opencontainers.image.version="${VERSION}"
LABEL org.opencontainers.image.licenses="Apache 2"


COPY thumbrella /usr/local/bin/thumbrella
RUN chmod 777 /usr/local/bin/thumbrella


RUN printf '\n\
\033[1;35m  Thumbrella %s\033[0m\n\
\033[0;90m  github.com/PeterShinners/thumbrella\033[0m\n\
\033[0;90m  https://thumbrella.dev/docs/docker-server/\033[0m\n\
\n\
\033[0;90m  Quickstart:\033[0m\n\
\033[0;90m    thumbrella diag            show capability report\033[0m\n\
\033[0;90m    thumbrella serve           start HTTP server (port 3114)\033[0m\n\
\033[0;90m    thumbrella render src dst  render a single file\033[0m\n\
\n' "${VERSION}" > /etc/motd

# Also show the MOTD for bash login shells (podman exec / docker exec).
RUN echo 'cat /etc/motd 2>/dev/null' >> /etc/bash.bashrc


HEALTHCHECK --interval=30s --timeout=5s --start-period=2s --retries=2 \
CMD curl -f "http://localhost:${TBR_PORT:-3114}/health"

EXPOSE 3411
ENTRYPOINT ["thumbrella"]
CMD ["serve"]
