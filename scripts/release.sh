#!/usr/bin/env bash
set -euo pipefail

linux_repo="${LINUX_REPO:-/workspaces/thumbrella}"
windows_repo="${WINDOWS_REPO:-/mnt/host/thumbrella}"
release_readme="${RELEASE_README:-release/README.release.md}"
open_browser=0
tag_arg=""

usage() {
  cat <<'EOF'
Usage: scripts/release.sh [--tag v0.5.1] [--linux-repo PATH] [--windows-repo PATH] [--open]

Creates release archives from tagged, clean Linux and Windows git trees, then
creates or updates a draft GitHub release and uploads the archives.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      [[ $# -ge 2 ]] || die "--tag requires a value"
      tag_arg="$2"
      shift 2
      ;;
    --linux-repo)
      [[ $# -ge 2 ]] || die "--linux-repo requires a value"
      linux_repo="$2"
      shift 2
      ;;
    --windows-repo)
      [[ $# -ge 2 ]] || die "--windows-repo requires a value"
      windows_repo="$2"
      shift 2
      ;;
    --open)
      open_browser=1
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

need git
need gh
need tar
need python3

check_clean_tagged_tree() {
  local repo="$1"
  local expected_tag="$2"
  local binary_path="$3"
  local label="$4"

  [[ -d "$repo/.git" ]] || die "$label repo not found: $repo"
  [[ -f "$repo/$binary_path" ]] || die "$label release binary missing: $repo/$binary_path"

  local status
  status="$(git -C "$repo" status --porcelain --untracked-files=no)"
  if [[ -n "$status" ]]; then
    if git -C "$repo" diff --ignore-cr-at-eol --quiet --ignore-submodules --; then
      printf '%s repo only differs by line endings; continuing.\n' "$label" >&2
    else
      printf '%s repo is dirty; continuing with prerelease packaging:\n%s\n' "$label" "$status" >&2
    fi
  fi

  local head_tag
  head_tag="$(git -C "$repo" describe --tags --exact-match 2>/dev/null || true)"
  [[ -n "$head_tag" ]] || die "$label HEAD is not tagged"
  [[ "$head_tag" == "$expected_tag" ]] || die "$label HEAD tag ($head_tag) does not match expected tag ($expected_tag)"

  local tag_commit_ts binary_mtime
  tag_commit_ts="$(git -C "$repo" show -s --format=%ct "$expected_tag")"
  binary_mtime="$(stat -c %Y "$repo/$binary_path")"
  [[ "$binary_mtime" -ge "$tag_commit_ts" ]] || die "$label binary is older than the tag commit"
}

copy_release_files() {
  local src_repo="$1"
  local src_binary="$2"
  local dst_dir="$3"
  local dst_binary_name="$4"

  cp "$src_repo/$src_binary" "$dst_dir/$dst_binary_name"
  if [[ -f "$src_repo/$release_readme" ]]; then
    cp "$src_repo/$release_readme" "$dst_dir/README.md"
  else
    cp "$src_repo/README.md" "$dst_dir/README.md"
  fi
  cp "$src_repo/LICENSE" "$dst_dir/LICENSE"
}

make_zip() {
  local src_dir="$1"
  local out_file="$2"
  local zip_binary="$3"

  if command -v zip >/dev/null 2>&1; then
    (cd "$src_dir" && zip -9 -r "$out_file" . >/dev/null)
    return
  fi

  python3 - "$src_dir" "$out_file" "$zip_binary" <<'PY'
import pathlib
import sys
import zipfile

src_dir = pathlib.Path(sys.argv[1])
out_file = pathlib.Path(sys.argv[2])
zip_name = sys.argv[3]

with zipfile.ZipFile(out_file, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
    for rel in [zip_name, "README.md", "LICENSE"]:
        zf.write(src_dir / rel, arcname=rel)
PY
}

tag="${tag_arg:-}"
if [[ -z "$tag" ]]; then
  tag="$(git -C "$linux_repo" describe --tags --exact-match 2>/dev/null || true)"
fi
[[ -n "$tag" ]] || die "could not determine release tag; pass --tag"

linux_binary="target/release/thumbrella"
windows_binary="target/release/thumbrella.exe"

check_clean_tagged_tree "$linux_repo" "$tag" "$linux_binary" "Linux"
check_clean_tagged_tree "$windows_repo" "$tag" "$windows_binary" "Windows"

workdir="$(mktemp -d "${TMPDIR:-/tmp}/thumbrella-release.${tag}.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

mkdir -p "$workdir/linux" "$workdir/windows" "$workdir/out"
copy_release_files "$linux_repo" "$linux_binary" "$workdir/linux" "thumbrella"
copy_release_files "$windows_repo" "$windows_binary" "$workdir/windows" "thumbrella.exe"

linux_archive="$workdir/out/thumbrella-${tag}-linux-x86_64.tar.gz"
windows_archive="$workdir/out/thumbrella-${tag}-windows-x86_64.zip"

tar -C "$workdir/linux" -czf "$linux_archive" thumbrella README.md LICENSE
make_zip "$workdir/windows" "$windows_archive" "thumbrella.exe"

notes_file="$workdir/notes.txt"
git -C "$linux_repo" show -s --format=%B "$tag" > "$notes_file"

if gh release view "$tag" >/dev/null 2>&1; then
  gh release edit "$tag" --draft --title "$tag" --notes-file "$notes_file"
else
  gh release create "$tag" --draft --title "$tag" --notes-file "$notes_file"
fi

gh release upload "$tag" "$linux_archive" "$windows_archive" --clobber

release_url="$(gh release view "$tag" --json url --jq .url)"
printf 'Release draft ready: %s\n' "$release_url"

if [[ "$open_browser" -eq 1 ]]; then
  if [[ -n "${BROWSER:-}" ]]; then
    "$BROWSER" "$release_url" >/dev/null 2>&1 || true
  else
    printf 'Set BROWSER to open the draft release automatically.\n' >&2
  fi
fi