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

# Run the setup steps: the ~/.charliebot home layout, then each step a package registers
# (register_setup_step in src/runtime/hooks/wiring.py), in registration order.
if (( DRY_RUN )); then
  uv run python -m src.app.setup --dry-run
else
  uv run python -m src.app.setup
fi

echo "Setup complete."
