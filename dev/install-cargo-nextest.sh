#!/usr/bin/env bash
# Downloads prebuilt cargo-nextest through the GitHub API asset endpoint.
# This endpoint stays available when get.nexte.st or release redirect URLs fail.
set -euo pipefail

VERSION="0.9.150"
REPO="nextest-rs/nextest"
TAG="cargo-nextest-${VERSION}"
ASSET="cargo-nextest-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"

DEST="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$DEST"
export PATH="$DEST:$PATH"

echo "Downloading ${ASSET} via GitHub release API..."
gh release download "$TAG" \
  --repo "$REPO" \
  --pattern "$ASSET" \
  --output - | tar zxf - -C "$DEST"

cargo nextest --version
