#!/usr/bin/env bash
# Downloads the pinned restic release binary, verifies its SHA256, and installs
# it into /usr/local/bin.
set -euo pipefail

VERSION="0.19.1"
REPO="restic/restic"
ASSET="restic_${VERSION}_linux_amd64.bz2"
SHA256="f415415624dcc452f2a02b8c33641791a8c6d6d3b65bbb3543fcf9a25151585c"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading ${ASSET} via GitHub release API..."
gh release download "v${VERSION}" \
  --repo "$REPO" \
  --pattern "$ASSET" \
  --dir "$TMP"
echo "$SHA256  $TMP/$ASSET" | sha256sum -c -
bunzip2 "$TMP/$ASSET"
sudo install -m 755 "$TMP/restic_${VERSION}_linux_amd64" /usr/local/bin/restic
restic version
