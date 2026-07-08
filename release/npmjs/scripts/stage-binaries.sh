#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../../.." && pwd)"
NPMJS_DIR="$ROOT_DIR/release/npmjs"

SRC_LINUX="$ROOT_DIR/target/release/thumbrella"
SRC_WINDOWS="$ROOT_DIR/target/x86_64-pc-windows-msvc/release/thumbrella.exe"

DEST_LINUX="$NPMJS_DIR/packages/server-linux-x64-gnu/bin/thumbrella"
DEST_WINDOWS="$NPMJS_DIR/packages/server-win32-x64-msvc/bin/thumbrella.exe"

echo "staging binaries into release/npmjs packages"

if [[ -f "$SRC_LINUX" ]]; then
  cp "$SRC_LINUX" "$DEST_LINUX"
  chmod +x "$DEST_LINUX"
  echo "staged linux binary: $DEST_LINUX"
else
  echo "warning: linux binary missing at $SRC_LINUX"
fi

if [[ -f "$SRC_WINDOWS" ]]; then
  cp "$SRC_WINDOWS" "$DEST_WINDOWS"
  echo "staged windows binary: $DEST_WINDOWS"
else
  echo "warning: windows binary missing at $SRC_WINDOWS"
fi
