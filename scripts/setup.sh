#!/usr/bin/env bash
set -euo pipefail

# Resolve paths relative to this script so setup works from any current directory.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

# The scripts and the server run one release: the charliebot CLI that
# $CHARLIEBOT_RELEASES_DIR/current selects. A missing release stops here, before
# setup changes anything.
RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
CURRENT="$RELEASES_DIR/current"
if [ ! -e "$CURRENT" ] && [ ! -L "$CURRENT" ]; then
  echo "setup: no release at $CURRENT; install one with scripts/release.sh <sha>" >&2
  exit 1
fi

DRY_RUN=0

usage() {
  echo "Usage: $0 [-n|--dry-run]"
}

# Parse setup flags. Dry-run keeps the flow read-only and skips skill writes.
while [[ $# -gt 0 ]]; do
  case "$1" in
    -n|--dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 1
      ;;
  esac
done

cd "$REPO_ROOT"

# Sync shared and host-specific skills into backend skill directories.
echo "==> Syncing skills"
if (( DRY_RUN )); then
  "$SCRIPT_DIR/sync-skills.sh" -n
else
  "$SCRIPT_DIR/sync-skills.sh"
fi

# Install the systemd user units and start the agents' slice. The unit files are the single
# home of the memory limits: the launcher's preflight reads its expected values from them.
echo "==> Installing systemd user units"
UNIT_DIR="$HOME/.config/systemd/user"
if (( DRY_RUN )); then
  echo "install -d $UNIT_DIR"
  echo "install -m 0644 $SCRIPT_DIR/systemd/*.slice $UNIT_DIR/"
  echo "systemctl --user daemon-reload"
  echo "systemctl --user enable --now charliebot-agents.slice"
else
  install -d "$UNIT_DIR"
  install -m 0644 "$SCRIPT_DIR/systemd/"*.slice "$UNIT_DIR/"
  systemctl --user daemon-reload
  systemctl --user enable --now charliebot-agents.slice
fi

# Retire the repair slice unit file an earlier setup installed: `disable` plus removing the file ends
# its use without `stop`, because stopping a slice ends the processes still running inside it. The
# retired name is matched by pattern so the tree holds no reference to it.
for stale in "$UNIT_DIR"/charliebot-[r]epair.slice; do
  [ -f "$stale" ] || continue
  if (( DRY_RUN )); then
    echo "systemctl --user disable $(basename "$stale")"
    echo "rm -f $stale"
    echo "systemctl --user daemon-reload"
  else
    systemctl --user disable "$(basename "$stale")"
    rm -f "$stale"
    systemctl --user daemon-reload
  fi
done

# Run the setup steps through the release CLI: the $CHARLIEBOT_HOME home layout, then each registered
# setup step (the cron seed, then the Claude Code check), in registration order. The release reads the
# checkout it seeds from through this variable, the way the server launcher exports it.
export CHARLIEBOT_REPO_ROOT="$REPO_ROOT"
if (( DRY_RUN )); then
  "$CURRENT/charliebot" setup --dry-run
else
  "$CURRENT/charliebot" setup
fi

echo "Setup complete."
