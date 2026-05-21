#!/usr/bin/env bash

PORT=${PORT:-8000}
SERVE=${SERVE:-/mnt/windows-thumbrella-repo/media}
echo "Serving '${SERVE}' on http://localhost:${PORT}"
exec webfsd -F -d -p "$PORT" -r "$SERVE"
