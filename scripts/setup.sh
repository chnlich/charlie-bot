#!/usr/bin/env bash
set -euo pipefail

# Resolve paths relative to this script so setup works from any current directory.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

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

# Run the setup steps: the $CHARLIEBOT_HOME home layout, then each step a package registers
# (register_setup_step in src/runtime/hooks/wiring.py), in registration order.
if (( DRY_RUN )); then
  uv run python -m src.app.setup --dry-run
else
  uv run python -m src.app.setup
fi

echo "Setup complete."
