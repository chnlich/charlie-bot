#!/usr/bin/env bash
# Preflight for a `type: charlie-code` backend option: asserts every mechanism a
# session on it depends on — the config entry exists, the charlie-code binary
# resolves and runs, the CLI contract matches what CharlieCodeBackend emits, and
# the endpoint serves the configured model — so one run covers the whole fault
# class before a session or delegation is started on it.
set -euo pipefail

# Resolve paths relative to this script so preflight works from any current directory.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

usage() {
  echo "Usage: $0 <backend-option-id>"
}

if [[ $# -eq 0 ]]; then
  usage >&2
  exit 2
fi

if [[ "$1" == "-h" || "$1" == "--help" ]]; then
  usage
  exit 2
fi

if [[ $# -ne 1 ]]; then
  usage >&2
  exit 2
fi

cd "$REPO_ROOT"

# The checks run through the release CLI (`charliebot preflight-charlie-code`), which the
# launcher scripts and the server select the same way: one release per host.
RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
CURRENT="$RELEASES_DIR/current"
if [ ! -e "$CURRENT" ] && [ ! -L "$CURRENT" ]; then
  echo "preflight-charlie-code: no release at $CURRENT; install one with scripts/release.sh <sha>" >&2
  exit 1
fi

exec "$CURRENT/charliebot" preflight-charlie-code "$1"
