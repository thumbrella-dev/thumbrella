# Base stage - shared between dev and prod
FROM python:3.12-slim AS base

RUN apt-get update && apt-get install -y --no-install-recommends \
    libmagic1 \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/*

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

# Dev stage - adds tooling
FROM base AS dev

RUN apt-get update && apt-get install -y --no-install-recommends \
    git exifprobe exif\
    && rm -rf /var/lib/apt/lists/*

RUN pip install --no-cache-dir debugpy pytest ipython ruff

WORKDIR /workspace

# Prod stage - copies source, minimal
FROM base AS prod

COPY src/ /app/src/
WORKDIR /app
CMD ["python", "-m", "thumbrella.server"]
