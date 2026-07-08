#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../../.." && pwd)"
NPMJS_DIR="$ROOT_DIR/release/npmjs"

SRC="$NPMJS_DIR/README.release.md"
DEST="$NPMJS_DIR/packages/server/README.md"

cp "$SRC" "$DEST"
echo "synced: $SRC -> $DEST"
