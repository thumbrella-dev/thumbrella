#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
dockerhub_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
release_repo="${RELEASE_REPO:-${repo_root}}"
image="${IMAGE:-thumbrella/server}"
tag=""
push=0
version=""
channel_tag="${CHANNEL_TAG:-prerelease}"
publish_channel=1
platform="linux-x86_64"
workdir="$(mktemp -d "${TMPDIR:-/tmp}/thumbrella-dockerhub.XXXXXX")"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

check_dockerhub_login() {
  if [[ "$push" -eq 1 || "${PUSH:-0}" == "1" ]]; then
    if ! podman login --get-login docker.io >/dev/null 2>&1; then
      die "not logged in to Docker Hub; run 'podman login docker.io' before using --push"
    fi
  fi
}

cleanup() {
  rm -rf "$workdir"
}
trap cleanup EXIT

while [[ $# -gt 0 ]]; do
  case "$1" in
    --push)
      push=1
      shift
      ;;
    --tag)
      [[ $# -ge 2 ]] || die "--tag requires a value"
      tag="$2"
      shift 2
      ;;
    --image)
      [[ $# -ge 2 ]] || die "--image requires a value"
      image="$2"
      shift 2
      ;;
    --version)
      [[ $# -ge 2 ]] || die "--version requires a value"
      version="$2"
      shift 2
      ;;
    --channel-tag)
      [[ $# -ge 2 ]] || die "--channel-tag requires a value"
      channel_tag="$2"
      shift 2
      check_dockerhub_login
      ;;
    --no-channel-tag)
      publish_channel=0
      shift
      ;;
    -h|--help)
      cat <<'EOF'
Usage: build.sh [TAG] [--push] [--image thumbrella/server] [--version x.y.z] [--channel-tag prerelease] [--no-channel-tag]

Builds a Docker Hub image from a GitHub release archive.
EOF
      exit 0
      ;;
    *)
      if [[ -z "$tag" ]]; then
        tag="$1"
        shift
      else
        die "unknown argument: $1"
      fi
      ;;
  esac
done

if [[ -z "$tag" ]]; then
  tag="$(git -C "$release_repo" describe --tags --exact-match 2>/dev/null || true)"
fi
[[ -n "$tag" ]] || die "pass a release tag (for example v0.5.1)"

if [[ -z "$version" ]]; then
  version="${tag#v}"
fi

archive_name="thumbrella-${tag}-${platform}.tar.gz"
archive_path="$workdir/$archive_name"
stage_dir="$workdir/stage"
mkdir -p "$stage_dir"

if [[ -n "${ARCHIVE_PATH:-}" ]]; then
  cp "$ARCHIVE_PATH" "$archive_path"
elif [[ -n "${ARCHIVE_URL:-}" ]]; then
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$archive_path" "$ARCHIVE_URL"
  else
    die "curl is required to download ARCHIVE_URL"
  fi
else
  if command -v gh >/dev/null 2>&1; then
    gh release download "$tag" --repo "${GITHUB_REPOSITORY:-thumbrella-dev/thumbrella}" --pattern "$archive_name" --dir "$workdir"
  else
    die "set ARCHIVE_PATH or ARCHIVE_URL, or install gh to fetch the release asset"
  fi
fi

tar -xzf "$archive_path" -C "$stage_dir"

[[ -f "$stage_dir/thumbrella" ]] || die "release archive did not contain thumbrella binary"
[[ -f "$stage_dir/README.md" ]] || die "release archive did not contain README.md"
[[ -f "$stage_dir/LICENSE" ]] || die "release archive did not contain LICENSE"

build_args=(
  podman build
  --format docker
  --pull
  --build-arg "VERSION=$version"
  -t "$image:$version"
)
if [[ "$publish_channel" -eq 1 ]]; then
  build_args+=(-t "$image:$channel_tag")
fi
build_args+=(-f "$dockerhub_dir/Dockerfile" "$stage_dir")

"${build_args[@]}"

if [[ "$push" -eq 1 || "${PUSH:-0}" == "1" ]]; then
  podman push "$image:$version"
  if [[ "$publish_channel" -eq 1 ]]; then
    podman push "$image:$channel_tag"
  fi
fi

printf 'Built image: %s:%s\n' "$image" "$version"