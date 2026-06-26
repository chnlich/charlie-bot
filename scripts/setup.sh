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

# Seed the host config without overwriting local secrets.
echo "==> Seeding config"
CONFIG_DIR="$HOME/.charliebot"
CONFIG_FILE="$CONFIG_DIR/config.yaml"
CONFIG_TEMPLATE="$REPO_ROOT/configs/config.example.yaml"
if [[ -e "$CONFIG_FILE" ]]; then
  echo "  Exists, not overwriting: $CONFIG_FILE"
else
  if (( DRY_RUN )); then
    echo "  [dry-run] mkdir -p $CONFIG_DIR"
    echo "  [dry-run] cp $CONFIG_TEMPLATE $CONFIG_FILE"
  else
    mkdir -p "$CONFIG_DIR"
    cp "$CONFIG_TEMPLATE" "$CONFIG_FILE"
    echo "  Seeded $CONFIG_FILE"
  fi
fi
echo "  Reminder: fill in secret keys gemini_api_key and charliebot_access_key before first start."

# Smoke-check the Claude Code backend command for headless-unsafe tools.
echo "==> Checking Claude Code backend tools"
uv run python - <<'PY'
from src.agents.backends.claude_code import BASE_COMMAND

required = ["Monitor", "ScheduleWakeup", "CronCreate", "CronDelete", "CronList"]

try:
  disallowed_index = BASE_COMMAND.index("--disallowed-tools")
except ValueError as exc:
  raise SystemExit("missing --disallowed-tools in BASE_COMMAND") from exc

try:
  disallowed_tools = set(BASE_COMMAND[disallowed_index + 1].split(","))
except IndexError as exc:
  raise SystemExit("--disallowed-tools has no value in BASE_COMMAND") from exc

missing = set(required) - disallowed_tools
if missing:
  raise SystemExit(f"missing disallowed tools: {','.join(sorted(missing))}")

print("OK: backend disallows " + ",".join(required))
PY

echo "Setup complete."
