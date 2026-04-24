# Base stage - shared between dev and prod
FROM python:3.12-slim AS base

RUN apt-get update && apt-get install -y --no-install-recommends \
    libmagic1 \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# Dev stage - adds tooling
FROM base AS dev

RUN apt-get update && apt-get install -y --no-install-recommends \
    git exifprobe exif curl ca-certificates \
    build-essential pkg-config cmake nasm clang \
    libavutil-dev libavcodec-dev libavformat-dev libavfilter-dev \
    libavdevice-dev libswscale-dev libswresample-dev \
    && rm -rf /var/lib/apt/lists/*

# Install a pinned Rust toolchain for reproducible dev containers.
ARG RUST_TOOLCHAIN=1.95.0
ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV PATH=/usr/local/cargo/bin:${PATH}

RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal --default-toolchain ${RUST_TOOLCHAIN} \
    && rustup component add rustfmt clippy \
    && cargo --version \
    && rustc --version

WORKDIR /workspace

# Prod stage - copies source, minimal
FROM base AS prod

COPY src/ /app/src/
WORKDIR /app
CMD ["python", "-V"]
