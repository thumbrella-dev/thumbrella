#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../../.." && pwd)"
NPMJS_DIR="$ROOT_DIR/release/npmjs"
REPO="${GITHUB_REPOSITORY:-thumbrella-dev/thumbrella}"
TAG=""
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/thumbrella-npm-release.XXXXXX")"
KEEP_WORKDIR=0

LINUX_ARCHIVE_PATH="${LINUX_ARCHIVE_PATH:-}"
WINDOWS_ARCHIVE_PATH="${WINDOWS_ARCHIVE_PATH:-}"
LINUX_ARCHIVE_URL="${LINUX_ARCHIVE_URL:-}"
WINDOWS_ARCHIVE_URL="${WINDOWS_ARCHIVE_URL:-}"

DEST_LINUX="$NPMJS_DIR/packages/server-linux-x64-gnu/bin/thumbrella"
DEST_WINDOWS="$NPMJS_DIR/packages/server-win32-x64-msvc/bin/thumbrella.exe"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  if [[ "$KEEP_WORKDIR" -eq 0 ]]; then
    rm -rf "$WORKDIR"
  else
    echo "kept workdir: $WORKDIR"
  fi
}
trap cleanup EXIT

usage() {
  cat <<'EOF'
Usage: stage-from-release.sh --tag vX.Y.Z [options]

Stages npm package binaries directly from GitHub release assets.

Options:
  --tag TAG              Release tag (for example: v1.0.0)
  --repo OWNER/REPO      GitHub repo (default: thumbrella-dev/thumbrella)
  --keep-workdir         Keep temporary extraction directory
  -h, --help             Show this help

Override inputs via environment variables:
  LINUX_ARCHIVE_PATH     Local path to linux archive (.tar.gz)
  WINDOWS_ARCHIVE_PATH   Local path to windows archive (.zip)
  LINUX_ARCHIVE_URL      Download URL for linux archive
  WINDOWS_ARCHIVE_URL    Download URL for windows archive

Default asset names from tag:
  thumbrella-<tag>-linux-x86_64.tar.gz
  thumbrella-<tag>-windows-x86_64.zip
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      [[ $# -ge 2 ]] || die "--tag requires a value"
      TAG="$2"
      shift 2
      ;;
    --repo)
      [[ $# -ge 2 ]] || die "--repo requires a value"
      REPO="$2"
      shift 2
      ;;
    --keep-workdir)
      KEEP_WORKDIR=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

if [[ -z "$TAG" ]]; then
  TAG="$(git -C "$ROOT_DIR" describe --tags --exact-match 2>/dev/null || true)"
fi

[[ -n "$TAG" ]] || die "pass --tag (for example: --tag v1.0.0)"

LINUX_ARCHIVE_NAME="thumbrella-${TAG}-linux-x86_64.tar.gz"
WINDOWS_ARCHIVE_NAME="thumbrella-${TAG}-windows-x86_64.zip"

LINUX_ARCHIVE="$WORKDIR/$LINUX_ARCHIVE_NAME"
WINDOWS_ARCHIVE="$WORKDIR/$WINDOWS_ARCHIVE_NAME"

fetch_asset() {
  local name="$1"
  local archive_path="$2"
  local from_path="$3"
  local from_url="$4"

  if [[ -n "$from_path" ]]; then
    cp "$from_path" "$archive_path"
    return
  fi

  if [[ -n "$from_url" ]]; then
    command -v curl >/dev/null 2>&1 || die "curl is required for *_ARCHIVE_URL"
    curl -fL --retry 3 -o "$archive_path" "$from_url"
    return
  fi

  command -v gh >/dev/null 2>&1 || die "install gh or set *_ARCHIVE_PATH/*_ARCHIVE_URL"
  gh release download "$TAG" --repo "$REPO" --pattern "$name" --dir "$WORKDIR"
}

echo "staging binaries from GitHub release assets"
echo "repo: $REPO"
echo "tag:  $TAG"

fetch_asset "$LINUX_ARCHIVE_NAME" "$LINUX_ARCHIVE" "$LINUX_ARCHIVE_PATH" "$LINUX_ARCHIVE_URL"
fetch_asset "$WINDOWS_ARCHIVE_NAME" "$WINDOWS_ARCHIVE" "$WINDOWS_ARCHIVE_PATH" "$WINDOWS_ARCHIVE_URL"

LINUX_EXTRACT_DIR="$WORKDIR/linux"
WINDOWS_EXTRACT_DIR="$WORKDIR/windows"
mkdir -p "$LINUX_EXTRACT_DIR" "$WINDOWS_EXTRACT_DIR"

tar -xzf "$LINUX_ARCHIVE" -C "$LINUX_EXTRACT_DIR"
[[ -f "$LINUX_EXTRACT_DIR/thumbrella" ]] || die "linux archive missing thumbrella binary"

python3 - <<'PY' "$WINDOWS_ARCHIVE" "$WINDOWS_EXTRACT_DIR"
import pathlib
import sys
import zipfile

archive = pathlib.Path(sys.argv[1])
out_dir = pathlib.Path(sys.argv[2])
with zipfile.ZipFile(archive, "r") as zf:
    zf.extractall(out_dir)
PY

[[ -f "$WINDOWS_EXTRACT_DIR/thumbrella.exe" ]] || die "windows archive missing thumbrella.exe binary"

cp "$LINUX_EXTRACT_DIR/thumbrella" "$DEST_LINUX"
chmod +x "$DEST_LINUX"
cp "$WINDOWS_EXTRACT_DIR/thumbrella.exe" "$DEST_WINDOWS"

echo "staged linux binary:   $DEST_LINUX"
echo "staged windows binary: $DEST_WINDOWS"
