#!/usr/bin/env bash
# Regenerate the winget manifests for a thumbrella release tag.
#
# Usage:
#   release/winget/update-manifest.sh v1.4.0
#
# Downloads the attested Windows archive from the GitHub release, optionally
# verifies build provenance, computes the installer SHA-256, and writes the
# three winget manifest files (version, defaultLocale, installer) under
# manifests/<letter>/<publisher>/<app>/<version>/ so the tree can be copied
# straight into a microsoft/winget-pkgs fork.
set -euo pipefail

# ---- configuration ----------------------------------------------------------
REPO="${REPO:-thumbrella-dev/thumbrella}"
PACKAGE_IDENTIFIER="${PACKAGE_IDENTIFIER:-Thumbrella.Server}"
PACKAGE_NAME="${PACKAGE_NAME:-Thumbrella Server}"
PUBLISHER="${PUBLISHER:-Thumbrella}"
SCHEMA_VERSION="${SCHEMA_VERSION:-1.12.0}"
VERIFY_ATTESTATION="${VERIFY_ATTESTATION:-1}"

TAG="${1:?usage: update-manifest.sh <tag> (e.g. v1.4.0)}"
VERSION="${TAG#v}"

# Folder layout mirrors the winget-pkgs repository:
#   manifests / <first letter of publisher> / <publisher> / <app> / <version>
PUBLISHER_PART="${PACKAGE_IDENTIFIER%%.*}"
APP_PART="${PACKAGE_IDENTIFIER##*.}"
LETTER="$(printf '%s' "${PUBLISHER_PART}" | tr '[:upper:]' '[:lower:]' | cut -c1)"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

ARCHIVE="thumbrella-${TAG}-windows-x86_64.zip"
URL="https://github.com/${REPO}/releases/download/${TAG}/${ARCHIVE}"
OUT_DIR="manifests/${LETTER}/${PUBLISHER_PART}/${APP_PART}/${VERSION}"

need() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need python3
need curl

mkdir -p "$OUT_DIR"

# ---- download + optional attestation verification ---------------------------
printf '==> downloading %s\n' "$URL"
curl -fsSL -o "$ARCHIVE" "$URL"

if [[ "$VERIFY_ATTESTATION" == "1" ]]; then
  if command -v gh >/dev/null 2>&1; then
    printf '==> verifying build provenance (gh attestation verify)\n'
    gh attestation verify "$ARCHIVE" --repo "$REPO"
  else
    printf 'warning: gh not found; skipping attestation verification\n' >&2
  fi
fi

# ---- installer hash ---------------------------------------------------------
INSTALLER_SHA256="$(python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest().upper())' "$ARCHIVE")"

# ---- release date (optional) ------------------------------------------------
RELEASE_DATE="$(curl -fsS "https://api.github.com/repos/${REPO}/releases/tags/${TAG}" \
  | python3 -c 'import sys,json; print((json.load(sys.stdin) or {}).get("published_at", "")[:10])' 2>/dev/null || true)"

# ---- write version manifest --------------------------------------------------
cat > "${OUT_DIR}/${PACKAGE_IDENTIFIER}.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.${SCHEMA_VERSION}.schema.json
PackageIdentifier: ${PACKAGE_IDENTIFIER}
PackageVersion: ${VERSION}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: ${SCHEMA_VERSION}
EOF

# ---- write default locale manifest ------------------------------------------
cat > "${OUT_DIR}/${PACKAGE_IDENTIFIER}.locale.en-US.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.${SCHEMA_VERSION}.schema.json
PackageIdentifier: ${PACKAGE_IDENTIFIER}
PackageVersion: ${VERSION}
PackageLocale: en-US
Publisher: ${PUBLISHER}
PublisherUrl: https://thumbrella.dev
PublisherSupportUrl: https://github.com/${REPO}/issues
PackageName: ${PACKAGE_NAME}
PackageUrl: https://thumbrella.dev
License: Apache-2.0
LicenseUrl: https://github.com/${REPO}/blob/${TAG}/LICENSE
ShortDescription: Fast thumbnail server for online media
Description: Thumbrella is a fast thumbnail server for online media, supporting 100+ formats including photos, video, documents, and 3D models through a simple HTTP API.
Moniker: thumbrella
Tags:
  - thumbnail
  - media
  - server
  - images
  - video
  - http
ReleaseNotesUrl: https://github.com/${REPO}/releases/tag/${TAG}
${RELEASE_DATE:+ReleaseDate: ${RELEASE_DATE}}
ManifestType: defaultLocale
ManifestVersion: ${SCHEMA_VERSION}
EOF

# ---- write installer manifest ------------------------------------------------
cat > "${OUT_DIR}/${PACKAGE_IDENTIFIER}.installer.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.${SCHEMA_VERSION}.schema.json
PackageIdentifier: ${PACKAGE_IDENTIFIER}
PackageVersion: ${VERSION}
InstallerLocale: en-US
Platform:
  - Windows.Desktop
MinimumOSVersion: 10.0.0.0
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
  - RelativeFilePath: thumbrella.exe
    PortableCommandAlias: thumbrella
InstallModes:
  - silent
Commands:
  - thumbrella
Installers:
  - Architecture: x64
    InstallerUrl: ${URL}
    InstallerSha256: ${INSTALLER_SHA256}
ManifestType: installer
ManifestVersion: ${SCHEMA_VERSION}
EOF

rm -f "$ARCHIVE"

printf '==> wrote manifests to %s\n' "$OUT_DIR"
printf '    next: winget validate %s  (run on Windows)\n' "$OUT_DIR"
