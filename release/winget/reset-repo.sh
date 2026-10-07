#!/usr/bin/env bash
# Reset the existing sparse winget clone and fork master to upstream master.
# Usage: release/winget/reset-repo.sh [checkout-directory]
# Discards fork-only commits on master without creating a backup.
set -euo pipefail

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

[[ $# -le 1 ]] || die "usage: $0 [checkout-directory]"
command -v git >/dev/null 2>&1 || die "missing required command: git"

REPO_DIR="${1:-/workspaces/winget-pkgs-tbr}"
UPSTREAM_URL="https://github.com/microsoft/winget-pkgs.git"

[[ -d "$REPO_DIR" ]] || die "checkout not found: $REPO_DIR; create the sparse clone first"
cd "$REPO_DIR"
[[ "$(git rev-parse --show-toplevel)" == "$(pwd -P)" ]] \
  || die "checkout-directory must be the repository root"

case "$(git config --get remote.origin.url)" in
  https://github.com/PeterShinners/winget-pkgs-tbr|\
  https://github.com/PeterShinners/winget-pkgs-tbr.git|\
  git@github.com:PeterShinners/winget-pkgs-tbr.git) ;;
  *) die "origin must point to PeterShinners/winget-pkgs-tbr" ;;
esac

[[ "$(git config --get core.sparseCheckout)" == true ]] \
  || die "sparse checkout must be enabled"
[[ "$(git config --get core.sparseCheckoutCone)" == true ]] \
  || die "cone-mode sparse checkout must be enabled"
[[ "$(git config --get remote.origin.partialclonefilter)" == blob:none ]] \
  || die "origin must use the blob:none partial-clone filter"
[[ -z "$(git status --porcelain)" ]] \
  || die "working tree is not clean; commit or move your changes first"

printf '==> resetting local and fork master to upstream, without a backup\n'
git switch master

# The explicit lease protects changes pushed to the fork during this reset.
git fetch --depth=1 --filter=blob:none --no-tags origin master
OLD_FORK_HEAD="$(git rev-parse refs/remotes/origin/master)"
git fetch --depth=1 --filter=blob:none --no-tags "$UPSTREAM_URL" master
UPSTREAM_HEAD="$(git rev-parse FETCH_HEAD)"

git reset --hard "$UPSTREAM_HEAD"
git sparse-checkout set manifests/t/Thumbrella
git push \
  --force-with-lease="refs/heads/master:$OLD_FORK_HEAD" \
  origin HEAD:refs/heads/master

[[ "$(git rev-parse HEAD)" == "$UPSTREAM_HEAD" ]] \
  || die "local master does not match the fetched upstream commit"
[[ -z "$(git status --porcelain)" ]] \
  || die "working tree is not clean after reset"

printf '==> reset master to %s\n' "$UPSTREAM_HEAD"
git status --short --branch
printf '==> sparse checkout:\n'
git sparse-checkout list
